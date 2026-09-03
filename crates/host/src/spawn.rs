//! guest 子进程 spawn（照抄内核 e2e 模式）：
//! - python：`python script.py` + `PYTHONPATH=<内核仓>/bindings/python`（guest 运行时）
//! - node：`node --experimental-strip-types script.ts`
//! 子进程继承父 env，额外 env 用于 provider 配置注入。

use crate::config::HostConfig;
use agent_kernel_process::ProcessPlugin;
use agent_kernel_sdk::{KernelError, Manifest};
use std::path::Path;
use tokio::process::Command;

/// 依次探测 python / python3。
pub fn find_interpreter() -> Option<&'static str> {
    for name in ["python", "python3"] {
        if works(name, &["--version"]) {
            return Some(name);
        }
    }
    None
}

/// 探测 node（--experimental-strip-types 需 >= 22.6）。
pub fn find_node() -> Option<&'static str> {
    for name in ["node", "node.exe"] {
        if works(name, &["--version"]) {
            return Some(name);
        }
    }
    None
}

fn works(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub async fn spawn_python(
    interpreter: &str,
    script: &Path,
    manifest: Manifest,
    extra_env: &[(String, String)],
) -> Result<ProcessPlugin, KernelError> {
    let mut cmd = Command::new(interpreter);
    cmd.arg(script).env(
        "PYTHONPATH",
        HostConfig::from_env().kernel_repo.join("bindings").join("python"),
    );
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    ProcessPlugin::spawn(manifest, &mut cmd).await
}

pub async fn spawn_node_ts(
    node: &str,
    script: &Path,
    manifest: Manifest,
    extra_env: &[(String, String)],
) -> Result<ProcessPlugin, KernelError> {
    let mut cmd = Command::new(node);
    cmd.arg("--experimental-strip-types").arg(script);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    ProcessPlugin::spawn(manifest, &mut cmd).await
}
