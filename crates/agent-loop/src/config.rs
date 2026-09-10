//! AgentLoopConfig（E3，第三轮审计）：运行参数单点收编。
//!
//! 目标（对应审计 E3/D4）：
//! 1. **单一解析点**——原先散落 6 个文件、15 处 `std::env::var` 直读，全部收拢到
//!    `from_env()`（每次 chat 开始解析一份快照，Web 配置中心热通道语义不变：改 env
//!    下轮对话即生效）；
//! 2. **warn-on-invalid**——原先 `unwrap_or(default)` 静默回退（拼错键名 = 无声用缺省），
//!    现在解析失败显式 warn 并回退，消除静默偏差；
//! 3. **typed 快照**——调用方拿 `&AgentLoopConfig`，不再各自摸 env。
//!
//! 不收编（保持直读，均有明确理由）：`WORKSPACE_ROOT`（系统边界，非 agent 可调项）、
//! `LLM_PROVIDER`（由 from_env 消费用于窗口判定，但 provider 本身属 llm-adapter 配置）、
//! `CONFIG_FILE` + 密钥 env（secrets.rs 安全域，独立 mtime 刷新机制，见该文件）。

use std::time::Duration;

/// 会消费 `num_ctx` 的本地窗口型 provider（显存受限，常需调小窗口换速度）。
/// 新本地后端（llama.cpp server / vLLM / LM Studio 等）接入后在此加名。
const LOCAL_WINDOW_PROVIDERS: [&str; 1] = ["ollama"];

/// agent-loop 运行参数快照（每次 chat 开始时从 env 解析一份）。
#[derive(Debug, Clone)]
pub(crate) struct AgentLoopConfig {
    /// 生效轮次上限（MAX_ROUNDS 缺省 = 构造参数）。
    pub max_rounds: usize,
    /// 单次发送工作集条数上限（HISTORY_LIMIT，0=无限制）。
    pub history_limit: usize,
    /// 压缩触发条数闸（COMPACT_TRIGGER，0=禁用）。
    pub compact_trigger: usize,
    /// 压缩保留最近条数（COMPACT_KEEP）。
    pub compact_keep: usize,
    /// 工具结果回喂上限字符（TOOL_RESULT_LIMIT，0=禁用）。
    pub tool_result_limit: usize,
    /// 文本附件内嵌上限字符（TEXT_ATTACH_LIMIT，0=禁用）。
    pub text_attach_limit: usize,
    /// 单次 chat 墙钟预算（CHAT_BUDGET_SECS，None=禁用）。
    pub budget: Option<Duration>,
    /// 单次 chat token 预算（CHAT_TOKEN_BUDGET，None=禁用）。
    pub token_budget: Option<u64>,
    /// LLM 瞬态重试次数（LLM_RETRY_ATTEMPTS）。
    pub retry_attempts: u32,
    /// 重试退避基数毫秒（LLM_RETRY_BASE_MS）。
    pub retry_base_ms: u64,
    /// 模型上下文窗口 token（LLM_CONTEXT_TOKENS，仅本地窗口型 provider 生效，0=禁用）。
    pub context_window: usize,
    /// 流式旁路目录（AGENT_STREAM_DIR；None=无流式）。
    pub stream_dir: Option<std::path::PathBuf>,
    /// 产物输出目录（AGENT_OUTPUT_DIR，工作区内相对路径）。
    pub output_dir: String,
    /// 系统提示词整体覆盖（AGENT_SYSTEM_PROMPT）。
    pub system_prompt_override: Option<String>,
    /// 具名提示词模板（PROMPT，经 assets 解析）。
    pub prompt_template: Option<String>,
}

fn env_trim(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// 解析数值 env：非法值 **warn 后回退缺省**（消灭 unwrap_or 静默偏差）。
fn env_num<T: std::str::FromStr + std::fmt::Debug>(key: &str, default: T, what: &str) -> T {
    let Some(raw) = env_trim(key) else {
        return default;
    };
    match raw.parse::<T>() {
        Ok(n) => n,
        Err(_) => {
            tracing::warn!(target: crate::ID, "配置 {key}={raw:?} 非法（{what}），回退缺省 {default:?}——请检查 config.json agent 段");
            default
        }
    }
}

fn secs_opt(v: f64) -> Option<Duration> {
    if v <= 0.0 {
        None
    } else {
        Duration::try_from_secs_f64(v).ok()
    }
}

fn num_opt(v: u64) -> Option<u64> {
    (v > 0).then_some(v)
}

impl AgentLoopConfig {
    /// 每次 chat 开始解析一份快照（`base_max_rounds` = 构造参数，作为 MAX_ROUNDS 缺省）。
    pub fn from_env(base_max_rounds: usize) -> Self {
        let provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
        let local_window = LOCAL_WINDOW_PROVIDERS.contains(&provider.as_str());
        let context_window = if local_window {
            env_num("LLM_CONTEXT_TOKENS", 0usize, "上下文窗口 token")
        } else {
            0 // 云端 API 窗口由服务端管理：禁用 token 闸、不下发 num_ctx（L7）
        };
        Self {
            max_rounds: env_num("MAX_ROUNDS", base_max_rounds.max(1), "最大轮数"),
            history_limit: env_num("HISTORY_LIMIT", 0usize, "单次发送条数上限"),
            compact_trigger: env_num("COMPACT_TRIGGER", 40usize, "压缩触发条数"),
            compact_keep: env_num("COMPACT_KEEP", 10usize, "压缩保留条数"),
            tool_result_limit: env_num("TOOL_RESULT_LIMIT", crate::DEFAULT_TOOL_RESULT_CHARS, "工具结果回喂上限"),
            text_attach_limit: env_num("TEXT_ATTACH_LIMIT", crate::DEFAULT_TEXT_ATTACH_CHARS, "文本附件内嵌上限"),
            budget: secs_opt(env_num("CHAT_BUDGET_SECS", 900.0f64, "chat 墙钟预算秒")),
            token_budget: num_opt(env_num("CHAT_TOKEN_BUDGET", 0u64, "chat token 预算")),
            retry_attempts: env_num("LLM_RETRY_ATTEMPTS", 2u32, "瞬态重试次数").min(6),
            retry_base_ms: env_num("LLM_RETRY_BASE_MS", 500u64, "重试退避基数毫秒"),
            context_window,
            stream_dir: std::env::var_os("AGENT_STREAM_DIR")
                .map(std::path::PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty()),
            output_dir: env_trim("AGENT_OUTPUT_DIR").unwrap_or_else(|| "outputs".into()),
            system_prompt_override: env_trim("AGENT_SYSTEM_PROMPT"),
            prompt_template: env_trim("PROMPT"),
        }
    }

    /// 发送预算：窗口 × 0.7（预留输出与工具 schema）；窗口 0 → 预算 0（闸禁用）。
    pub fn ctx_budget(&self) -> usize {
        if self.context_window == 0 {
            0
        } else {
            (self.context_window as f64 * 0.7) as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// L7 回归（自 context.rs 迁入）：窗口值仅本地窗口型 provider 生效；
    /// 非法值 warn 后回退缺省（E3：消灭静默回退）。
    #[test]
    fn window_only_applies_to_local_window_providers() {
        let _g = env_lock().lock().unwrap();
        let (prov, tok) = (std::env::var("LLM_PROVIDER").ok(), std::env::var("LLM_CONTEXT_TOKENS").ok());

        std::env::set_var("LLM_PROVIDER", "openai");
        std::env::set_var("LLM_CONTEXT_TOKENS", "8192");
        let cfg = AgentLoopConfig::from_env(8);
        assert_eq!(cfg.context_window, 0, "云端 provider 不消费窗口：token 闸禁用");
        assert_eq!(cfg.ctx_budget(), 0);

        std::env::set_var("LLM_PROVIDER", "ollama");
        let cfg = AgentLoopConfig::from_env(8);
        assert_eq!(cfg.context_window, 8192, "ollama 读窗口值");
        assert_eq!(cfg.ctx_budget(), 5734); // 8192 × 0.7

        std::env::set_var("LLM_CONTEXT_TOKENS", "0");
        assert_eq!(AgentLoopConfig::from_env(8).context_window, 0, "0=禁用（向后兼容）");

        // E3：非法值不再静默——解析失败回退缺省（0=禁用），由 warn 记录
        std::env::set_var("LLM_CONTEXT_TOKENS", "not-a-number");
        assert_eq!(AgentLoopConfig::from_env(8).context_window, 0, "非法值回退缺省");

        std::env::remove_var("LLM_CONTEXT_TOKENS");
        assert_eq!(AgentLoopConfig::from_env(8).context_window, 0, "缺省=禁用");

        match prov {
            Some(v) => std::env::set_var("LLM_PROVIDER", v),
            None => std::env::remove_var("LLM_PROVIDER"),
        }
        match tok {
            Some(v) => std::env::set_var("LLM_CONTEXT_TOKENS", v),
            None => std::env::remove_var("LLM_CONTEXT_TOKENS"),
        }
    }
}
