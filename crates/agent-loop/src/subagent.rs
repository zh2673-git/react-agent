//! 子代理委派（R12 拆分自 lib.rs）。
//!
//! 职责：`run_subagent`（新会话复用 agent.chat 全链路，委派深度与预算随链传播 +
//! R11 事件镜像注册）与 `task_spec`（`task` 保留工具的声明，随 tools 下发使模型可见可调）。

use super::*;
use std::sync::atomic::Ordering;

/// `task` 保留工具的声明（随 tools 传给模型，使其在严格函数调用协议下可见可调）。
pub(super) fn task_spec() -> ToolSpec {
    ToolSpec {
        name: RESERVED_TASK.into(),
        description: "Delegate a self-contained subtask to a sub-agent (fresh session, same tools). \
Returns only the final answer. Use for heavy research/exploration/summarization to keep this context clean."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "Self-contained task description with all needed context"}
            },
            "required": ["task"]
        }),
    }
}

impl AgentLoopPlugin {
    /// 子代理（Phase 3-3）：新 session_id 复用 agent.chat 全链路（提示词组装/记忆/工具/事件日志）。
    /// 委派深度随链传播：`depth >= 1`（已在子代理内）→ 字段级拒绝再嵌套；
    /// 子会话事件日志独立（session_id 关联可追溯）。
    /// 预算随链衰减（T4）：`budget_snap` = (剩余ms, 剩余token) 写入子 chat 请求；
    /// 预算未启用（两者皆 None）→ 不携带，子链退化为各自的 env 缺省（同为禁用）。
    pub(super) async fn run_subagent(
        &self,
        src: &Envelope,
        parent_session: &str,
        task_text: &str,
        depth: u32,
        budget_snap: Option<(Option<u64>, Option<u64>)>,
    ) -> Value {
        if depth >= 1 {
            return json!({"ok": false, "error": {"code": "K400", "message": "task 不支持嵌套委派（子代理内不可再调用 task）"}});
        }
        let n = self.sub_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let sub_session = format!("{parent_session}#sub-{n}");
        self.trace(src, parent_session, json!({"type": "subagent", "sub_session": sub_session, "task": task_text}))
            .await;
        let mut payload = json!({"op": "chat", "session_id": sub_session, "user_text": task_text, "depth": depth + 1});
        if let Some((ms_left, toks_left)) = budget_snap {
            if let Some(ms) = ms_left {
                payload["budget_ms_left"] = json!(ms);
            }
            if let Some(toks) = toks_left {
                payload["tokens_left"] = json!(toks);
            }
        }
        let env = Envelope::new(PluginId::new(ID), payload);
        // R11 事件镜像：委派期间子会话 trace 事件同步透传到父 trace（trace() 据 mirrors 表），
        // 前端「子代理」框据此实时呈现过程（思考流式帧另经旁路文件由网关 tail 透传）。
        self.mirrors.lock().unwrap().insert(sub_session.clone(), parent_session.to_string());
        // 递归委派：Box::pin 打断未来大小的无限递归（嵌套上限由随链 depth 硬性收敛）
        let resp = Box::pin(self.chat_body(&env)).await;
        self.mirrors.lock().unwrap().remove(&sub_session);
        if resp.get("ok") == Some(&json!(true)) {
            json!({
                "ok": true,
                "answer": resp.get("answer").cloned().unwrap_or(Value::Null),
                "sub_session": sub_session,
            })
        } else {
            // 错误消息瘦身（PLAN E2）：只取 error.message，不再把整个响应 JSON 塞进错误
            let emsg = resp
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            json!({"ok": false, "error": {"code": "SUBAGENT_FAILED", "message": format!("子代理失败: {emsg}")}})
        }
    }
}
