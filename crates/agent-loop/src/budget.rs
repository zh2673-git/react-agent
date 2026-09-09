//! 预算与取消（R12 拆分自 lib.rs）。
//!
//! 职责：`ChatBudget` / `UsageAcc`（单次 chat 的剩余预算与多轮用量累计）、预算/重试
//! 配置读取（`budget_secs` / `token_budget` / `retry_attempts` / `retry_base_ms`）、
//! 瞬态判定（`is_transient_llm_error` / `is_transient_kernel_err`）、轮次边界停车
//! 检查（`check_cancel` / `check_stop`）。

use super::*;
use std::time::Instant;

/// 单次 chat 总时长预算（T4）：`CHAT_BUDGET_SECS`（秒，支持小数便于测试；0=禁用，缺省 900）。
/// 这是轮次边界的护栏——轮内超支由单步 deadline（5s/60s/600s）封顶，不追求精确。
/// 缺省 900s 须 ≥ LLM_DEADLINE(600s)：否则一次长思考 + 工具轮会在轮边界被误掐。
pub(super) fn budget_secs() -> Option<Duration> {
    let v: f64 = std::env::var("CHAT_BUDGET_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(900.0);
    if v <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(v).ok()
}

/// 单次 chat 总 token 预算（T4）：`CHAT_TOKEN_BUDGET`（input+output 累计；0=禁用）。
pub(super) fn token_budget() -> Option<u64> {
    match std::env::var("CHAT_TOKEN_BUDGET").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(n) if n > 0 => Some(n),
        _ => None,
    }
}

/// LLM 瞬态失败重试次数（T3）：`LLM_RETRY_ATTEMPTS`（缺省 2，上限 6）。
pub(super) fn retry_attempts() -> u32 {
    std::env::var("LLM_RETRY_ATTEMPTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2).min(6)
}

/// 重试退避基数毫秒（T3）：`LLM_RETRY_BASE_MS`（缺省 500；0 用于测试立即重试）。
pub(super) fn retry_base_ms() -> u64 {
    std::env::var("LLM_RETRY_BASE_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(500)
}

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
    pub(super) deadline: Option<Instant>,
    pub(super) tokens_left: Option<u64>,
}

/// 多轮累计用量：ReAct 一轮可能多次调用 LLM，用户要的是总消耗而非单轮。
#[derive(Default)]
pub(super) struct UsageAcc {
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
    seen: bool,
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

    /// 计费口径（T4 预算）：input+output（cache_read 是折扣而非独立产出，不计入）。
    pub(super) fn total(&self) -> u64 {
        self.input + self.output
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

    /// 轮次边界统一停车检查（P2 取消 + T4 预算）：取消优先（用户意愿最高），
    /// 其次时长、再次 token。命中即返回错误 payload 立即收敛。
    /// token 口径 input+output（usage.total()）；超支边界 = 当前轮已发生的消耗，
    /// 轮内超支由单步 deadline 封顶（护栏语义，不追求精确）。
    pub(super) async fn check_stop(&self, env: &Envelope, session_id: &str, budget: &ChatBudget, usage: &UsageAcc) -> Option<Value> {
        if let Some(v) = self.check_cancel(env, session_id).await {
            return Some(v);
        }
        if let Some(dl) = budget.deadline {
            if Instant::now() >= dl {
                tracing::info!(target: ID, session = %session_id, "budget exhausted: wall clock");
                self.trace(env, session_id, json!({"type": "error", "where": "budget", "message": "chat 预算耗尽：时长超限"})).await;
                return Some(json!({"ok": false, "error": {"code": "K508", "message": "chat 预算耗尽：时长超限"}}));
            }
        }
        if let Some(left) = budget.tokens_left {
            if usage.total() >= left {
                tracing::info!(target: ID, session = %session_id, used = usage.total(), budget = left, "budget exhausted: tokens");
                self.trace(env, session_id, json!({"type": "error", "where": "budget", "message": "chat 预算耗尽：token 超限"})).await;
                return Some(json!({"ok": false, "error": {"code": "K508", "message": "chat 预算耗尽：token 超限"}}));
            }
        }
        None
    }
}
