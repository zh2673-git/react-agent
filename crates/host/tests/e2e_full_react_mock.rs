//! e2e：全链路 ReAct——四个真实 guest（ts memory + py llm(mock) + py tools + py assets）+ 真 agent-loop。
//! mock 脚本：第 1 轮要 list_dir，第 2 轮收敛。同时验证 assets 软依赖、steps 回传。

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
        {"ok": true, "content": null, "tool_calls": [{"id": "call-1", "name": "list_dir", "arguments": {"path": "."}}], "model": "mock", "finish_reason": "tool_calls"},
        {"ok": true, "content": "answer is listed", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
    ]);
    let mem_dir = std::env::temp_dir().join(format!("react-agent-full-{}-{}", std::process::id(), nanos()));
    let ws_dir = std::env::temp_dir().join(format!("react-agent-ws-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws_dir).expect("create workspace");

    let kernel = fresh_kernel();
    let ws_env = [("WORKSPACE_ROOT", ws_dir.to_string_lossy().into_owned())];

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

    // 3. tools（Python，生产级 7 件套）
    let tools = spawn_python(
        py,
        &plugins_dir().join("tools").join("tools_plugin.py"),
        guest_manifest("tools", &["tools.exec"]),
        &ws_env,
    )
    .await
    .expect("spawn tools");
    register(&kernel, tools).await;

    // 4. assets（Python，软依赖）
    let assets = spawn_python(
        py,
        &plugins_dir().join("assets").join("assets_plugin.py"),
        guest_manifest("assets", &["assets.registry"]),
        &[],
    )
    .await
    .expect("spawn assets");
    register(&kernel, assets).await;

    // 5. agent-loop（InProcess）
    kernel.register(agent_loop(8)).await;

    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "e2e", "user_text": "list the workspace"}),
        ))
        .await
        .expect("dispatch chat");

    assert_eq!(r["ok"], json!(true), "full loop: {r}");
    assert_eq!(r["answer"], json!("answer is listed"));
    assert_eq!(r["rounds"], json!(2));

    // steps 形状（Q）：round 递增、工具名正确、ms 为正
    let steps = r["steps"].as_array().expect("steps present");
    assert_eq!(steps.len(), 1, "{r}");
    assert_eq!(steps[0]["round"], json!(1));
    assert_eq!(steps[0]["tool"], json!("list_dir"));
    assert!(steps[0]["ms"].as_u64().is_some());

    // 会话已持久化（经真实 TS memory guest）
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("memory"), json!({"op": "get", "session_id": "e2e"})))
        .await
        .expect("dispatch memory.get");
    let msgs = expect_ok(&r, "history")["messages"].as_array().unwrap().len();
    assert!(msgs >= 4, "history should contain user/assistant/tool messages: {msgs}");

    let _ = std::fs::remove_dir_all(&mem_dir);
    let _ = std::fs::remove_dir_all(&ws_dir);
    kernel.stop();
    kernel.destroy().await;
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
