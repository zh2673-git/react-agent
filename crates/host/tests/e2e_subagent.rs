//! e2e：subagent 委派全链路（Phase 3-3）——真实 memory/llm(mock)/tools guest + 真 agent-loop。
//! 父会话模型调用保留工具 task → agent-loop 以全新 session_id 复用 agent.chat 全链路跑子代理，
//! 仅回传最终答案；子代理内再嵌套 task → 字段级拒绝；父子事件日志独立且关联。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use react_agent_agent_loop::new as agent_loop;
use serde_json::json;

#[tokio::test]
async fn task_delegates_to_subagent_and_denies_nesting() {
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

    // mock 序列：① 父 round1 task 委派 ② 子 round1 再嵌套 task（应被拒）③ 子 round2 收敛 ④ 父 round2 收敛
    let script = json!([
        {"ok": true, "content": null, "tool_calls": [{"id": "c1", "name": "task", "arguments": {"task": "研究 demo 主题"}}], "model": "mock", "finish_reason": "tool_calls"},
        {"ok": true, "content": null, "tool_calls": [{"id": "c2", "name": "task", "arguments": {"task": "再嵌套一层"}}], "model": "mock", "finish_reason": "tool_calls"},
        {"ok": true, "content": "SUB-ANSWER", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
        {"ok": true, "content": "PARENT-FINAL", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
    ]);

    let mem_dir = std::env::temp_dir().join(format!("react-agent-sub-{}-{}", std::process::id(), nanos()));
    let ws_dir = std::env::temp_dir().join(format!("react-agent-sub-ws-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws_dir).expect("create workspace");

    let kernel = fresh_kernel();

    let mem = spawn_node_ts(
        node,
        &plugins_dir().join("memory").join("memory_plugin.ts"),
        guest_manifest("memory", &["memory.session", "session.trace"]),
        &[("MEMORY_DATA_DIR", mem_dir.to_string_lossy().into())],
    )
    .await
    .expect("spawn memory");
    register(&kernel, mem).await;

    let llm = spawn_python(
        py,
        &plugins_dir().join("llm_adapter").join("llm_plugin.py"),
        guest_manifest("llm-adapter", &["llm.chat"]),
        &[("LLM_PROVIDER", "mock".into()), ("MOCK_SCRIPT", script.to_string())],
    )
    .await
    .expect("spawn llm-adapter");
    register(&kernel, llm).await;

    let tools = spawn_python(
        py,
        &plugins_dir().join("tools").join("tools_plugin.py"),
        guest_manifest("tools", &["tools.exec"]),
        &[("WORKSPACE_ROOT", ws_dir.to_string_lossy().into_owned())],
    )
    .await
    .expect("spawn tools");
    register(&kernel, tools).await;

    kernel.register(agent_loop(8)).await;

    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "root", "user_text": "start"}),
        ))
        .await
        .expect("dispatch chat");
    assert_eq!(r["ok"], json!(true), "parent chat: {r}");
    assert_eq!(r["answer"], json!("PARENT-FINAL"), "parent: {r}");
    assert_eq!(r["rounds"], json!(2), "parent 应两轮收敛: {r}");

    // 父会话历史：user + assistant(task 调用) + tool(子代理答案) + assistant(最终)
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("memory"), json!({"op": "get", "session_id": "root"})))
        .await
        .expect("memory.get root");
    let msgs = expect_ok(&r, "root history")["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4, "父会话历史形状: {r}");
    assert_eq!(msgs[1]["tool_calls"][0]["name"], json!("task"), "{r}");
    let tool_msg_text = msgs[2]["content"].as_str().unwrap();
    assert!(tool_msg_text.contains("SUB-ANSWER"), "task 工具结果应含子代理最终答案: {tool_msg_text}");

    // 父会话事件日志：subagent 事件记录子会话 id，tool_call/tool_result 齐全
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("memory"), json!({"op": "trace.read", "session_id": "root"})))
        .await
        .expect("trace.read root");
    let events = expect_ok(&r, "root trace")["events"].as_array().unwrap();
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    for expected in ["user", "subagent", "tool_call", "tool_result", "assistant"] {
        assert!(types.contains(&expected), "父事件流缺 {expected}: {types:?}");
    }
    let sub_session = events
        .iter()
        .find(|e| e["type"] == json!("subagent"))
        .and_then(|e| e["sub_session"].as_str())
        .expect("subagent 事件应含 sub_session")
        .to_string();
    assert!(sub_session.starts_with("root#sub-"), "子会话 id 应挂在父会话下: {sub_session}");

    // 子会话：独立历史（user + assistant 收敛）与事件日志（含嵌套拒绝的 tool_result ok:false）
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("memory"), json!({"op": "get", "session_id": sub_session})))
        .await
        .expect("memory.get sub");
    let sub_msgs = expect_ok(&r, "sub history")["messages"].as_array().unwrap();
    assert_eq!(sub_msgs.len(), 4, "子会话历史形状（含嵌套拒绝的 tool 喂回）: {r}");
    assert_eq!(sub_msgs[3]["content"], json!("SUB-ANSWER"), "子会话最终答案: {r}");

    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("memory"),
            json!({"op": "trace.read", "session_id": sub_session}),
        ))
        .await
        .expect("trace.read sub");
    let sub_events = expect_ok(&r, "sub trace")["events"].as_array().unwrap();
    let denied = sub_events
        .iter()
        .find(|e| e["type"] == json!("tool_result") && e["ok"] == json!(false))
        .expect("子会话应有嵌套拒绝的 tool_result(ok:false)");
    let denied_text = denied["result_truncated"].as_str().unwrap();
    assert!(denied_text.contains("不支持嵌套委派"), "拒绝原因应回喂给子代理: {denied_text}");

    // 深度守卫复位：新顶层 chat 正常（depth 回 0；mock 耗尽停在最后一项 → PARENT-FINAL）
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "root2", "user_text": "again"}),
        ))
        .await
        .expect("dispatch chat again");
    assert_eq!(r["ok"], json!(true), "深度复位后新会话正常: {r}");

    let _ = std::fs::remove_dir_all(&mem_dir);
    let _ = std::fs::remove_dir_all(&ws_dir);
    kernel.stop();
    kernel.destroy().await;
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
