//! 宿主配置：一律读环境变量（内核 Init 不传 config；子进程继承 env）。

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
            // tools：工作区边界 / scope / 搜索链
            "WORKSPACE_ROOT",
            "TOOLS_ENABLED",
            "SEARCH_REGION",
            "SEARCH_BACKEND",
            "BOCHA_API_KEY",
            "BAIDU_API_KEY",
            "TAVILY_API_KEY",
            // assets：资产目录
            "SKILLS_DIR",
            "PROMPTS_DIR",
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

/// 工具全集名（与 plugins/tools/tools_plugin.py 的 ALL_TOOLS 保持同步）。
pub const ALL_TOOL_NAMES: [&str; 7] = ["read_file", "write_file", "edit_file", "list_dir", "bash", "web_search", "web_read"];

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
}
