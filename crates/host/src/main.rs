//! react-agent 宿主：装配内核 → spawn guest → 注册（memory → llm-adapter → tools → agent-loop，
//! provider 先探测）→ 按 `REACT_FRONTEND` 装配前端（repl / web）。
//!
//! 用法：
//!   cargo run -p react-agent-host -- "一句话问题"     # 单轮
//!   cargo run -p react-agent-host                    # REPL（默认前端）
//!   REACT_FRONTEND=web cargo run -p react-agent-host # web 网关（默认 127.0.0.1:8710）

use anyhow::{bail, Context};
use agent_kernel_sdk::{Envelope, GlobalConfig, PluginId};
use agent_kernel_kernel::Kernel;
use react_agent_host::{config, config::HostConfig, frontend, manifests, spawn};
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // react_progress：agent-loop 逐轮工具调用回显（REPL 实时进度，07 §2.3）
                tracing_subscriber::EnvFilter::new(
                    "warn,react_agent_host=info,react_progress=info",
                )
            }),
        )
        .init();

    // config.json（08 §2.2 持久通道）：启动时应用为 env，spawn 下发复用既有机制
    let applied = config::apply_config_file_to_env();
    if applied > 0 {
        tracing::info!("已从 {} 应用 {applied} 项持久配置", config::config_file().display());
    }

    let cfg = HostConfig::from_env();

    // 流式旁路目录：建目录后以 env 下发（agent-loop 为 InProcess，同进程读 env；
    // llm-adapter 所需路径由 agent-loop 按同一规则拼出后随 payload 下发）
    let stream_dir = config::stream_dir();
    if let Err(e) = std::fs::create_dir_all(&stream_dir) {
        tracing::warn!("流式旁路目录创建失败，退化为非流式: {e}");
    }
    std::env::set_var("AGENT_STREAM_DIR", &stream_dir);

    // Kernel::new 本就返回 Arc<Kernel>
    let kernel = Kernel::new(GlobalConfig {
        node_id: "react-agent".into(),
        max_total_inflight: 32,
    });

    assemble(&kernel, &cfg).await?;

    let session = std::env::var("SESSION_ID").unwrap_or_else(|_| "default".into());
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        frontend::turn(&kernel, &session, &args.join(" ")).await;
    } else {
        frontend::from_env().run(kernel.clone(), session).await?;
    }

    kernel.stop();
    kernel.destroy().await;
    Ok(())
}

/// 组装：spawn + 注册 + 探测。任何 provider 起不来 → 可读报错退出（register 吞 K302，所以先探测）。
async fn assemble(kernel: &Kernel, cfg: &HostConfig) -> anyhow::Result<()> {
    // 1. memory（TS guest）—— memory.session（模型上下文）+ session.trace（只追加事件日志，Phase 3-1）
    let Some(node) = spawn::find_node() else {
        bail!("node 未找到（--experimental-strip-types 需 >= 22.6）");
    };
    let mem_script = cfg.plugins_dir.join("memory").join("memory_plugin.ts");
    let mem = spawn::spawn_node_ts(
        node,
        &mem_script,
        manifests::guest_manifest("memory", &["memory.session", "session.trace"]),
        &[],
    )
    .await
    .context("spawn memory(ts) 失败")?;
    kernel.register(Arc::new(mem)).await;
    probe(kernel, "memory", json!({"op": "get", "session_id": "__probe__"}), "memory(ts)").await?;

    // 2. llm-adapter（Python guest）
    let Some(py) = spawn::find_interpreter() else {
        bail!("python 未找到（guest 需要 grpcio；pip install grpcio httpx）");
    };
    let llm_script = cfg.plugins_dir.join("llm_adapter").join("llm_plugin.py");
    let llm = spawn::spawn_python(
        py,
        &llm_script,
        manifests::guest_manifest("llm-adapter", &["llm.chat"]),
        &cfg.llm_env(),
    )
    .await
    .context("spawn llm-adapter(py) 失败")?;
    kernel.register(Arc::new(llm)).await;
    probe(
        kernel,
        "llm-adapter",
        json!({"op": "chat", "messages": [{"role": "user", "content": "ping"}]}),
        &format!("llm-adapter(py, provider={})", cfg.llm_provider),
    )
    .await?;

    // 3. tools（Python guest）—— 生产级 7 件套，env 透传（工作区边界/搜索链/scope）+ bash 沙箱策略
    let mut tools_env = vec![("WORKSPACE_ROOT".into(), config::workspace_root())];
    tools_env.extend(cfg.passthrough_env());
    apply_bash_sandbox(&mut tools_env).await;
    let tools_script = cfg.plugins_dir.join("tools").join("tools_plugin.py");
    let tools = spawn::spawn_python(
        py,
        &tools_script,
        manifests::guest_manifest("tools", &["tools.exec"]),
        &tools_env,
    )
    .await
    .context("spawn tools(py) 失败")?;
    kernel.register(Arc::new(tools)).await;
    probe(kernel, "tools", json!({"op": "list"}), "tools(py)").await?;

    // 4. assets（Python guest，软依赖）——skills/prompts 注册表；不可用仅 warn 不阻断
    let mut assets_env = cfg.passthrough_env();
    assets_env.push(("WORKSPACE_ROOT".into(), config::workspace_root()));
    let assets_script = cfg.plugins_dir.join("assets").join("assets_plugin.py");
    match spawn::spawn_python(
        py,
        &assets_script,
        manifests::guest_manifest("assets", &["assets.registry"]),
        &assets_env,
    )
    .await
    {
        Ok(assets) => {
            kernel.register(Arc::new(assets)).await;
            match probe(
                kernel,
                "assets",
                json!({"op": "skills.list"}),
                "assets(py)",
            )
            .await
            {
                Ok(()) => {}
                Err(e) => tracing::warn!("assets 探测失败（软依赖，降级为无技能模式）: {e}"),
            }
        }
        Err(e) => tracing::warn!("spawn assets(py) 失败（软依赖，降级为无技能模式）: {e}"),
    }

    // 5. agent-loop（InProcess，硬依赖已全部就位）
    kernel.register(react_agent_agent_loop::new(cfg.max_rounds)).await;
    tracing::info!("agent-loop registered (max_rounds={})", cfg.max_rounds);
    Ok(())
}

/// bash 沙箱装配（05 §2.1 宿主层 fail-closed）：
/// 默认 BASH_SANDBOX=on —— 探测同目录 sandbox-run 助手，通过则向 tools 传 SANDBOX_HELPER；
/// 探测失败/助手缺失/取值非法 → 把 bash 移出 TOOLS_ENABLED（拒绝执行，绝不静默降级为无沙箱直跑）；
/// 仅 BASH_SANDBOX=off 显式豁免时无沙箱直跑（bash 描述如实声明）。
async fn apply_bash_sandbox(tools_env: &mut Vec<(String, String)>) {
    let mode = std::env::var("BASH_SANDBOX").unwrap_or_else(|_| "on".into());
    let helper = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sandbox-run.exe")));
    match config::resolve_bash_sandbox(&mode, helper) {
        config::BashSandbox::ExplicitOff => {
            tracing::info!("bash 沙箱已显式关闭（BASH_SANDBOX=off）：bash 将以完整用户权限直跑");
        }
        config::BashSandbox::Sandboxed { helper } => match probe_sandbox(&helper).await {
            Ok(()) => {
                tracing::info!("bash 沙箱就绪（sandbox-run 受限令牌助手探测通过）");
                tools_env.push(("SANDBOX_HELPER".into(), helper.to_string_lossy().into_owned()));
            }
            Err(e) => deny_bash(tools_env, &format!("沙箱助手探测失败: {e}")),
        },
        config::BashSandbox::Denied(reason) => deny_bash(tools_env, &reason),
    }
}

fn deny_bash(tools_env: &mut Vec<(String, String)>, reason: &str) {
    let idx = tools_env.iter().position(|(k, _)| k == "TOOLS_ENABLED");
    let list: Vec<String> = match idx.and_then(|i| tools_env.get(i)) {
        Some((_, v)) => v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        None => config::ALL_TOOL_NAMES.iter().map(|s| s.to_string()).collect(),
    };
    if list.iter().any(|n| n == "bash") {
        let filtered: Vec<String> = list.into_iter().filter(|n| n != "bash").collect();
        let v = filtered.join(",");
        match idx {
            Some(i) => tools_env[i].1 = v,
            None => tools_env.push(("TOOLS_ENABLED".into(), v)),
        }
        tracing::warn!(
            "bash 沙箱不可用，已 fail-closed 将 bash 移出 TOOLS_ENABLED（{reason}）。如接受无沙箱直跑请设 BASH_SANDBOX=off"
        );
    } else {
        tracing::warn!("bash 沙箱不可用（{reason}）；TOOLS_ENABLED 本就未含 bash，无需调整");
    }
}

/// 沙箱助手探测：真实走一次受限令牌执行（cmd /c exit 0），15s 超时。
async fn probe_sandbox(helper: &std::path::Path) -> anyhow::Result<()> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(helper).arg("probe").output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("探测超时（15s）"))??;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "exit={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

async fn probe(kernel: &Kernel, target: &str, payload: Value, label: &str) -> anyhow::Result<()> {
    let r = kernel
        .dispatch(Envelope::new(PluginId::new(target), payload))
        .await
        .map_err(|e| anyhow::anyhow!("{label} dispatch 失败: {e}"))?;
    if r.get("ok") != Some(&json!(true)) {
        bail!("{label} 探测失败: {r}");
    }
    tracing::info!("{label} 探测通过");
    Ok(())
}
