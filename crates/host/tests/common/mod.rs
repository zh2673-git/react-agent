//! e2e 公共脚手架：内核 e2e 测试模式（缺解释器/缺目录 → 优雅 skip）。

#![allow(dead_code)]

use agent_kernel_kernel::Kernel;
use agent_kernel_process::ProcessPlugin;
use agent_kernel_sdk::{ApiVersion, Capability, Domain, GlobalConfig, KernelError, Manifest, PluginId, PluginKind, Semantics, Version};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("host crate under workspace")
        .to_path_buf()
}

pub fn plugins_dir() -> PathBuf {
    std::env::var_os("PLUGINS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("plugins"))
}

pub fn kernel_repo() -> PathBuf {
    std::env::var_os("AGENT_KERNEL_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().parent().expect("workspace has parent").join("agent-kernel"))
}

pub fn guest_manifest(id: &str, capabilities: &[&str]) -> Manifest {
    Manifest {
        name: PluginId::new(id),
        kind: PluginKind::Capability,
        version: Version::new(0, 1, 0),
        api_version: ApiVersion::new(0, 1),
        capabilities: capabilities.iter().map(|c| Capability::new(*c)).collect(),
        dependencies: vec![],
        domain: Domain::Process,
        semantics: Semantics::Serial,
        priority: 1,
        max_inflight: Some(8),
        fuel_limit: None,
        host_timeout_ms: None,
        epoch_interval_ms: None,
        subscriptions: vec![],
    }
}

pub fn fresh_kernel() -> Arc<Kernel> {
    Kernel::new(GlobalConfig { node_id: "react-agent-test".into(), max_total_inflight: 32 })
}

pub fn find_interpreter() -> Option<&'static str> {
    for name in ["python", "python3"] {
        if works(name, &["--version"]) {
            return Some(name);
        }
    }
    None
}

pub fn find_node() -> Option<&'static str> {
    for name in ["node", "node.exe"] {
        if works(name, &["--version"]) {
            return Some(name);
        }
    }
    None
}

fn works(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

pub async fn spawn_python(
    interpreter: &str,
    script: &PathBuf,
    manifest: Manifest,
    extra_env: &[(&str, String)],
) -> Result<ProcessPlugin, KernelError> {
    let mut cmd = Command::new(interpreter);
    cmd.arg(script)
        .env("PYTHONPATH", kernel_repo().join("bindings").join("python"))
        // 关键：stderr 落 null。否则测试 panic 泄漏的 guest 会继承测试进程的 stderr
        // 管道，cargo 等待管道 EOF 导致整次测试永不退出。
        .stderr(std::process::Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    ProcessPlugin::spawn(manifest, &mut cmd).await
}

pub async fn spawn_node_ts(
    node: &str,
    script: &PathBuf,
    manifest: Manifest,
    extra_env: &[(&str, String)],
) -> Result<ProcessPlugin, KernelError> {
    let mut cmd = Command::new(node);
    cmd.arg("--experimental-strip-types")
        .arg(script)
        .stderr(std::process::Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    ProcessPlugin::spawn(manifest, &mut cmd).await
}

pub async fn register(kernel: &Kernel, plugin: ProcessPlugin) {
    kernel.register(Arc::new(plugin)).await;
}

pub fn skip(reason: &str) {
    eprintln!("[skip] {reason}");
}

/// 断言 payload ok:true 并返回它。
pub fn expect_ok<'a>(v: &'a Value, label: &str) -> &'a Value {
    assert_eq!(v.get("ok"), Some(&serde_json::json!(true)), "{label}: {v}");
    v
}
