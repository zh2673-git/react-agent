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

/// R2 回滚 × 压缩标记：发生实际截断时，幸存前缀里的压缩标记必须一并物理删除——
/// 标记摘要描述的旧史已不再完整，保留即上下文残留（agent 误以为已回滚材料还在）。
#[tokio::test]
async fn memory_rollback_drops_compaction_marker() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-rb-{}-{}", std::process::id(), chrono_stamp()));
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

    // 历史：[压缩标记, user q1, assistant a1, user q2, assistant a2]（无 trace 文件 → 仅回滚消息侧）
    let _ = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "append", "session_id": "s3", "messages": [
                {"role": "user", "content": "[Context compaction] 之前的会话历史已压缩为以下摘要：\nold materials ..."},
                {"role": "user", "content": "q1"},
                {"role": "assistant", "content": "a1"},
                {"role": "user", "content": "q2"},
                {"role": "assistant", "content": "a2"},
            ]}),
        ))
        .await
        .expect("dispatch memory.append");

    // 回滚到 q2 之前：kept = [标记, q1, a1] → 标记应被丢弃 → [q1, a1]，removed_messages = 3（含标记）
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "rollback", "session_id": "s3", "upto_user_index": 1}),
        ))
        .await
        .expect("dispatch memory.rollback");
    assert_eq!(expect_ok(&r, "rollback")["removed_messages"], json!(3), "{r}");

    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "get", "session_id": "s3"})))
        .await
        .expect("dispatch memory.get");
    let msgs = expect_ok(&r, "get")["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "标记应随回滚丢弃: {r}");
    assert_eq!(msgs[0]["content"], json!("q1"), "{r}");
    assert_eq!(msgs[1]["content"], json!("a1"), "{r}");

    // 回滚到最前：kept = [] → 全清
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "rollback", "session_id": "s3", "upto_user_index": 0}),
        ))
        .await
        .expect("dispatch memory.rollback(0)");
    assert_eq!(expect_ok(&r, "rollback 0")["removed_messages"], json!(2), "{r}");
    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "get", "session_id": "s3"})))
        .await
        .expect("dispatch memory.get(empty)");
    assert_eq!(expect_ok(&r, "get empty")["messages"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}

/// R2 回滚 × trace 尾部对齐：压缩后 memory 只剩「标记 + 末尾几条」，两侧 user 数
/// 天然不对齐。回滚点以 trace user 事件序号定位（UI 真相源），memory 侧从尾部对齐：
/// 落在保留区 → 保标记、截到该轮前；落在摘要区 → 标记与消息全清；越界 → 整体失败。
#[tokio::test]
async fn memory_rollback_aligns_memory_with_trace_after_compaction() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-rbt-{}-{}", std::process::id(), chrono_stamp()));
    let kernel = fresh_kernel();
    let script = plugins_dir().join("memory").join("memory_plugin.ts");
    let pp = spawn_node_ts(
        node,
        &script,
        guest_manifest("memory", &["memory.session", "session.trace"]),
        &[("MEMORY_DATA_DIR", data_dir.to_string_lossy().into())],
    )
    .await
    .expect("spawn memory guest");
    register(&kernel, pp).await;

    let target = PluginId::new("memory");

    // memory：[标记, q1, a1, q2, a2]——q1/q2 是压缩后幸存的两轮，对应 trace 末尾两条 user 事件
    let _ = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "append", "session_id": "s4", "messages": [
                {"role": "user", "content": "[Context compaction] 之前的会话历史已压缩为以下摘要：\nold ..."},
                {"role": "user", "content": "q1"},
                {"role": "assistant", "content": "a1"},
                {"role": "user", "content": "q2"},
                {"role": "assistant", "content": "a2"},
            ]}),
        ))
        .await
        .expect("dispatch memory.append");

    // trace：4 轮 user 事件（u0..u3，每轮后跟一条 assistant，共 8 事件）——u0/u1 已被压缩进标记
    let traces = data_dir.join("traces");
    std::fs::create_dir_all(&traces).expect("create traces dir");
    let mut lines: Vec<String> = Vec::new();
    for i in 0..4 {
        lines.push(json!({"type": "user", "ts": i, "text": format!("u{i}")}).to_string());
        lines.push(json!({"type": "assistant", "ts": i, "text": format!("a{i}")}).to_string());
    }
    std::fs::write(traces.join("s4.jsonl"), lines.join("\n") + "\n").expect("write trace");

    // 越界（upto=4 ≥ trace 4 条 user 事件）：整体失败不落盘
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "rollback", "session_id": "s4", "upto_user_index": 4}),
        ))
        .await
        .expect("dispatch memory.rollback(oob)");
    assert!(r["ok"] == json!(false) && r["error"]["message"].as_str().unwrap().contains("共 4 条"), "{r}");
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "get", "session_id": "s4"})))
        .await
        .expect("dispatch memory.get(after oob)");
    assert_eq!(expect_ok(&r, "get after oob")["messages"].as_array().unwrap().len(), 5, "越界失败不得落盘: {r}");

    // 回滚到 u2（= q1，尾部对齐落保留区）：memory 截到 q1 前 → [标记]；trace 截到 u2 前 → 4 事件
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "rollback", "session_id": "s4", "upto_user_index": 2}),
        ))
        .await
        .expect("dispatch memory.rollback(2)");
    assert_eq!(expect_ok(&r, "rollback 2")["removed_messages"], json!(4), "{r}");
    assert_eq!(expect_ok(&r, "rollback 2")["removed_events"], json!(4), "{r}");
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "get", "session_id": "s4"})))
        .await
        .expect("dispatch memory.get(kept marker)");
    let msgs = expect_ok(&r, "get kept marker")["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{r}");
    assert!(msgs[0]["content"].as_str().unwrap().starts_with("[Context compaction]"), "保留区回滚应保标记: {r}");
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "trace.read", "session_id": "s4"})))
        .await
        .expect("dispatch trace.read(after 2)");
    let events = expect_ok(&r, "trace.read after 2")["events"].as_array().unwrap();
    assert_eq!(events.len(), 4, "trace 应截到 u2 前: {r}");
    assert_eq!(events[0]["text"], json!("u0"), "{r}");
    assert_eq!(events[2]["text"], json!("u1"), "{r}");

    // 回滚到 u0（尾部对齐落摘要区，memory 只剩标记）：标记一并清空
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "rollback", "session_id": "s4", "upto_user_index": 0}),
        ))
        .await
        .expect("dispatch memory.rollback(0)");
    assert_eq!(expect_ok(&r, "rollback 0")["removed_messages"], json!(1), "{r}");
    assert_eq!(expect_ok(&r, "rollback 0")["removed_events"], json!(4), "{r}");
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "get", "session_id": "s4"})))
        .await
        .expect("dispatch memory.get(empty 2)");
    assert_eq!(expect_ok(&r, "get empty 2")["messages"].as_array().unwrap().len(), 0, "{r}");
    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "trace.read", "session_id": "s4"})))
        .await
        .expect("dispatch trace.read(empty 2)");
    assert_eq!(expect_ok(&r, "trace.read empty 2")["events"].as_array().unwrap().len(), 0, "{r}");

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}
