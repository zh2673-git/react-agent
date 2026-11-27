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

// ---- M 轮新增：trace.meta / session.skills / system 保留 / 回归 ----

fn write_jsonl(path: &std::path::Path, events: &[serde_json::Value]) {
    let mut s = String::new();
    for e in events {
        s.push_str(&e.to_string());
        s.push('\n');
    }
    std::fs::write(path, s).expect("write jsonl");
}

fn read_jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse jsonl line"))
        .collect()
}

fn assistant_ev(i: usize) -> serde_json::Value {
    json!({"type": "assistant", "ts": i, "text": format!("e{i}")})
}

/// M1：>800 条（分页条数上限）后 trace.meta 仍返回真实末尾游标；trace.read.next 仍只是本页
/// 消费位置；缺 trace 文件返回空状态（ok:true）而非失败。
#[tokio::test]
async fn trace_meta_reports_true_end_beyond_page() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-meta-{}-{}", std::process::id(), chrono_stamp()));
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

    // 1200 条小事件：> 800 条分页上限，但远 < 1MB 字节预算 → 命中条数边界
    let traces = data_dir.join("traces");
    std::fs::create_dir_all(&traces).expect("create traces dir");
    let evs: Vec<serde_json::Value> = (0..1200).map(assistant_ev).collect();
    write_jsonl(&traces.join("s-meta.jsonl"), &evs);

    // trace.meta：完整有效事件总数 + 真实末尾绝对游标
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "trace.meta", "session_id": "s-meta"})))
        .await
        .expect("dispatch trace.meta");
    let meta = expect_ok(&r, "trace.meta");
    assert_eq!(meta["exists"], json!(true), "{r}");
    assert_eq!(meta["total_events"], json!(1200), "完整有效事件数: {r}");
    assert_eq!(meta["end_cursor"], json!(1200), "真实末尾游标: {r}");
    assert!(meta["size"].as_u64().unwrap() > 0, "文件大小: {r}");

    // trace.read 首页只到 800（本页消费位置），next 不是总末尾
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "trace.read", "session_id": "s-meta"})))
        .await
        .expect("dispatch trace.read page1");
    let read1 = expect_ok(&r, "trace.read page1");
    assert_eq!(read1["events"].as_array().unwrap().len(), 800, "首页受 800 条上限: {r}");
    assert_eq!(read1["next"], json!(800), "next 是本页消费位置而非末尾: {r}");

    // 续读追平：after=800 → 余下 400，next=1200
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "trace.read", "session_id": "s-meta", "after": 800}),
        ))
        .await
        .expect("dispatch trace.read page2");
    let read2 = expect_ok(&r, "trace.read page2");
    assert_eq!(read2["events"].as_array().unwrap().len(), 400, "{r}");
    assert_eq!(read2["next"], json!(1200), "{r}");

    // 缺 trace 文件 → 空状态（ok:true, exists:false, 0/0），不是失败
    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "trace.meta", "session_id": "s-nofile"})))
        .await
        .expect("dispatch trace.meta(nofile)");
    let meta2 = expect_ok(&r, "trace.meta nofile");
    assert_eq!(meta2["exists"], json!(false), "{r}");
    assert_eq!(meta2["total_events"], json!(0), "{r}");
    assert_eq!(meta2["end_cursor"], json!(0), "{r}");

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}

/// M1：~1MB 字节预算下 trace.meta 末尾游标仍准；首页按字节预算截断（< 全部条数），
/// 多批 next 前进可追平到真实末尾。
#[tokio::test]
async fn trace_meta_reports_true_end_beyond_byte_budget() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-metab-{}-{}", std::process::id(), chrono_stamp()));
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

    // 300 条 × ~5KB ≈ 1.5MB：> 1MB 字节预算，但条数远小于 800 → 命中字节边界
    let traces = data_dir.join("traces");
    std::fs::create_dir_all(&traces).expect("create traces dir");
    let big = "x".repeat(5000);
    let evs: Vec<serde_json::Value> = (0..300)
        .map(|i| json!({"type": "assistant", "ts": i, "text": big}))
        .collect();
    write_jsonl(&traces.join("s-big.jsonl"), &evs);

    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "trace.meta", "session_id": "s-big"})))
        .await
        .expect("dispatch trace.meta");
    let meta = expect_ok(&r, "trace.meta");
    assert_eq!(meta["total_events"], json!(300), "字节预算不影响总数: {r}");
    assert_eq!(meta["end_cursor"], json!(300), "{r}");

    // 首页受 1MB 字节预算截断：条数 < 300 且 >= 1，next == 本页条数
    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "trace.read", "session_id": "s-big"})))
        .await
        .expect("dispatch trace.read page1");
    let read1 = expect_ok(&r, "trace.read page1");
    let n0 = read1["events"].as_array().unwrap().len();
    assert!(n0 >= 1 && n0 < 300, "字节预算应截断首页（0<n<300）: n={n0} {r}");
    assert_eq!(read1["next"], json!(n0), "{r}");

    // 多批追平到 300
    let mut cursor = n0;
    let mut collected = n0;
    for _ in 0..10 {
        if cursor >= 300 {
            break;
        }
        let r = kernel
            .dispatch(Envelope::new(
                target.clone(),
                json!({"op": "trace.read", "session_id": "s-big", "after": cursor}),
            ))
            .await
            .expect("dispatch trace.read chase");
        let rd = expect_ok(&r, "trace.read chase");
        let n = rd["events"].as_array().unwrap().len();
        let next = rd["next"].as_u64().unwrap() as usize;
        assert!(next > cursor, "cursor 必须前进: {r}");
        cursor = next;
        collected += n;
    }
    assert_eq!(cursor, 300, "追平到真实末尾");
    assert_eq!(collected, 300, "收集到全部 300 条（无遗漏/无重复）");

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}

/// M2：session.skills 扫描完整 trace（含第一页之后），恢复后段 skill_loaded；旧 trace.read
/// 首页会漏掉它们。去重、保序、忽略空 skill。
#[tokio::test]
async fn session_skills_recovers_beyond_first_page() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-skill-{}-{}", std::process::id(), chrono_stamp()));
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

    let traces = data_dir.join("traces");
    std::fs::create_dir_all(&traces).expect("create traces dir");
    let mut evs: Vec<serde_json::Value> = (0..900).map(assistant_ev).collect();
    evs[2] = json!({"type": "skill_loaded", "ts": 2, "skill": "alpha"});
    evs[880] = json!({"type": "skill_loaded", "ts": 880, "skill": "beta"}); // 第一页之外
    evs[885] = json!({"type": "skill_loaded", "ts": 885, "skill": "alpha"}); // 重复 → 去重
    evs[888] = json!({"type": "skill_loaded", "ts": 888, "skill": ""}); // 空 skill → 忽略
    evs[890] = json!({"type": "skill_loaded", "ts": 890, "skill": "gamma"});
    write_jsonl(&traces.join("s-sk.jsonl"), &evs);

    let r = kernel
        .dispatch(Envelope::new(target.clone(), json!({"op": "session.skills", "session_id": "s-sk"})))
        .await
        .expect("dispatch session.skills");
    let v = expect_ok(&r, "session.skills");
    assert_eq!(v["total_events"], json!(900), "{r}");
    let skills: Vec<&str> = v["skills"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert_eq!(skills, vec!["alpha", "beta", "gamma"], "去重保序、恢复后段技能: {r}");

    // 旧做法（trace.read 首页）漏掉 beta —— 证明必须扫完整 trace
    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "trace.read", "session_id": "s-sk"})))
        .await
        .expect("dispatch trace.read page1");
    let first_page = expect_ok(&r, "trace.read page1");
    let has_beta = first_page["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["skill"] == json!("beta"));
    assert!(!has_beta, "首页不应含第 880 条的 beta（证明首页会漏）: {r}");

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}

/// M3：system 不在首位时，压缩后仍保留唯一的规范化 system（置顶、位于压缩标记之前）。
#[tokio::test]
async fn summarize_preserves_system_not_at_head() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-sys-{}-{}", std::process::id(), chrono_stamp()));
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

    // system 在第 3 条（非首位）：[u0, a0, system sysA, u1, a1, u2, a2]
    let _ = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "append", "session_id": "s-sys", "messages": [
                {"role": "user", "content": "u0"},
                {"role": "assistant", "content": "a0"},
                {"role": "system", "content": "sysA"},
                {"role": "user", "content": "u1"},
                {"role": "assistant", "content": "a1"},
                {"role": "user", "content": "u2"},
                {"role": "assistant", "content": "a2"},
            ]}),
        ))
        .await
        .expect("dispatch append");

    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "summarize", "session_id": "s-sys", "summary": "s", "keep_last": 2}),
        ))
        .await
        .expect("dispatch summarize");
    assert_eq!(expect_ok(&r, "summarize")["count"], json!(4), "system + 标记 + u2 + a2: {r}");

    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "get", "session_id": "s-sys"})))
        .await
        .expect("dispatch get");
    let msgs = expect_ok(&r, "get")["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4, "{r}");
    assert_eq!(msgs[0]["role"], json!("system"), "system 置顶: {r}");
    assert_eq!(msgs[0]["content"], json!("sysA"), "{r}");
    assert_eq!(msgs[1]["role"], json!("user"), "{r}");
    assert!(msgs[1]["content"].as_str().unwrap().contains("[Context compaction]"), "{r}");
    assert_eq!(msgs[2]["content"], json!("u2"), "{r}");
    assert_eq!(msgs[3]["content"], json!("a2"), "{r}");
    let sys_count = msgs.iter().filter(|m| m["role"] == json!("system")).count();
    assert_eq!(sys_count, 1, "只能有一个 system: {r}");

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}

/// M3：历史含多个 system 时压缩后只保留一个（取最后一条），不产生冲突 system。
#[tokio::test]
async fn summarize_keeps_single_system() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-sys2-{}-{}", std::process::id(), chrono_stamp()));
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

    // 两个 system：[system sys-old, u0, system sys-new, u1, a1]
    let _ = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "append", "session_id": "s-sys2", "messages": [
                {"role": "system", "content": "sys-old"},
                {"role": "user", "content": "u0"},
                {"role": "system", "content": "sys-new"},
                {"role": "user", "content": "u1"},
                {"role": "assistant", "content": "a1"},
            ]}),
        ))
        .await
        .expect("dispatch append");

    let _ = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "summarize", "session_id": "s-sys2", "summary": "s", "keep_last": 2}),
        ))
        .await
        .expect("dispatch summarize");

    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "get", "session_id": "s-sys2"})))
        .await
        .expect("dispatch get");
    let msgs = expect_ok(&r, "get")["messages"].as_array().unwrap();
    let sys_count = msgs.iter().filter(|m| m["role"] == json!("system")).count();
    assert_eq!(sys_count, 1, "多 system 压成唯一一个: {r}");
    assert_eq!(msgs[0]["role"], json!("system"), "{r}");
    assert_eq!(msgs[0]["content"], json!("sys-new"), "取最后一条 system: {r}");
    assert!(
        !msgs.iter().any(|m| m["content"] == json!("sys-old")),
        "旧 system 不得残留: {r}"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}

/// 回归：rollback / sidecar(events/*.jsonl) / session.index 语义不变——
/// 物理截断 trace、追加 rollback 审计事件、索引登记会话。
#[tokio::test]
async fn rollback_sidecar_and_index_unchanged() {
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/typescript").join("src").join("index.ts").exists() {
        skip("kernel bindings/typescript not found");
        return;
    }

    let data_dir = std::env::temp_dir().join(format!("react-agent-reg-{}-{}", std::process::id(), chrono_stamp()));
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

    // memory：[标记, q1, a1, q2, a2]
    let _ = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "append", "session_id": "s-reg", "messages": [
                {"role": "user", "content": "[Context compaction] 之前的会话历史已压缩为以下摘要：\nold ..."},
                {"role": "user", "content": "q1"},
                {"role": "assistant", "content": "a1"},
                {"role": "user", "content": "q2"},
                {"role": "assistant", "content": "a2"},
            ]}),
        ))
        .await
        .expect("dispatch append");

    // trace：4 轮 user 事件（u0..u3，各跟一条 assistant，共 8 事件）
    let traces = data_dir.join("traces");
    std::fs::create_dir_all(&traces).expect("create traces dir");
    let mut evs: Vec<serde_json::Value> = Vec::new();
    for i in 0..4 {
        evs.push(json!({"type": "user", "ts": i, "text": format!("u{i}")}));
        evs.push(json!({"type": "assistant", "ts": i, "text": format!("a{i}")}));
    }
    let trace_path = traces.join("s-reg.jsonl");
    write_jsonl(&trace_path, &evs);

    // 回滚到 u2：memory 截到 q1 前 → [标记]；trace 截到 u2 前 → 4 事件
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "rollback", "session_id": "s-reg", "upto_user_index": 2}),
        ))
        .await
        .expect("dispatch rollback");
    assert_eq!(expect_ok(&r, "rollback")["removed_messages"], json!(4), "{r}");
    assert_eq!(expect_ok(&r, "rollback")["removed_events"], json!(4), "{r}");

    // trace 物理截断到 4 行
    let remain = read_jsonl(&trace_path);
    assert_eq!(remain.len(), 4, "trace 应截到 u2 前: {}", remain.len());
    assert_eq!(remain[0]["text"], json!("u0"));

    // sidecar：追加了 rollback 审计事件（末行 kind==rollback），原文不复活到工作集
    let sidecar = read_jsonl(&data_dir.join("events").join("s-reg.jsonl"));
    let last_kind = sidecar.last().unwrap()["kind"].as_str().unwrap_or("");
    assert_eq!(last_kind, "rollback", "sidecar 末行应为 rollback 审计: {sidecar:?}");

    // session.index：登记了该会话
    let idx = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(data_dir.join("session.index.json")).expect("read index"),
    )
    .expect("parse index");
    let has_sess = idx["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == json!("s-reg"));
    assert!(has_sess, "session.index 应登记 s-reg: {idx}");

    // 工作集仍是 [标记]
    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "get", "session_id": "s-reg"})))
        .await
        .expect("dispatch get");
    let msgs = expect_ok(&r, "get")["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{r}");
    assert!(msgs[0]["content"].as_str().unwrap().starts_with("[Context compaction]"), "{r}");

    let _ = std::fs::remove_dir_all(&data_dir);
    kernel.stop();
    kernel.destroy().await;
}
