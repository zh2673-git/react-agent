//! 宿主配置：一律读环境变量（内核 Init 不传 config；子进程继承 env）。

use serde_json::Value;
use std::path::PathBuf;

/// bash 沙箱策略解析结果（BASH_SANDBOX，05 §2.1 宿主层 fail-closed）。
#[derive(Debug, Clone, PartialEq)]
pub enum BashSandbox {
    /// 走 sandbox-run 助手（受限令牌，fail-closed）
    Sandboxed { helper: PathBuf },
    /// 显式 BASH_SANDBOX=off：无沙箱直跑（诚实声明）
    ExplicitOff,
    /// fail-closed：bash 移出 TOOLS_ENABLED，拒绝执行
    Denied(String),
}

/// 解析 BASH_SANDBOX 策略。`helper` 为宿主进程旁的 sandbox-run 候选路径。
/// fail-closed 铁律：默认要求沙箱可用；助手缺失/取值非法一律 Denied，绝不静默降级为无沙箱直跑。
pub fn resolve_bash_sandbox(mode: &str, helper: Option<PathBuf>) -> BashSandbox {
    match mode.trim().to_ascii_lowercase().as_str() {
        "off" => BashSandbox::ExplicitOff,
        "on" | "" => match helper {
            Some(h) => BashSandbox::Sandboxed { helper: h },
            None => BashSandbox::Denied("sandbox-run 助手未找到（应与宿主二进制同目录）".into()),
        },
        other => BashSandbox::Denied(format!("BASH_SANDBOX={other} 非法（合法值: on, off）")),
    }
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    /// mock | openai | anthropic | ollama
    pub llm_provider: String,
    pub llm_model: String,
    /// openai 兼容端点（openai provider 用；可指 DeepSeek 等）
    pub llm_base_url: String,
    pub ollama_host: String,
    /// ollama 传输通道：native（/api/chat，缺省）| v1（OpenAI 兼容层回退）
    pub ollama_endpoint: String,
    pub max_rounds: usize,
    /// 内核仓库 checkout（供 PYTHONPATH / TS shim 解析）
    pub kernel_repo: PathBuf,
    /// guest 插件脚本目录
    pub plugins_dir: PathBuf,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl HostConfig {
    pub fn from_env() -> Self {
        // crates/host → crates → workspace root
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("host crate under workspace")
            .to_path_buf();
        let kernel_repo = std::env::var_os("AGENT_KERNEL_REPO")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                workspace
                    .parent()
                    .map(|p| p.join("agent-kernel"))
                    .unwrap_or_else(|| PathBuf::from("../agent-kernel"))
            });
        let plugins_dir = std::env::var_os("PLUGINS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace.join("plugins"));

        let llm_provider = env_or("LLM_PROVIDER", "mock");
        let model_default = match llm_provider.as_str() {
            "openai" => "gpt-4o-mini",
            "anthropic" => "claude-3-5-sonnet-latest",
            "ollama" => "qwen2.5:7b",
            _ => "mock-1",
        };
        Self {
            llm_provider,
            llm_model: env_or("LLM_MODEL", model_default),
            llm_base_url: env_or("LLM_BASE_URL", "https://api.openai.com/v1"),
            ollama_host: env_or("OLLAMA_HOST", "localhost:11434"),
            ollama_endpoint: env_or("OLLAMA_ENDPOINT", "native"),
            max_rounds: env_or("MAX_ROUNDS", "8").parse().unwrap_or(8),
            kernel_repo,
            plugins_dir,
        }
    }

    /// llm-adapter 子进程需要的环境变量。
    pub fn llm_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("LLM_PROVIDER".into(), self.llm_provider.clone()),
            ("LLM_MODEL".into(), self.llm_model.clone()),
            ("LLM_BASE_URL".into(), self.llm_base_url.clone()),
            ("OLLAMA_HOST".into(), self.ollama_host.clone()),
            ("OLLAMA_ENDPOINT".into(), self.ollama_endpoint.clone()),
        ];
        if let Some(k) = std::env::var_os("OPENAI_API_KEY") {
            env.push(("OPENAI_API_KEY".into(), k.to_string_lossy().into()));
        }
        if let Some(k) = std::env::var_os("ANTHROPIC_API_KEY") {
            env.push(("ANTHROPIC_API_KEY".into(), k.to_string_lossy().into()));
        }
        if let Some(k) = std::env::var_os("ANTHROPIC_BASE_URL") {
            env.push(("ANTHROPIC_BASE_URL".into(), k.to_string_lossy().into()));
        }
        if let Some(k) = std::env::var_os("MOCK_SCRIPT") {
            env.push(("MOCK_SCRIPT".into(), k.to_string_lossy().into()));
        }
        env
    }

    /// 从父进程透传给 guest 的可选环境变量（存在才传；guest 内部各自读缺省）。
    pub fn passthrough_env(&self) -> Vec<(String, String)> {
        const KEYS: &[&str] = &[
            // tools：工作区边界 / scope / 搜索链 / MCP server 声明
            "WORKSPACE_ROOT",
            "TOOLS_ENABLED",
            "ALLOW_CORE_WRITE",
            "MCP_SERVERS",
            "SEARCH_REGION",
            "SEARCH_BACKEND",
            "BOCHA_API_KEY",
            "BAIDU_API_KEY",
            "TAVILY_API_KEY",
            // assets：资产目录
            "SKILLS_DIR",
            "PROMPTS_DIR",
            // tools（files.py 变更快照落盘目录，与 memory 同源）
            "MEMORY_DATA_DIR",
            // agent-loop（InProcess 自读，但 WSL/远程场景下保持子进程一致）
            "AGENT_SYSTEM_PROMPT",
            "PROMPT",
            "HISTORY_LIMIT",
        ];
        KEYS
            .iter()
            .filter_map(|k| std::env::var_os(k).map(|v| (k.to_string(), v.to_string_lossy().into_owned())))
            .collect()
    }
}

/// WORKSPACE_ROOT：未设置时取宿主进程 cwd（tools 文件工具的越界拦截根）。
pub fn workspace_root() -> String {
    std::env::var("WORKSPACE_ROOT").unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned())
}

/// 项目根目录（crates/host → crates → workspace root）。
pub fn workspace_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("host crate under workspace")
        .to_path_buf()
}

/// 流式旁路目录：llm-adapter 边生成边写增量，本进程 tail 后经 SSE 推前端。
/// 缺省 `<项目根>/.stream`；可用 AGENT_STREAM_DIR 覆盖。
pub fn stream_dir() -> PathBuf {
    std::env::var_os("AGENT_STREAM_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| workspace_dir().join(".stream"))
}

/// 旁路文件路径。session 名来自 URL 参数，必须过安全校验（防路径穿越）；
/// 非法即 None（退化为无流式）。规则须与 agent-loop 的 stream_file_for 完全一致。
pub fn stream_file(session: &str) -> Option<PathBuf> {
    let safe = !session.is_empty()
        && session.len() <= 64
        && !session.contains("..")
        && session.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !safe {
        return None;
    }
    Some(stream_dir().join(format!("{session}.jsonl")))
}

/// skills 根目录（Web 技能 CRUD 的写入位置；与 assets guest 同源：SKILLS_DIR env 或缺省同目录）。
pub fn skills_dir() -> PathBuf {
    std::env::var_os("SKILLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_dir().join("plugins").join("assets").join("skills"))
}

// ───────────────────────── config.json（08 §2.2 活配置持久态） ─────────────────────────
//
// 形状：{"llm":{...},"tools":{"enabled":[...]},"agent":{...}}
//   agent 段（P5/E1 收编）：{"max_rounds","system_prompt","prompt","history_limit",
//   "compact_trigger","compact_keep","tool_result_limit","budget_secs","token_budget",
//   "retry_attempts","retry_base_ms","llm_context_tokens"} —— 键名即 agent-loop 运行参数，
//   应用为同名 env。
// 持久通道：启动时 apply → env（agent-loop InProcess 自读，spawn 下发复用现有机制，零改动）；
// 热通道：PUT /api/config 转发成功后 env 热应用（下轮对话生效）+ 落盘。
// api_key 明文落 config.json（本机文件），线上回显只给尾 4 位。

/// config.json 路径（CONFIG_FILE env 或 `<项目根>/config.json`）。
pub fn config_file() -> PathBuf {
    std::env::var_os("CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_dir().join("config.json"))
}

/// 读 config.json；缺失/损坏 → Null（启动不因坏配置失败，warn 由调用方记）。
pub fn load_config() -> Value {
    match std::fs::read_to_string(config_file()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// 写 config.json（PUT /api/config 全成后调用）。
pub fn persist_config(v: &Value) -> anyhow::Result<()> {
    let path = config_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

/// agent 段键 → env 名映射（P5/E1）。单一事实来源：持久通道（启动 apply）
/// 与热通道（PUT /api/config）共用。
pub const AGENT_ENV_MAP: &[(&str, &str)] = &[
    ("max_rounds", "MAX_ROUNDS"),
    ("system_prompt", "AGENT_SYSTEM_PROMPT"),
    ("prompt", "PROMPT"),
    ("history_limit", "HISTORY_LIMIT"),
    ("compact_trigger", "COMPACT_TRIGGER"),
    ("compact_keep", "COMPACT_KEEP"),
    ("tool_result_limit", "TOOL_RESULT_LIMIT"),
    ("budget_secs", "CHAT_BUDGET_SECS"),
    ("token_budget", "CHAT_TOKEN_BUDGET"),
    ("retry_attempts", "LLM_RETRY_ATTEMPTS"),
    ("retry_base_ms", "LLM_RETRY_BASE_MS"),
    ("llm_context_tokens", "LLM_CONTEXT_TOKENS"),
    ("output_dir", "AGENT_OUTPUT_DIR"),
];

/// 启动时把 config.json 应用为 env（spawn 下发走既有机制）。返回应用的键数。
pub fn apply_config_file_to_env() -> usize {
    let cfg = load_config();
    if cfg.is_null() {
        return 0;
    }
    let mut n = 0;
    let mut set = |k: &str, v: String| {
        std::env::set_var(k, v);
        n += 1;
    };
    if let Some(llm) = cfg.get("llm") {
        let provider = llm.get("provider").and_then(Value::as_str);
        if let Some(v) = provider {
            set("LLM_PROVIDER", v.into());
        }
        if let Some(v) = llm.get("model").and_then(Value::as_str) {
            set("LLM_MODEL", v.into());
        }
        let anthropic = provider == Some("anthropic");
        if let Some(v) = llm.get("base_url").and_then(Value::as_str) {
            set(if anthropic { "ANTHROPIC_BASE_URL" } else { "LLM_BASE_URL" }, v.into());
        }
        if let Some(v) = llm.get("api_key").and_then(Value::as_str) {
            set(if anthropic { "ANTHROPIC_API_KEY" } else { "OPENAI_API_KEY" }, v.into());
        }
    }
    if let Some(enabled) = cfg.get("tools").and_then(|t| t.get("enabled")).and_then(Value::as_array) {
        let names: Vec<&str> = enabled.iter().filter_map(Value::as_str).collect();
        if !names.is_empty() {
            set("TOOLS_ENABLED", names.join(","));
        }
    }
    // 核心写保护逃生舱（v6 自扩展安全边界）：默认关（核心路径写拒绝），显式开才放行。
    // 持久通道 → env，guest 子进程经 passthrough_env 继承——改后需重启 host。
    if let Some(allow) = cfg.get("tools").and_then(|t| t.get("allow_core_write")).and_then(Value::as_bool) {
        set("ALLOW_CORE_WRITE", if allow { "1".to_string() } else { "0".to_string() });
    }
    // MCP server 声明（tools PLAN §八）：整体 JSON 序列化为 MCP_SERVERS env。只走持久通道
    // （server 子进程生命周期归 tools 插件 init/destroy，热改无意义）——改后需重启 host。
    if let Some(mcp) = cfg.get("mcp_servers").filter(|v| v.is_object()) {
        if let Ok(s) = serde_json::to_string(mcp) {
            set("MCP_SERVERS", s);
        }
    }
    // agent 段（P5/E1）：键名即 agent-loop 参数，字符串原样、其余 JSON 标量转串。
    if let Some(agent) = cfg.get("agent").filter(|v| v.is_object()) {
        if let Some(obj) = agent.as_object() {
            for (key, env) in AGENT_ENV_MAP {
                if let Some(v) = obj.get(*key) {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        Value::Null => continue,
                        other => other.to_string(),
                    };
                    set(env, s);
                }
            }
        }
    }
    // 媒体生成（tools PLAN §九）：media 段 → MEDIA_* env，技能工具子进程经
    // passthrough_env 继承。与 MCP_SERVERS 同款持久通道语义（guest spawn 时 env 固化，
    // 热改不可达已运行 guest）——改后需重启 host。
    if let Some(media) = cfg.get("media").filter(|v| v.is_object()) {
        for (section, prefix) in [("image", "MEDIA_IMAGE"), ("video", "MEDIA_VIDEO")] {
            if let Some(sec) = media.get(section).filter(|v| v.is_object()) {
                for (key, env) in [("base_url", "BASE_URL"), ("model", "MODEL"), ("key", "KEY")] {
                    if let Some(v) = sec.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        set(&format!("{prefix}_{env}"), v.into());
                    }
                }
            }
        }
        if let Some(video) = media.get("video").filter(|v| v.is_object()) {
            for (key, env) in [("submit_url", "MEDIA_VIDEO_SUBMIT_URL"), ("query_url", "MEDIA_VIDEO_QUERY_URL")] {
                if let Some(v) = video.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    set(env, v.into());
                }
            }
        }
    }
    n
}

/// 工具全集名（与 plugins/tools/tools_plugin.py 的 ALL_TOOLS 保持同步）。
pub const ALL_TOOL_NAMES: [&str; 9] = [
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "bash",
    "web_search",
    "web_read",
    "grep",
    "symbols_search",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_sandbox_default_on_requires_helper() {
        // 缺省=on：有助手 → 沙箱；无助手 → Denied（fail-closed，不静默直跑）
        assert!(matches!(
            resolve_bash_sandbox("", Some(PathBuf::from("x/sandbox-run.exe"))),
            BashSandbox::Sandboxed { .. }
        ));
        assert!(matches!(resolve_bash_sandbox("on", None), BashSandbox::Denied(_)));
        assert!(matches!(resolve_bash_sandbox("", None), BashSandbox::Denied(_)));
    }

    #[test]
    fn bash_sandbox_explicit_off_is_exemption() {
        assert_eq!(resolve_bash_sandbox("off", None), BashSandbox::ExplicitOff);
        assert_eq!(resolve_bash_sandbox(" OFF ", None), BashSandbox::ExplicitOff);
    }

    #[test]
    fn bash_sandbox_illegal_value_denied() {
        assert!(matches!(resolve_bash_sandbox("strict", None), BashSandbox::Denied(m) if m.contains("strict")));
    }

    #[test]
    fn config_agent_section_applies_to_env() {
        // P5/E1：config.json agent 段经持久通道应用为 agent-loop 运行参数 env。
        // 本测试操作进程 env 与 CONFIG_FILE——host 单测进程内无其他用例读这些键，存取后即还原。
        let tmp = std::env::temp_dir().join("ra-agent-config-test.json");
        std::fs::write(
            &tmp,
            r#"{"agent":{"max_rounds":12,"history_limit":7,"budget_secs":1.5,"token_budget":500,"system_prompt":"CUSTOM P5","retry_attempts":3,"llm_context_tokens":8192}}"#,
        )
        .unwrap();
        let keys = ["MAX_ROUNDS", "HISTORY_LIMIT", "CHAT_BUDGET_SECS", "CHAT_TOKEN_BUDGET", "AGENT_SYSTEM_PROMPT", "LLM_RETRY_ATTEMPTS", "LLM_CONTEXT_TOKENS", "CONFIG_FILE"];
        let saved: Vec<(String, Option<String>)> = keys.iter().map(|k| (k.to_string(), std::env::var(k).ok())).collect();
        std::env::set_var("CONFIG_FILE", &tmp);

        let n = apply_config_file_to_env();

        assert!(n >= 5, "agent 段 5 个键应被应用: n={n}");
        assert_eq!(std::env::var("MAX_ROUNDS").unwrap(), "12");
        assert_eq!(std::env::var("HISTORY_LIMIT").unwrap(), "7");
        assert_eq!(std::env::var("CHAT_BUDGET_SECS").unwrap(), "1.5");
        assert_eq!(std::env::var("CHAT_TOKEN_BUDGET").unwrap(), "500");
        assert_eq!(std::env::var("AGENT_SYSTEM_PROMPT").unwrap(), "CUSTOM P5");
        assert_eq!(std::env::var("LLM_RETRY_ATTEMPTS").unwrap(), "3");
        assert_eq!(std::env::var("LLM_CONTEXT_TOKENS").unwrap(), "8192");

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn config_media_section_applies_to_env() {
        // PLAN §九：media 段（image/video）→ MEDIA_* env 持久通道映射；空串不覆盖。
        let tmp = std::env::temp_dir().join("ra-media-config-test.json");
        std::fs::write(
            &tmp,
            r#"{"media":{"image":{"base_url":"https://img.example.com/v1","model":"img-1","key":"ik-123","model_override_ignored":true},"video":{"base_url":"https://vid.example.com","model":"vid-1","key":"","submit_url":"/v1/video/submit","query_url":"/v1/video/status?id={task_id}"}}}"#,
        )
        .unwrap();
        let keys = [
            "MEDIA_IMAGE_BASE_URL", "MEDIA_IMAGE_MODEL", "MEDIA_IMAGE_KEY",
            "MEDIA_VIDEO_BASE_URL", "MEDIA_VIDEO_MODEL", "MEDIA_VIDEO_KEY",
            "MEDIA_VIDEO_SUBMIT_URL", "MEDIA_VIDEO_QUERY_URL", "CONFIG_FILE",
        ];
        let saved: Vec<(String, Option<String>)> = keys.iter().map(|k| (k.to_string(), std::env::var(k).ok())).collect();
        std::env::set_var("CONFIG_FILE", &tmp);

        let n = apply_config_file_to_env();

        assert_eq!(std::env::var("MEDIA_IMAGE_BASE_URL").unwrap(), "https://img.example.com/v1");
        assert_eq!(std::env::var("MEDIA_IMAGE_MODEL").unwrap(), "img-1");
        assert_eq!(std::env::var("MEDIA_IMAGE_KEY").unwrap(), "ik-123");
        assert_eq!(std::env::var("MEDIA_VIDEO_BASE_URL").unwrap(), "https://vid.example.com");
        assert_eq!(std::env::var("MEDIA_VIDEO_MODEL").unwrap(), "vid-1");
        assert_eq!(std::env::var("MEDIA_VIDEO_SUBMIT_URL").unwrap(), "/v1/video/submit");
        assert_eq!(std::env::var("MEDIA_VIDEO_QUERY_URL").unwrap(), "/v1/video/status?id={task_id}");
        // 空 key 不落 env（存在才传语义）；未知键忽略
        assert!(std::env::var("MEDIA_VIDEO_KEY").is_err());

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_file(&tmp);
    }
}
