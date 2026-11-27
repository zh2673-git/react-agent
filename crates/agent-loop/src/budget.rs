//! 预算与取消（R12 拆分自 lib.rs）。
//!
//! 职责：`ChatBudget` / `UsageAcc`（单次 chat 的剩余预算与多轮用量累计）、预算/重试
//! 配置读取（`budget_secs` / `token_budget` / `retry_attempts` / `retry_base_ms`）、
//! 瞬态判定（`is_transient_llm_error` / `is_transient_kernel_err`）、轮次边界停车
//! 检查（`check_cancel` / `check_budget`）。

use super::*;
use std::time::Instant;

// 预算/重试参数已于 E3 收编至 `config::AgentLoopConfig`（from_env 单点解析 +
// warn-on-invalid）；本文件保留瞬态判定、预算结构与轮次边界停车检查。

/// 瞬态判定（T3）：限流/超时/网关类错误值得重试；参数/鉴权类重试无益。
/// llm-adapter 的 provider 异常统一为 code=LLM_ERROR + message="{ExcType}: {exc}"，
/// 故按 message 关键词匹配（429/rate limit/timeout/5xx/连接类）；K400 是确定性失败。
pub(super) fn is_transient_llm_error(err: &Value) -> bool {
    let code = err.get("code").and_then(Value::as_str).unwrap_or("");
    if code == "K400" {
        return false;
    }
    let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
    let hay = format!("{code} {msg}").to_ascii_lowercase();
    [
        "429", "rate limit", "ratelimit", "timeout", "timed out", "502", "503", "504", "overloaded", "connection",
        "temporarily",
    ]
    .iter()
    .any(|k| hay.contains(k))
}

/// 内核层瞬态判定（T3）：deadline 超时值得重试；panic/取消/路由失败重试无益。
pub(super) fn is_transient_kernel_err(e: &KernelError) -> bool {
    matches!(e, KernelError::DeadlineExceeded(_))
}

/// 单次 chat 的剩余预算（T4）。顶层 chat 取 env 缺省；子代理继承父链衰减后的剩余。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ChatBudget {
    /// 墙钟到点 → **软收敛**（不是中止）：让模型把已完成的部分说完再停。
    pub(super) deadline: Option<Instant>,
    /// token 上限 → **硬停止**：成本闸，不可为收敛再调一次 LLM。
    pub(super) tokens_left: Option<u64>,
}

/// 预算停车原因（T4 语义修正）。
///
/// 关键区分：`Tokens` 是**成本上限**，停下来是目的本身，不能为「把话说完」再花一次钱；
/// `Deadline` 是**时长护栏**，它的目的只是防失控长跑，而不是惩罚「任务本身耗时长」。
/// 旧实现把两者都当硬中止（直接 `return K508`），后果是本轮所有工具成果、思考、
/// 部分答案全部丢弃，用户只拿到一个「chat 预算耗尽：时长超限」的错误气泡——
/// 任务没做完，工作也没了。这是把「护栏」当成了「处罚」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopReason {
    /// token 预算耗尽（K508）：成本上限，立即收敛。
    Tokens,
    /// 墙钟到点（软）：不中止，转入「强制收敛轮」输出部分成果。
    Deadline,
}

impl StopReason {
    /// 是否必须立刻中止（true = 丢弃工作直接返回错误；false = 先收敛再说）。
    pub(super) fn is_hard(self) -> bool {
        matches!(self, StopReason::Tokens)
    }

    /// chat_summary 上报用的稳定标识（观测口径，不改即前端无感）。
    pub(super) fn tag(self) -> &'static str {
        match self {
            StopReason::Tokens => "tokens",
            StopReason::Deadline => "deadline",
        }
    }
}

/// 收敛轮指令：预算到点后仍要交出一份「已做什么 / 还差什么」的答复，
/// 而不是让已完成的步骤随中止一起蒸发。
pub(super) const WRAP_UP_DEADLINE: &str = "[policy:budget_wrap_up] 本次 chat 的墙钟预算已到点。\
不要再发起新的探索或长任务：请立刻用纯文本收敛，明确写出\n\
(1) 已经完成的部分（含已产出的文件路径、已验证的结论）；\n\
(2) 尚未完成的部分与原因；\n\
(3) 下一步最小可执行的建议。\n\
用户会据此续做，因此「如实报告进度」比「假装完成」重要。";

/// 多轮累计用量：ReAct 一轮可能多次调用 LLM，用户要的是总消耗而非单轮。
#[derive(Default)]
pub(super) struct UsageAcc {
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
    seen: bool,
    /// 累计的 provider 上报 HTTP 尝试次数（provider 内部重试已计入）。
    /// None = 全部请求都未上报（旧 provider / 未接入观测），与「0 次」区分。
    http_attempts: Option<u64>,
}

/// 计费权重：cache_read 相对 input 的单价比（env `CACHE_CREDIT_WEIGHT`，缺省 0.32）。
///
/// 实测口径（2026-11-16 `~/.config/magpie/usage.jsonl`，最小二乘拟合，误差 0.1%）：
/// `credits ≈ 1.020e-05×in + 3.260e-06×cache_read + 1.223e-04×out + 0.0267`
/// → cache_read 单价 = input 的 **32.0%**，output = input 的 **12.0 倍**。
///
/// 缺省 0.32 对齐实测；置 0 即刻退回旧口径（不计 cache_read）。
/// 非法值 warn 后回退缺省（与 E3 的「消灭静默回退」纪律一致）。
pub(super) fn cache_credit_weight() -> f64 {
    const DEFAULT: f64 = 0.32;
    match std::env::var("CACHE_CREDIT_WEIGHT") {
        Ok(raw) => {
            let v = raw.trim();
            if v.is_empty() {
                return DEFAULT;
            }
            match v.parse::<f64>() {
                Ok(n) if n >= 0.0 => n,
                Ok(_) => {
                    tracing::warn!(target: crate::ID, "CACHE_CREDIT_WEIGHT={raw:?} 非法（应为非负数），回退缺省 {DEFAULT}");
                    DEFAULT
                }
                Err(_) => {
                    tracing::warn!(target: crate::ID, "CACHE_CREDIT_WEIGHT={raw:?} 非法（应为非负数），回退缺省 {DEFAULT}");
                    DEFAULT
                }
            }
        }
        Err(_) => DEFAULT,
    }
}

impl UsageAcc {
    pub(super) fn add(&mut self, u: Option<&Value>) {
        let Some(u) = u else { return };
        self.seen = true;
        self.input += u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.output += u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.cache_read += u.get("cache_read_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.reasoning += u.get("reasoning_tokens").and_then(Value::as_u64).unwrap_or(0);
    }

    /// 累计一次请求的 HTTP 尝试次数（P5 观测）。
    ///
    /// `None` = 该次请求未上报（旧 provider / 流式旁路），**不参与累计**——这样
    /// 「未观测」既不会伪装成 0，也不会把已观测的次数冲掉。
    pub(super) fn add_http_attempts(&mut self, n: Option<u64>) {
        let Some(n) = n else { return };
        self.http_attempts = Some(self.http_attempts.unwrap_or(0) + n);
    }

    /// 指标外抛用：累计 HTTP 尝试次数（None = 全部未上报 → 线契约落 null）。
    ///
    /// 刻意不进 `to_value()`：后者同时喂 `assistant.usage` 与 `chat_summary.tokens`，
    /// 是既有冻结的四字段用量契约；观测指标只在 metrics / chat_summary 顶层单列。
    pub(super) fn http_attempts_total(&self) -> Option<u64> {
        self.http_attempts
    }

    /// 计费口径（T4 预算）：input+output+cache_read×权重。
    ///
    /// 为什么必须计入 cache_read：实测 provider 对 cache_read 的计费单价是 input 的
    /// **32%**，而当日消耗的 **77.1%** 来自 cache_read——只算 input+output 等于
    /// 只看见 1/9.4 的真实账单，token 预算闸在真实消耗面前完全失真。
    ///
    /// 权重可配（env `CACHE_CREDIT_WEIGHT`，缺省 0.32 = 实测单价比）：
    /// 置 0 即刻退回旧口径（provider 若改变 cache 定价而不影响别的语义时用）。
    pub(super) fn total(&self) -> u64 {
        let weight = cache_credit_weight();
        self.input + self.output + (self.cache_read as f64 * weight) as u64
    }

    /// provider 未上报用量时返回 Null（前端据此不显示统计条，而非显示 0）。
    pub(super) fn to_value(&self) -> Value {
        if !self.seen {
            return Value::Null;
        }
        json!({
            "input_tokens": self.input,
            "output_tokens": self.output,
            "cache_read_tokens": self.cache_read,
            "reasoning_tokens": self.reasoning,
        })
    }
}

impl AgentLoopPlugin {
    /// 取消检查（P2/T1）：轮次边界轮询。take 语义（命中即清）。
    /// 返回 Some(error payload) = 已取消，调用方立即收敛返回。
    pub(super) async fn check_cancel(&self, env: &Envelope, session_id: &str) -> Option<Value> {
        if !self.cancels.lock().unwrap().remove(session_id) {
            return None;
        }
        tracing::info!(target: ID, session = %session_id, "cancelled at round boundary");
        self.trace(env, session_id, json!({"type": "error", "where": "cancel", "message": "已被用户取消"})).await;
        Some(json!({"ok": false, "error": {"code": "K499", "message": "已被用户取消"}}))
    }

    /// 轮次边界预算检查（T4）。取消（P2）不在此处理——它是硬中止，由调用方先查
    /// `check_cancel`，语义不与预算混在一起。
    ///
    /// 返回 `Some(reason)` 时由调用方决定怎么停：
    /// - `reason.is_hard()`（token）→ 立即返回错误 payload；
    /// - 否则（墙钟）→ 转入强制收敛轮，先把部分成果说出来。
    ///
    /// token 口径 input+output（usage.total()）；超支边界 = 当前轮已发生的消耗，
    /// 轮内超支由单步 deadline 封顶（护栏语义，不追求精确）。
    pub(super) fn check_budget(&self, budget: &ChatBudget, usage: &UsageAcc) -> Option<StopReason> {
        if let Some(left) = budget.tokens_left {
            if usage.total() >= left {
                tracing::info!(target: ID, used = usage.total(), budget = left, "budget reached: tokens (hard)");
                return Some(StopReason::Tokens);
            }
        }
        if let Some(dl) = budget.deadline {
            if Instant::now() >= dl {
                tracing::info!(target: ID, "budget reached: wall clock (soft, converge)");
                return Some(StopReason::Deadline);
            }
        }
        None
    }
}

#[cfg(test)]
mod p0a_tests {
    use super::*;
    use serde_json::json;

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn acc(input: u64, output: u64, cache_read: u64) -> UsageAcc {
        let mut u = UsageAcc::default();
        u.add(Some(&json!({
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_tokens": cache_read,
        })));
        u
    }

    fn weight() -> f64 {
        cache_credit_weight()
    }

    /// P0-A 核心断言：total() 必须随 cache_read 增长——旧口径（input+output）
    /// 完全无视 cache_read，导致 token 预算闸在真实账单面前漏 9.4 倍。
    #[test]
    fn total_counts_cache_read() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("CACHE_CREDIT_WEIGHT", "0.32");

        let a = acc(1000, 100, 0);
        let b = acc(1000, 100, 100_000);
        assert_eq!(a.total(), 1100, "无 cache 时口径不变");
        assert!(
            b.total() > a.total(),
            "cache_read 必须计入预算口径：{} vs {}",
            b.total(),
            a.total()
        );
        // 100000 × 0.32 = 32000
        assert_eq!(b.total(), 1100 + 32_000);
    }

    /// 权重可配：置 0 即刻退回旧口径（provider 改定价时的应急开关）。
    #[test]
    fn weight_zero_restores_legacy_total() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("CACHE_CREDIT_WEIGHT", "0");
        assert_eq!(weight(), 0.0);
        assert_eq!(acc(1000, 100, 100_000).total(), 1100);
        std::env::remove_var("CACHE_CREDIT_WEIGHT");
        assert_eq!(weight(), 0.32, "缺省 = 实测单价比 32%");
    }

    /// 非法值 warn 后回退缺省（E3 纪律：消灭静默回退）。
    #[test]
    fn invalid_weight_falls_back_to_default() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("CACHE_CREDIT_WEIGHT", "not-a-number");
        assert_eq!(weight(), 0.32);
        std::env::set_var("CACHE_CREDIT_WEIGHT", "-1");
        assert_eq!(weight(), 0.32, "负数非法 → 回退缺省");
        std::env::remove_var("CACHE_CREDIT_WEIGHT");
    }
}
