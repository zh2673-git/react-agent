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
        // D2 对齐（内核 v0.1.4+）：子代理回归内核 dispatch——不再进程内直调 chat_body。
        // 收益：① 嵌套编排进入调度账本（per-plugin max_inflight / 全局闸真实计数，
        // 此前的调度盲区闭合）；② Draining 期间被 K403 正确拒收；③ panic 经 B2 隔离；
        // ④ 整体 deadline 由内核 select 强制（取自随链衰减的剩余预算）。
        // trace 连续性：显式拷贝父 trace_id（call_capability 每次生成新 trace，不满足
        // 链路语义，故此处走 call_plugin；寻址按自身 PluginId，组合根装配的名字）。
        let mut env = Envelope::new(PluginId::new(ID), payload);
        env.trace_id = src.trace_id;
        if let Some((Some(ms), _)) = budget_snap {
            env.deadline = Some(Duration::from_millis(ms));
        }
        let Some(host) = self.host.get() else {
            return json!({"ok": false, "error": {"code": "K500", "message": "agent-loop: host not initialized"}});
        };
        // R11 事件镜像：委派期间子会话 trace 事件同步透传到父 trace（trace() 据 mirrors 表），
        // 前端「子代理」框据此实时呈现过程（思考流式帧另经旁路文件由网关 tail 透传）。
        self.mirrors.lock().unwrap().insert(sub_session.clone(), parent_session.to_string());
        let dispatched = host.call_plugin(env).await;
        self.mirrors.lock().unwrap().remove(&sub_session);
        let resp = match dispatched {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
        };
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
