//! E5：真模型端到端冒烟（可选，默认跳过）。
//!
//! 启动条件：`REACT_LIVE_LLM=1` 且进程 env 已配置可用 provider
//! （如 `LLM_PROVIDER=openai` + `OPENAI_API_KEY`，或 ollama 本机在跑）。
//! 真模型调用花钱且输出非确定——断言只验"链路收敛"，不验内容。
//!
//! 运行：`$env:REACT_LIVE_LLM="1"; cargo test -p react-agent-host --test e2e_live_llm`

mod common;

use agent_kernel_kernel::Kernel;
use agent_kernel_sdk::{Envelope, PluginId};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn live_llm_end_to_end_smoke() {
    if std::env::var("REACT_LIVE_LLM").as_deref() != Ok("1") {
        common::skip("设置 REACT_LIVE_LLM=1 并配置真实 provider 后运行（E5 浸泡验证入口）");
        return;
    }
    let py = common::find_interpreter().expect("python 未找到");
    let node = common::find_node().expect("node 未找到");
    let plugins_dir = common::plugins_dir();
    let mem_dir = std::env::temp_dir().join("agent-loop-live-mem");
    std::fs::create_dir_all(&mem_dir).unwrap();

    let kernel = Kernel::new(agent_kernel_sdk::GlobalConfig { node_id: "live-test".into(), max_total_inflight: 8 });

    // 真实 guest 三件套（与生产装配同一来源），LLM provider 配置继承进程 env
    let mem = common::spawn_node_ts(
        node,
        &plugins_dir.join("memory").join("memory_plugin.ts"),
        common::guest_manifest("memory", &["memory.session", "session.trace"]),
        &[("MEMORY_DATA_DIR", mem_dir.to_string_lossy().into_owned())],
    )
    .await
    .expect("spawn memory");
    let llm = common::spawn_python(
        py,
        &plugins_dir.join("llm_adapter").join("llm_plugin.py"),
        common::guest_manifest("llm-adapter", &["llm.chat"]),
        &[],
    )
    .await
    .expect("spawn llm-adapter");
    let tools = common::spawn_python(
        py,
        &plugins_dir.join("tools").join("tools_plugin.py"),
        common::guest_manifest("tools", &["tools.exec"]),
        &[],
    )
    .await
    .expect("spawn tools");

    kernel.register(Arc::new(mem)).await;
    kernel.register(Arc::new(llm)).await;
    kernel.register(Arc::new(tools)).await;
    kernel.register(react_agent_agent_loop::new(6)).await;

    let resp = tokio::time::timeout(
        Duration::from_secs(180),
        kernel.dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "live", "user_text": "只回答数字：1+1等于几？"}),
        )),
    )
    .await
    .expect("真模型 chat 超时（180s）")
    .expect("dispatch failed");

    assert_eq!(resp["ok"], json!(true), "{resp:?}");
    let answer = resp["answer"].as_str().expect("answer");
    assert!(!answer.trim().is_empty(), "真模型返回空答案");
    println!("[live] 真模型冒烟通过，answer = {answer}");
}
