//! react-agent 宿主：装配内核 → spawn guest → 注册（memory → llm-adapter → tools → agent-loop，
//! provider 先探测）→ 按 `REACT_FRONTEND` 装配前端（repl / web）。
//!
//! 装配细节（插件清单、探测、bash 沙箱、技能工具预热）已抽到 `react_agent_host::assemble`：
//! 默认 `HostSpec::from_env` 与改造前行为逐项一致；发行形态（rsi-agent）可替换 spec。
//!
//! 用法：
//!   cargo run -p react-agent-host -- "一句话问题"     # 单轮
//!   cargo run -p react-agent-host                    # REPL（默认前端）
//!   REACT_FRONTEND=web cargo run -p react-agent-host # web 网关（默认 127.0.0.1:8710）

use agent_kernel_kernel::Kernel;
use agent_kernel_sdk::GlobalConfig;
use react_agent_host::{assemble::HostSpec, config, config::HostConfig, frontend};

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

    // 装配规格（Phase 1 接缝）：默认 spec 与改造前行为逐项一致；发行形态可替换。
    let spec = HostSpec::from_env(&cfg);
    // Kernel::new 本就返回 Arc<Kernel>
    let kernel = Kernel::new(GlobalConfig {
        node_id: spec.node_id.clone(),
        max_total_inflight: spec.max_total_inflight,
    });

    react_agent_host::assemble::assemble_with(&kernel, &spec).await?;

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
