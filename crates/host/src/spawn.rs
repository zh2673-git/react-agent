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
    let pythonpath = HostConfig::from_env()
        .kernel_repo
        .join("bindings")
        .join("python");
    spawn_with_retry("python", &manifest, || {
        let mut cmd = Command::new(interpreter);
        cmd.arg(script).env("PYTHONPATH", &pythonpath);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd
    })
    .await
}

pub async fn spawn_node_ts(
    node: &str,
    script: &Path,
    manifest: Manifest,
    extra_env: &[(String, String)],
) -> Result<ProcessPlugin, KernelError> {
    spawn_with_retry("node", &manifest, || {
        let mut cmd = Command::new(node);
        cmd.arg("--experimental-strip-types").arg(script);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd
    })
    .await
}

/// 装载期首行 PORT 的等待是内核固化的 5s：冷启动（构建后首次执行、杀软扫描、
/// node 类型擦除首次编译）偶发踩线即报 K700『guest port line timeout』。
/// 只对超时类失败重试；`kill_on_drop` 保证超时的 guest 随本轮 Command 回收，
/// 否则下一轮会与残留进程抢 stdout/PORT。
const STARTUP_ATTEMPTS: usize = 3;

fn is_startup_timeout(e: &KernelError) -> bool {
    matches!(e, KernelError::DomainUnavailable(m) if m.contains("timeout"))
}

async fn spawn_with_retry<F>(
    label: &str,
    manifest: &Manifest,
    mut build: F,
) -> Result<ProcessPlugin, KernelError>
where
    F: FnMut() -> Command,
{
    let mut attempt = 1;
    loop {
        let mut cmd = build();
        cmd.kill_on_drop(true);
        match ProcessPlugin::spawn(manifest.clone(), &mut cmd).await {
            Ok(plugin) => {
                if attempt > 1 {
                    tracing::warn!("{label} 第 {attempt} 次 spawn 成功（前序启动超时）");
                }
                return Ok(plugin);
            }
            Err(e) if is_startup_timeout(&e) && attempt < STARTUP_ATTEMPTS => {
                tracing::warn!("{label} 启动超时（{attempt}/{STARTUP_ATTEMPTS}），重试: {e}");
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            Err(e) => return Err(e),
        }
    }
}
