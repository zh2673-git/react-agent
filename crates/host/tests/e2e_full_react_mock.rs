//! e2e：全链路 ReAct——三个真实 guest（ts memory + py llm(mock) + py tools）+ 真 agent-loop。
//! mock 脚本：第 1 轮要 calculator("2+2")，第 2 轮收敛 "answer is 4"。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use react_agent_agent_loop::new as agent_loop;
use serde_json::json;

#[tokio::test]
async fn full_react_loop_with_mock_llm_and_real_guests() {
    let Some(py) = find_interpreter() else {
        skip("python not found");
        return;
    };
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }

    let script = json!([
        {"ok": true, "content": null, "tool_calls": [{"id": "call-1", "name": "calculator", "arguments": {"expr": "2+2"}}], "model": "mock", "finish_reason": "tool_calls"},
        {"ok": true, "content": "answer is 4", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
    ]);
    let mem_dir = std::env::temp_dir().join(format!("react-agent-full-{}-{}", std::process::id(), nanos()));

    let kernel = fresh_kernel();

    // 1. memory（TS）
    let mem = spawn_node_ts(
        node,
        &plugins_dir().join("memory").join("memory_plugin.ts"),
        guest_manifest("memory", &["memory.session"]),
        &[("MEMORY_DATA_DIR", mem_dir.to_string_lossy().into())],
    )
    .await
    .expect("spawn memory");
    register(&kernel, mem).await;

    // 2. llm-adapter（Python，mock 脚本）
    let llm = spawn_python(
        py,
        &plugins_dir().join("llm_adapter").join("llm_plugin.py"),
        guest_manifest("llm-adapter", &["llm.chat"]),
        &[("LLM_PROVIDER", "mock".into()), ("MOCK_SCRIPT", script.to_string())],
    )
    .await
    .expect("spawn llm-adapter");
    register(&kernel, llm).await;

    // 3. tools（Python）
    let tools = spawn_python(
        py,
        &plugins_dir().join("tools").join("tools_plugin.py"),
        guest_manifest("tools", &["tools.exec"]),
        &[],
    )
    .await
    .expect("spawn tools");
    register(&kernel, tools).await;

    // 4. agent-loop（InProcess）
    kernel.register(agent_loop(8)).await;

    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "e2e", "user_text": "what is 2+2? use the calculator"}),
        ))
        .await
        .expect("dispatch chat");

    assert_eq!(r["ok"], json!(true), "full loop: {r}");
    assert_eq!(r["answer"], json!("answer is 4"));
    assert_eq!(r["rounds"], json!(2));

    // 会话已持久化（经真实 TS memory guest）
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("memory"), json!({"op": "get", "session_id": "e2e"})))
        .await
        .expect("dispatch memory.get");
    let msgs = expect_ok(&r, "history")["messages"].as_array().unwrap().len();
    assert!(msgs >= 4, "history should contain user/assistant/tool messages: {msgs}");

    let _ = std::fs::remove_dir_all(&mem_dir);
    kernel.stop();
    kernel.destroy().await;
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
