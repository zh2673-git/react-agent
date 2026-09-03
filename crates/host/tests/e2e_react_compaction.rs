//! e2e：上下文压缩全链路（Phase 2-2）——真实 memory/llm(mock)/tools guest + 真 agent-loop。
//! 历史超过 COMPACT_TRIGGER → 旧史交 LLM 摘要 → memory.summarize 持久化 → 本轮继续干活。
//! 独立测试二进制：COMPACT_TRIGGER 经进程 env 注入，避免与其他测试互相污染。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use react_agent_agent_loop::new as agent_loop;
use serde_json::json;

#[tokio::test]
async fn compaction_triggers_summarize_and_loop_continues() {
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

    // agent-loop 为 InProcess（读测试进程 env）；本二进制仅此一个测试，set_var 安全
    std::env::set_var("COMPACT_TRIGGER", "2");
    std::env::set_var("COMPACT_KEEP", "1");

    // mock 序列：① 摘要应答（无工具）② list_dir 工具调用 ③ 最终收敛
    let script = json!([
        {"ok": true, "content": "user asked to build a demo; a file was written", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
        {"ok": true, "content": null, "tool_calls": [{"id": "call-c", "name": "list_dir", "arguments": {"path": "."}}], "model": "mock", "finish_reason": "tool_calls"},
        {"ok": true, "content": "compaction-final", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
    ]);

    let mem_dir = std::env::temp_dir().join(format!("react-agent-cmp-{}-{}", std::process::id(), nanos()));
    let ws_dir = std::env::temp_dir().join(format!("react-agent-cmp-ws-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws_dir).expect("create workspace");

    let kernel = fresh_kernel();

    // 1. memory（TS guest）—— memory.session + session.trace（事件日志）
    let mem = spawn_node_ts(
        node,
        &plugins_dir().join("memory").join("memory_plugin.ts"),
        guest_manifest("memory", &["memory.session", "session.trace"]),
        &[("MEMORY_DATA_DIR", mem_dir.to_string_lossy().into())],
    )
    .await
    .expect("spawn memory");
    register(&kernel, mem).await;

    // 预置历史 3 条（本 turn 的 user 追加后共 4 > COMPACT_TRIGGER=2）
    let _ = kernel
        .dispatch(Envelope::new(
            PluginId::new("memory"),
            json!({"op": "append", "session_id": "cmp", "messages": [
                {"role": "user", "content": "build a demo"},
                {"role": "assistant", "content": "wrote file"},
                {"role": "user", "content": "now list it"},
            ]}),
        ))
        .await
        .expect("seed history");

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
        &[("WORKSPACE_ROOT", ws_dir.to_string_lossy().into_owned())],
    )
    .await
    .expect("spawn tools");
    register(&kernel, tools).await;

    // 4. agent-loop
    kernel.register(agent_loop(8)).await;

    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "cmp", "user_text": "go on"}),
        ))
        .await
        .expect("dispatch chat");
    assert_eq!(r["ok"], json!(true), "compaction loop: {r}");
    assert_eq!(r["answer"], json!("compaction-final"));

    // 持久化侧已压缩：[标记] + [user] + 本轮 assistant(tool) + tool + assistant(final)
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("memory"), json!({"op": "get", "session_id": "cmp"})))
        .await
        .expect("dispatch memory.get");
    let msgs = expect_ok(&r, "history")["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5, "压缩后历史形状: {r}");
    assert!(
        msgs[0]["content"].as_str().unwrap().contains("[Context compaction]"),
        "首条应为压缩标记: {r}"
    );
    assert!(
        msgs[0]["content"].as_str().unwrap().contains("user asked to build a demo"),
        "标记应含 LLM 摘要内容: {r}"
    );

    // 事件日志（Phase 3-1）：只追加 JSONL，含 user/compaction/tool_call/tool_result/assistant 全链事件
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("memory"),
            json!({"op": "trace.read", "session_id": "cmp"}),
        ))
        .await
        .expect("dispatch trace.read");
    let events = expect_ok(&r, "trace.read")["events"].as_array().unwrap();
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    for expected in ["user", "compaction", "tool_call", "tool_result", "assistant"] {
        assert!(types.contains(&expected), "事件流缺 {expected}: {types:?}");
    }
    let tool_call = events.iter().find(|e| e["type"] == json!("tool_call")).unwrap();
    assert_eq!(tool_call["name"], json!("list_dir"), "{tool_call}");
    assert_eq!(tool_call["args"]["path"], json!("."), "事件应含工具参数: {tool_call}");
    // after=N 增量读取
    let n = expect_ok(&r, "trace.read")["next"].as_u64().unwrap() as i64;
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("memory"),
            json!({"op": "trace.read", "session_id": "cmp", "after": n}),
        ))
        .await
        .expect("dispatch trace.read(after)");
    assert!(expect_ok(&r, "trace.read(after)")["events"].as_array().unwrap().is_empty(), "after=next 应无增量: {r}");

    let _ = std::env::remove_var("COMPACT_TRIGGER");
    let _ = std::env::remove_var("COMPACT_KEEP");
    let _ = std::fs::remove_dir_all(&mem_dir);
    let _ = std::fs::remove_dir_all(&ws_dir);
    kernel.stop();
    kernel.destroy().await;
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
