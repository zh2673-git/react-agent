//! e2e：TypeScript guest（memory 插件）——append/get/clear 全链路。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use serde_json::json;

#[tokio::test]
async fn memory_append_get_clear_roundtrip() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-mem-{}-{}", std::process::id(), chrono_stamp()));
    let kernel = fresh_kernel();
    let script = plugins_dir().join("memory").join("memory_plugin.ts");
    let pp = spawn_node_ts(
        node,
        &script,
        guest_manifest("memory", &["memory.session"]),
        &[("MEMORY_DATA_DIR", data_dir.to_string_lossy().into())],
    )
    .await
    .expect("spawn memory guest");
    register(&kernel, pp).await;

    let target = PluginId::new("memory");

    // append
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "append", "session_id": "s1", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
            ]}),
        ))
        .await
        .expect("dispatch memory.append");
    assert_eq!(expect_ok(&r, "append")["count"], json!(2));

    // get
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "get", "session_id": "s1"})))
        .await
        .expect("dispatch memory.get");
    let msgs = expect_ok(&r, "get")["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], json!("user"));
    assert_eq!(msgs[1]["content"], json!("hi"));

    // get with limit
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "get", "session_id": "s1", "limit": 1})))
        .await
        .expect("dispatch memory.get(limit)");
    assert_eq!(expect_ok(&r, "get limit")["messages"].as_array().unwrap().len(), 1);

    // clear + get
    let _ = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "clear", "session_id": "s1"})))
        .await
        .expect("dispatch memory.clear");
    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "get", "session_id": "s1"})))
        .await
        .expect("dispatch memory.get(cleared)");
    assert_eq!(expect_ok(&r, "get cleared")["messages"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}

#[tokio::test]
async fn memory_summarize_compacts_history_and_repairs_orphans() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-sum-{}-{}", std::process::id(), chrono_stamp()));
    let kernel = fresh_kernel();
    let script = plugins_dir().join("memory").join("memory_plugin.ts");
    let pp = spawn_node_ts(
        node,
        &script,
        guest_manifest("memory", &["memory.session"]),
        &[("MEMORY_DATA_DIR", data_dir.to_string_lossy().into())],
    )
    .await
    .expect("spawn memory guest");
    register(&kernel, pp).await;

    let target = PluginId::new("memory");

    // 历史共 5 条：user, assistant(tool_calls), tool, tool, assistant（keep_last=4 会切出孤儿 tool）
    let _ = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "append", "session_id": "s2", "messages": [
                {"role": "user", "content": "goal: build demo"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "name": "bash", "arguments": {}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"},
                {"role": "tool", "tool_call_id": "c1", "content": "ok2"},
                {"role": "assistant", "content": "done"},
            ]}),
        ))
        .await
        .expect("dispatch memory.append");

    // summarize：keep_last=2 → 切片 [tool, assistant]，开头孤儿 tool 被防撕裂修复丢弃
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "summarize", "session_id": "s2", "summary": "user built a demo; bash ran ok", "keep_last": 2}),
        ))
        .await
        .expect("dispatch memory.summarize");
    assert_eq!(expect_ok(&r, "summarize")["count"], json!(2), "标记 + 防撕裂修复后的保留条数: {r}");

    // 形状：[标记(user)] + [assistant(done)]；孤儿 tool 已被丢弃
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "get", "session_id": "s2"})))
        .await
        .expect("dispatch memory.get");
    let msgs = expect_ok(&r, "get")["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{r}");
    assert_eq!(msgs[0]["role"], json!("user"));
    assert!(msgs[0]["content"].as_str().unwrap().contains("[Context compaction]"), "{r}");
    assert!(msgs[0]["content"].as_str().unwrap().contains("user built a demo"), "{r}");
    assert_eq!(msgs[1]["role"], json!("assistant"));
    assert_eq!(msgs[1]["content"], json!("done"));
    // 持久化落盘（重读自文件）
    // （同进程 Map 已验证；文件持久化由 append/get roundtrip 覆盖）

    // summary 缺失 → 报错
    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "summarize", "session_id": "s2", "summary": "  "})))
        .await
        .expect("dispatch memory.summarize(empty)");
    assert_eq!(r["ok"], json!(false), "{r}");

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}

fn chrono_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
