//! react-agent 宿主：装配内核 → spawn guest → 注册（memory → llm-adapter → tools → agent-loop，
//! provider 先探测）→ 单轮聊天或 stdin REPL。
//!
//! 用法：
//!   cargo run -p react-agent-host -- "一句话问题"     # 单轮
//!   cargo run -p react-agent-host                    # REPL

mod config;
mod manifests;
mod spawn;

use anyhow::{bail, Context};
use agent_kernel_sdk::{Envelope, GlobalConfig, PluginId};
use agent_kernel_kernel::Kernel;
use config::HostConfig;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,react_agent_host=info")),
        )
        .init();

    let cfg = HostConfig::from_env();
    let kernel = Kernel::new(GlobalConfig { node_id: "react-agent".into(), max_total_inflight: 32 });

    assemble(&kernel, &cfg).await?;

    let session = std::env::var("SESSION_ID").unwrap_or_else(|_| "default".into());
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        turn(&kernel, &session, &args.join(" ")).await;
    } else {
        repl(&kernel, &session).await;
    }

    kernel.stop();
    kernel.destroy().await;
    Ok(())
}

/// 组装：spawn + 注册 + 探测。任何 provider 起不来 → 可读报错退出（register 吞 K302，所以先探测）。
async fn assemble(kernel: &Kernel, cfg: &HostConfig) -> anyhow::Result<()> {
    // 1. memory（TS guest）
    let Some(node) = spawn::find_node() else {
        bail!("node 未找到（--experimental-strip-types 需 >= 22.6）");
    };
    let mem_script = cfg.plugins_dir.join("memory").join("memory_plugin.ts");
    let mem = spawn::spawn_node_ts(
        node,
        &mem_script,
        manifests::guest_manifest("memory", &["memory.session"]),
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

    // 3. tools（Python guest）
    let tools_script = cfg.plugins_dir.join("tools").join("tools_plugin.py");
    let tools = spawn::spawn_python(
        py,
        &tools_script,
        manifests::guest_manifest("tools", &["tools.exec"]),
        &[],
    )
    .await
    .context("spawn tools(py) 失败")?;
    kernel.register(Arc::new(tools)).await;
    probe(kernel, "tools", json!({"op": "list"}), "tools(py)").await?;

    // 4. agent-loop（InProcess，硬依赖已全部就位）
    kernel.register(react_agent_agent_loop::new(cfg.max_rounds)).await;
    tracing::info!("agent-loop registered (max_rounds={})", cfg.max_rounds);
    Ok(())
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

async fn turn(kernel: &Kernel, session: &str, text: &str) {
    let env = Envelope::new(
        PluginId::new("agent-loop"),
        json!({"op": "chat", "session_id": session, "user_text": text}),
    );
    match kernel.dispatch(env).await {
        Ok(v) if v.get("ok") == Some(&json!(true)) => {
            if let Some(ans) = v.get("answer").and_then(Value::as_str) {
                println!("{ans}");
            }
        }
        Ok(v) => eprintln!("[agent error] {}", v.get("error").cloned().unwrap_or(v)),
        Err(e) => eprintln!("[kernel error] {e}"),
    }
}

async fn repl(kernel: &Kernel, session: &str) {
    println!("react-agent ready（session={session}）。输入 exit/quit 退出。");
    let mut line = String::new();
    loop {
        print!("react-agent> ");
        let _ = std::io::stdout().flush();
        line.clear();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => break, // EOF
            Ok(_) => {}
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "exit" || text == "quit" {
            break;
        }
        turn(kernel, session, text).await;
    }
}
