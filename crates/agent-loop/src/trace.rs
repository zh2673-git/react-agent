//! 事件日志与流式旁路（R12 拆分自 lib.rs）。
//!
//! 职责：`trace`（只追加 JSONL 审计日志 + R11 子代理镜像）、`stream_dir` /
//! `stream_file_for`（流式旁路文件路径推导与安全校验）。

use super::*;

/// 旁路文件路径：session 名来自 URL 参数，必须过安全校验（防路径穿越）。
/// `#` 仅为子代理会话分隔符（`{parent}#sub-{n}`，R11）：允许出现在文件名中，
/// 宿主网关按 `{session}#sub-*.jsonl` 前缀 glob 子文件——`#` 不进 URL，无注入面。
/// 目录来自 `AgentLoopConfig.stream_dir`（E3 收编，宿主以 AGENT_STREAM_DIR 下发）；
/// 未配置 → 无流式（行为同改造前）。
pub(super) fn stream_file_for(dir: Option<&std::path::Path>, session: &str) -> Option<String> {
    let dir = dir?;
    let safe = !session.is_empty()
        && session.len() <= 64
        && !session.contains("..")
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '#'));
    if !safe {
        return None;
    }
    Some(dir.join(format!("{session}.jsonl")).to_string_lossy().into_owned())
}

impl AgentLoopPlugin {
    /// 事件日志（Phase 3-1，dsh：Model-visible means logged）：只追加 JSONL，
    /// 服务于审计/恢复/UI 重放。尽力而为——失败仅 debug，不阻断主流程。
    /// R11 子代理镜像：session_id 命中 mirrors（子代理委派进行中）→ 同一事件补写
    /// 一份到父 trace 并打 `sub` 标签（前端过程框内「子代理」框的渲染依据，实时与
    /// 重放同源）。`user` 事件不镜像——父 trace 的 user 事件序是回滚定位真相源，
    /// 混入子轮次会错位（任务文本已由 `subagent` 事件承载）。
    pub(super) async fn trace(&self, src: &Envelope, session_id: &str, mut event: Value) {
        if let Some(obj) = event.as_object_mut() {
            obj.entry("ts".to_string()).or_insert_with(|| {
                json!(std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0))
            });
        }
        let mirror = self.mirrors.lock().unwrap().get(session_id).cloned();
        if let Err(e) = self
            .call(
                src,
                CAP_MEMORY,
                json!({"op": "trace.append", "session_id": session_id, "events": [event]}),
                MEM_DEADLINE,
            )
            .await
        {
            tracing::debug!(target: ID, "trace.append failed: {e}");
        }
        if let Some(parent) = mirror {
            if event.get("type").and_then(Value::as_str) != Some("user") {
                let sub = session_id.rsplit('#').next().unwrap_or(session_id).to_string();
                if let Some(obj) = event.as_object_mut() {
                    obj.insert("sub".into(), json!(sub));
                }
                if let Err(e) = self
                    .call(
                        src,
                        CAP_MEMORY,
                        json!({"op": "trace.append", "session_id": parent, "events": [event]}),
                        MEM_DEADLINE,
                    )
                    .await
                {
                    tracing::debug!(target: ID, "subagent mirror append failed: {e}");
                }
            }
        }
    }
}
