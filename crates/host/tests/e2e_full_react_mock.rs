//! e2e：全链路 ReAct——四个真实 guest（ts memory + py llm(mock) + py tools + py assets）+ 真 agent-loop。
//! mock 脚本：第 1 轮 write_file（产物登记），第 2 轮收敛（答案文本兜底扫描）。
//! 同时验证 assets 软依赖、steps 回传、artifact trace 事件（结构化 + 文本兜底）。

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
        {"ok": true, "content": null, "tool_calls": [{"id": "call-1", "name": "write_file", "arguments": {"path": "outputs/report.md", "content": "# 产物\n"}}], "model": "mock", "finish_reason": "tool_calls"},
        {"ok": true, "content": "已生成 outputs/report.md 与 outputs/data.xlsx", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
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
    assert_eq!(r["answer"], json!("已生成 outputs/report.md 与 outputs/data.xlsx"));
    assert_eq!(r["rounds"], json!(2));

    // steps 形状（Q）：round 递增、工具名正确、ms 为正
    let steps = r["steps"].as_array().expect("steps present");
    assert_eq!(steps.len(), 1, "{r}");
    assert_eq!(steps[0]["round"], json!(1));
    assert_eq!(steps[0]["tool"], json!("write_file"));
    assert!(steps[0]["ms"].as_u64().is_some());

    // 产物登记（Q）：write_file 结构化 artifact + 答案文本兜底扫描 artifact
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("memory"),
            json!({"op": "trace.read", "session_id": "e2e"}),
        ))
        .await
        .expect("dispatch trace.read");
    let events = expect_ok(&r, "trace.read")["events"].as_array().unwrap();
    let artifacts: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["type"] == json!("artifact"))
        .collect();
    assert!(
        artifacts.iter().any(|e| e["path"] == json!("outputs/report.md") && e["tool"] == json!("write_file")),
        "write_file 结构化 artifact 缺失: {events:?}"
    );
    assert!(
        artifacts.iter().any(|e| e["path"] == json!("outputs/data.xlsx")),
        "答案文本兜底 artifact 缺失: {events:?}"
    );

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

/// R11 子代理事件镜像：父轮调 `task` 委派 → 子会话 trace 事件同步透传到父 trace
/// （打 `sub` 标签；`user` 不镜像——回滚 user 序真相源不被污染），子答案回传父轮收敛。
#[tokio::test]
async fn subagent_mirrors_trace_events_to_parent() {
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

    // 3 次 LLM 调用：父 r1 发起委派 → 子 r1 直接收敛 → 父 r2 收敛
    let script = json!([
        {"ok": true, "content": null, "tool_calls": [{"id": "call-1", "name": "task", "arguments": {"task": "子任务：计算 1+1"}}], "model": "mock", "finish_reason": "tool_calls"},
        {"ok": true, "content": "子任务完成：2", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
        {"ok": true, "content": "委派完成，结果是 2", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
    ]);
    let mem_dir = std::env::temp_dir().join(format!("react-agent-sub-{}-{}", std::process::id(), nanos()));
    let ws_dir = std::env::temp_dir().join(format!("react-agent-subws-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws_dir).expect("create workspace");

    let kernel = fresh_kernel();

    let mem = spawn_node_ts(
        node,
        &plugins_dir().join("memory").join("memory_plugin.ts"),
        guest_manifest("memory", &["memory.session"]),
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
            json!({"op": "chat", "session_id": "e2e-sub", "user_text": "委派子任务"}),
        ))
        .await
        .expect("dispatch chat");

    assert_eq!(r["ok"], json!(true), "subagent loop: {r}");
    assert_eq!(r["answer"], json!("委派完成，结果是 2"));

    // 父 trace：subagent 标记 + 镜像事件（sub 标签、user 除外）
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("memory"),
            json!({"op": "trace.read", "session_id": "e2e-sub"}),
        ))
        .await
        .expect("dispatch trace.read");
    let events = expect_ok(&r, "trace.read")["events"].as_array().unwrap();

    assert!(
        events.iter().any(|e| e["type"] == json!("subagent") && e["sub_session"] == json!("e2e-sub#sub-1")),
        "subagent 标记缺失: {events:?}"
    );
    let mirrored: Vec<&serde_json::Value> = events.iter().filter(|e| e["sub"] == json!("sub-1")).collect();
    assert!(
        mirrored.iter().any(|e| e["type"] == json!("assistant") && e["answer"] == json!("子任务完成：2")),
        "子 assistant 事件未镜像到父 trace: {events:?}"
    );
    assert!(
        !mirrored.iter().any(|e| e["type"] == json!("user")),
        "user 事件不得镜像（回滚 user 序真相源）: {mirrored:?}"
    );

    // 子会话 trace 独立存在（审计可追溯）
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("memory"),
            json!({"op": "trace.read", "session_id": "e2e-sub#sub-1"}),
        ))
        .await
        .expect("dispatch trace.read sub");
    let sub_events = expect_ok(&r, "trace.read sub")["events"].as_array().unwrap();
    assert!(sub_events.iter().any(|e| e["type"] == json!("user")), "子 trace 缺 user: {sub_events:?}");

    let _ = std::fs::remove_dir_all(&mem_dir);
    let _ = std::fs::remove_dir_all(&ws_dir);
    kernel.stop();
    kernel.destroy().await;
}
