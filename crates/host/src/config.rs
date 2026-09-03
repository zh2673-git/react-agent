//! 宿主配置：一律读环境变量（内核 Init 不传 config；子进程继承 env）。

use std::path::PathBuf;

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
}
