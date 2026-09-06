//! SSE 事件流（W10 自 frontend.rs 拆分）：从 `after` 起增量轮询 memory 的 trace.read，
//! 逐事件 `data:` 推送；客户端断开即返回。连接建立即从 after=0 重放全量（刷新恢复 =
//! 日志重放），随后跟随实时增量 + 流式旁路（主链与子代理）。
use super::parse_query;
use crate::config;
use agent_kernel_sdk::{Envelope, PluginId};
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(super) async fn sse_events(mut stream: TcpStream, kernel: std::sync::Arc<Kernel>, query: &str) -> anyhow::Result<()> {
    let params = parse_query(query);
    let session = params.get("session").cloned().unwrap_or_else(|| "default".into());
    let mut after: u64 = params.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);

    // 写 SSE 响应头
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ncache-control: no-cache\r\nconnection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let (mut read_half, mut write_half) = tokio::io::split(stream);
    // 重放阶段（after 尚未 catch up）：只推持久 trace 事件、不推流式旁路（最终 assistant 已含完整内容）；
    // 一旦某批 trace.read 返回空（catch up）即进入实时阶段，开始推 stream_*。
    let mut replaying = true;
    let mut ping_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    // 流式旁路：llm-adapter 边生成边写增量，本连接 tail 后与 trace 事件合并推送。
    // session 名非法（防路径穿越）→ None，退化为纯 trace 重放。
    let stream_path = config::stream_file(&session);
    let mut stream_off: u64 = 0;
    // R11 子代理旁路：连接建立时刻（门闩基准）+ 逐子文件偏移表（每个子会话一个旁路文件）。
    let connected_at = std::time::SystemTime::now();
    let mut sub_offs: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    loop {
        // 增量拉取
        let resp = kernel
            .dispatch(Envelope::new(
                PluginId::new("memory"),
                json!({"op": "trace.read", "session_id": session, "after": after}),
            ))
            .await;
        let mut has_new = false;
        match resp {
            Ok(v) if v.get("ok") == Some(&json!(true)) => {
                if let Some(events) = v.get("events").and_then(Value::as_array) {
                    for e in events {
                        let line = format!("data: {e}\n\n");
                        write_half.write_all(line.as_bytes()).await?;
                        has_new = true;
                    }
                }
                after = v.get("next").and_then(Value::as_u64).unwrap_or(after);
                // 本批无新事件 = 已 catch up（重放结束）→ 此后进入实时阶段，开始推流式旁路
                let empty = v.get("events").and_then(Value::as_array).map_or(true, |a| a.is_empty());
                if empty {
                    replaying = false;
                }
            }
            Ok(v) => {
                let line = format!(
                    "event: error\ndata: {}\n\n",
                    json!({"type": "error", "where": "trace.read", "message": v.to_string()})
                );
                write_half.write_all(line.as_bytes()).await?;
            }
            Err(e) => {
                let line = format!(
                    "event: error\ndata: {}\n\n",
                    json!({"type": "error", "where": "trace.read", "message": e.to_string()})
                );
                write_half.write_all(line.as_bytes()).await?;
            }
        }
        // 流式旁路增量：读文件新增字节（按行；未写完的半行留给下一轮补齐）。
        // 仅在实时阶段（replaying=false）推送——重放阶段已由最终 assistant 事件覆盖，不再推中间态。
        let mut streaming = false;
        if !replaying {
        if let Some(path) = &stream_path {
            if let Ok(buf) = std::fs::read(path) {
                let len = buf.len() as u64;
                if len < stream_off {
                    stream_off = 0; // 新一轮以 "w" 覆盖重写 → 从头读
                }
                if len > stream_off {
                    let text = String::from_utf8_lossy(&buf[stream_off as usize..]).to_string();
                    let mut consumed = 0usize;
                    for line in text.split_inclusive('\n') {
                        if !line.ends_with('\n') {
                            break; // 半行：等下一轮
                        }
                        consumed += line.len();
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(mut ev) = serde_json::from_str::<Value>(trimmed) else {
                            continue; // 损坏行：跳过（offset 已推进，不重试）
                        };
                        // 加 stream_ 前缀，避免与 trace 事件类型撞名（如 error）
                        let mapped = match ev.get("type").and_then(Value::as_str) {
                            Some("start") => "stream_start",
                            Some("delta") => "stream_delta",
                            Some("end") => "stream_end",
                            Some("error") => "stream_error",
                            _ => continue,
                        };
                        ev["type"] = json!(mapped);
                        write_half.write_all(format!("data: {ev}\n\n").as_bytes()).await?;
                        streaming = true;
                        has_new = true;
                    }
                    stream_off += consumed as u64;
                }
            }
        }
        }
        // R11 子代理流式旁路：tail `{session}#sub-*.jsonl`（sink 每轮以 "w" 覆写，语义同主文件）。
        // 门闩：仅 tail mtime 晚于连接建立的子文件——上回合/回滚残留的陈旧子文件不复活。
        // 帧打 `sub` 标签（`#` 后缀），前端据此路由进过程框内的「子代理」框。
        if !replaying {
            if let Ok(entries) = std::fs::read_dir(config::stream_dir()) {
                let prefix = format!("{session}#");
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let Some(sub) = name
                        .strip_prefix(&prefix)
                        .and_then(|s| s.strip_suffix(".jsonl"))
                        .filter(|s| !s.is_empty() && !s.contains('#'))
                    else {
                        continue;
                    };
                    if entry.metadata().ok().map_or(true, |m| match m.modified() {
                        Ok(mt) => mt <= connected_at,
                        Err(_) => true,
                    }) {
                        continue; // 陈旧子文件或取不到 mtime：不 tail
                    }
                    let Ok(buf) = std::fs::read(entry.path()) else {
                        continue;
                    };
                    let len = buf.len() as u64;
                    let off = sub_offs.entry(name.clone()).or_insert(0);
                    if len < *off {
                        *off = 0; // 新一轮以 "w" 覆写重写 → 从头读
                    }
                    if len > *off {
                        let text = String::from_utf8_lossy(&buf[*off as usize..]).to_string();
                        let mut consumed = 0usize;
                        for line in text.split_inclusive('\n') {
                            if !line.ends_with('\n') {
                                break; // 半行：等下一轮
                            }
                            consumed += line.len();
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let Ok(mut ev) = serde_json::from_str::<Value>(trimmed) else {
                                continue; // 损坏行：跳过（offset 已推进，不重试）
                            };
                            let mapped = match ev.get("type").and_then(Value::as_str) {
                                Some("start") => "stream_start",
                                Some("delta") => "stream_delta",
                                Some("end") => "stream_end",
                                Some("error") => "stream_error",
                                _ => continue,
                            };
                            ev["type"] = json!(mapped);
                            ev["sub"] = json!(sub);
                            write_half.write_all(format!("data: {ev}\n\n").as_bytes()).await?;
                            streaming = true;
                            has_new = true;
                        }
                        *off += consumed as u64;
                    }
                }
            }
        }
        if has_new {
            write_half.flush().await?;
            if !streaming {
                continue; // 非流式积压立刻再拉；流式期间让位给下面的 sleep，避免忙轮询
            }
        }
        // 心跳注释行（防中间层空闲断连）
        if tokio::time::Instant::now() >= ping_deadline {
            write_half.write_all(b": ping\n\n").await?;
            write_half.flush().await?;
            ping_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        }
        // 等待：流式进行中 50ms（跟手），空闲 300ms（省 memory 轮询开销）
        let poll_ms = if streaming { 50 } else { 300 };
        let mut byte = [0u8; 1];
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {}
            r = read_half.read(&mut byte) => {
                if r.is_err() || r.unwrap_or(1) == 0 {
                    break; // 客户端离开
                }
                // 收到数据（不太可能）：忽略继续
            }
        }
    }
    Ok(())
}
