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

    // W17 多窗口：主实例（env 未显式指定 WORKSPACE_ROOT）缺省恢复上次工作区记忆
    // （.instances/.last-workspace）；子实例/命令行启动带显式 env，不受影响。
    if std::env::var("WORKSPACE_ROOT").map(|v| v.trim().is_empty()).unwrap_or(true) {
        if let Some(ws) = react_agent_host::instances::recall_workspace() {
            std::env::set_var("WORKSPACE_ROOT", &ws);
            tracing::info!("已恢复上次工作区: {ws}");
        }
    }

    let cfg = HostConfig::from_env();

    // W17 修正：主实例显式锚定 memory 数据目录（与 memory 插件缺省同口径 plugins_dir/memory/data）。
    // 不设时 files.py 缺省锚 WORKSPACE_ROOT（用户工作区），而 /api/fc-snapshot 与 undo 端点
    // 锚编译期代码根——两侧错位：undo 快照写进用户工作区且 diff 预览必 404。子实例由
    // instances.rs 显式设置不受影响；显式 env 仍最优先。
    if std::env::var_os("MEMORY_DATA_DIR").is_none() {
        std::env::set_var(
            "MEMORY_DATA_DIR",
            cfg.plugins_dir.join("memory").join("data"),
        );
        tracing::info!(
            "MEMORY_DATA_DIR 未设置，锚定 {}",
            cfg.plugins_dir.join("memory").join("data").display()
        );
    }

    // W17：启动即记忆当前工作区（下次启动缺省恢复它；强杀无退出钩子也不丢）
    react_agent_host::instances::remember_workspace(&config::workspace_root());

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

    // W17 迭代十：子实例改为按需复活——主实例启动不再批量拉起注册实例（「打开哪个
    // 恢复哪个」）；恢复入口 = 「新窗口」modal 实例列表（GET /api/instances 标注
    // online/offline，离线实例 POST /api/instances 同工作区即复活，会话/config 延续）。

    if !args.is_empty() {
        frontend::turn(&kernel, &session, &args.join(" ")).await;
    } else {
        frontend::from_env().run(kernel.clone(), session).await?;
    }

    kernel.stop();
    kernel.destroy().await;
    Ok(())
}

/// 组装：并行 spawn + 注册 + 并行探测。memory/llm/tools/assets 互不依赖，串行装配曾让
/// 最慢单项（llm 云端 ping 71s）拖满全程；并行后总时长 = 最慢单项。探测失败策略分层：
/// memory/tools 为硬依赖（bail）；llm-adapter 进程存活即注册成功，云端 ping 不通仅 warn
/// 降级（chat 时自然报错，web 设置可重配）；assets 本就是软依赖。
async fn assemble(kernel: &Kernel, cfg: &HostConfig) -> anyhow::Result<()> {
    let Some(node) = spawn::find_node() else {
        bail!("node 未找到（--experimental-strip-types 需 >= 22.6）");
    };
    let Some(py) = spawn::find_interpreter() else {
        bail!("python 未找到（guest 需要 grpcio；pip install grpcio httpx）");
    };

    // bash 沙箱探测（文件系统级，构造 tools env 前完成；15s 超时）
    let mut tools_env = vec![("WORKSPACE_ROOT".into(), config::workspace_root())];
    tools_env.extend(cfg.passthrough_env());
    apply_bash_sandbox(&mut tools_env).await;

    // ── 并行 spawn 四插件（互不依赖）──
    let mem_script = cfg.plugins_dir.join("memory").join("memory_plugin.ts");
    let mem_f = spawn::spawn_node_ts(
        node,
        &mem_script,
        manifests::guest_manifest("memory", &["memory.session", "session.trace"], false),
        &[],
    );
    let llm_script = cfg.plugins_dir.join("llm_adapter").join("llm_plugin.py");
    // R1：Concurrent——abort op 必须能在流式 chat 进行中被并发受理（Serial = 插件级锁
    // 排队到 chat 结束后，取消永远迟到）。安全性：Python guest gRPC 4 线程池天然并发；
    // provider 均为请求时读 env 的无状态实现，configure 与 chat 的 env 读写竞争良性
    // （最坏一次请求用旧/新模型，无跨调用可变态）。
    let llm_env = cfg.llm_env();
    let llm_f = spawn::spawn_python(
        py,
        &llm_script,
        manifests::guest_manifest("llm-adapter", &["llm.chat"], true),
        &llm_env,
    );
    let tools_script = cfg.plugins_dir.join("tools").join("tools_plugin.py");
    let tools_f = spawn::spawn_python(
        py,
        &tools_script,
        manifests::guest_manifest("tools", &["tools.exec"], false),
        &tools_env,
    );
    let mut assets_env = cfg.passthrough_env();
    assets_env.push(("WORKSPACE_ROOT".into(), config::workspace_root()));
    let assets_script = cfg.plugins_dir.join("assets").join("assets_plugin.py");
    let assets_f = spawn::spawn_python(
        py,
        &assets_script,
        manifests::guest_manifest("assets", &["assets.registry"], false),
        &assets_env,
    );
    let (mem, llm, tools, assets) = tokio::join!(mem_f, llm_f, tools_f, assets_f);

    let mem = mem.context("spawn memory(ts) 失败")?;
    let llm = llm.context("spawn llm-adapter(py) 失败")?;
    let tools = tools.context("spawn tools(py) 失败")?;
    kernel.register(Arc::new(mem)).await;
    kernel.register(Arc::new(llm)).await;
    kernel.register(Arc::new(tools)).await;
    match assets {
        Ok(assets) => kernel.register(Arc::new(assets)).await,
        Err(e) => tracing::warn!("spawn assets(py) 失败（软依赖，降级为无技能模式）: {e}"),
    }

    // ── 并行探测（带超时护栏：慢网络/挂死不得拖死启动）──
    let llm_payload = json!({"op": "chat", "messages": [{"role": "user", "content": "ping"}]});
    let p_mem = probe(kernel, "memory", json!({"op": "get", "session_id": "__probe__"}), "memory(ts)", 30);
    let llm_label = format!("llm-adapter(py, provider={})", cfg.llm_provider);
    let p_llm = probe(kernel, "llm-adapter", llm_payload, &llm_label, 120);
    let tools_payload = json!({"op": "list"});
    let p_tools = probe(kernel, "tools", tools_payload, "tools(py)", 30);
    let assets_payload = json!({"op": "skills.list"});
    let p_assets = probe(kernel, "assets", assets_payload, "assets(py)", 30);
    let (r_mem, r_llm, r_tools, r_assets) = tokio::join!(p_mem, p_llm, p_tools, p_assets);

    r_mem.context("memory 探测失败")?;
    r_tools.context("tools 探测失败")?;
    if let Err(e) = r_llm {
        // 硬依赖判定收窄：guest 进程存活 = 依赖就位；云端联通性降级 warn（chat 时自然报错）
        tracing::warn!("llm-adapter 云端 ping 未通过（启动继续，chat 时会报错，可在 web 设置重配）: {e}");
    }
    if let Err(e) = r_assets {
        tracing::warn!("assets 探测失败（软依赖，降级为无技能模式）: {e}");
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

async fn probe(
    kernel: &Kernel,
    target: &str,
    payload: Value,
    label: &str,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        kernel.dispatch(Envelope::new(PluginId::new(target), payload)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{label} 探测超时（>{timeout_secs}s）"))?
    .map_err(|e| anyhow::anyhow!("{label} dispatch 失败: {e}"))?;
    if r.get("ok") != Some(&json!(true)) {
        bail!("{label} 探测失败: {r}");
    }
    tracing::info!("{label} 探测通过");
    Ok(())
}
