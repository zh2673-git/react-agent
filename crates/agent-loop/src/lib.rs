//! ReAct 编排插件（Rust，InProcess 域）。
//!
//! 时间契约：主执行流为**状态机循环**（感知→规划→行动→观察→回跳），收敛于最终答案
//! 或 `max_rounds` 强制收敛。循环状态全部存于局部变量与会话记忆（memory 插件），
//! 本体 `&self` 无跨调用可变态（A1）。
//!
//! 空间契约：跨插件通信一律走 `HostApi::call_plugin`（按 `Envelope.target` 路由），
//! 不直接触碰任何其他插件的状态。
//!
//! 依赖（硬）：`memory.session` / `llm.chat` / `tools.exec`——由 host 按
//! memory → llm-adapter → tools → agent-loop 的顺序注册后生效。

mod contract;

pub use contract::{ChatReq, LlmChatResp, MemoryMsg, ToolCall, ToolSpec};

use agent_kernel_sdk::*;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const ID: &str = "agent-loop";
pub const ID_MEMORY: &str = "memory";
pub const ID_LLM: &str = "llm-adapter";
pub const ID_TOOLS: &str = "tools";

/// 各转发步的相对截止（A2：Envelope.deadline 为相对时长）。
const MEM_DEADLINE: Duration = Duration::from_secs(5);
const TOOLS_DEADLINE: Duration = Duration::from_secs(10);
const LLM_DEADLINE: Duration = Duration::from_secs(120);

const SYSTEM_PROMPT: &str = "You are a helpful agent. Answer the user's request. \
When tools are provided and useful, call them (one batch per round); after receiving tool results, \
continue reasoning until you can produce the final answer in plain text.";

pub struct AgentLoopPlugin {
    max_rounds: usize,
    manifest: Manifest,
    host: OnceLock<Arc<dyn HostApi>>,
}

/// 构造插件实例（`Arc<dyn Plugin>`）。
pub fn new(max_rounds: usize) -> PluginInstance {
    let manifest = Manifest {
        name: PluginId::new(ID),
        kind: PluginKind::Orchestrator,
        version: Version::new(0, 1, 0),
        api_version: ApiVersion::new(1, 0),
        capabilities: vec![Capability::new("agent.chat")],
        dependencies: vec![
            DependencySpec { capability: Capability::new("memory.session"), hard: true },
            DependencySpec { capability: Capability::new("llm.chat"), hard: true },
            DependencySpec { capability: Capability::new("tools.exec"), hard: true },
        ],
        domain: Domain::InProcess,
        semantics: Semantics::Serial,
        priority: 1,
        max_inflight: Some(4),
        fuel_limit: None,
        host_timeout_ms: None,
        epoch_interval_ms: None,
        subscriptions: vec![],
    };
    Arc::new(AgentLoopPlugin { max_rounds, manifest, host: OnceLock::new() })
}

impl AgentLoopPlugin {
    /// 跨插件调用便捷封装：复制 trace_id/priority，附 deadline。
    async fn call(
        &self,
        src: &Envelope,
        target: &str,
        payload: Value,
        deadline: Duration,
    ) -> Result<Value, KernelError> {
        let host = self
            .host
            .get()
            .ok_or_else(|| KernelError::Internal("agent-loop: host not initialized".into()))?;
        let mut fwd = Envelope::new(PluginId::new(target), payload);
        fwd.trace_id = src.trace_id;
        fwd.priority = src.priority;
        fwd.deadline = Some(deadline);
        host.call_plugin(fwd).await
    }

    /// 感知：拉取会话历史。
    async fn perceive(&self, src: &Envelope, session_id: &str) -> Result<Vec<MemoryMsg>, KernelError> {
        let v = self
            .call(src, ID_MEMORY, json!({"op": "get", "session_id": session_id}), MEM_DEADLINE)
            .await?;
        let msgs = serde_json::from_value(v.get("messages").cloned().unwrap_or(Value::Null))
            .unwrap_or_default();
        Ok(msgs)
    }

    /// 规划：调用 LLM（含/不含工具）。
    async fn plan(
        &self,
        src: &Envelope,
        messages: &[MemoryMsg],
        tools: Option<&[ToolSpec]>,
    ) -> Result<LlmChatResp, KernelError> {
        let mut payload = json!({"op": "chat", "messages": messages});
        if let Some(t) = tools {
            if !t.is_empty() {
                payload["tools"] = json!(t);
            }
        }
        let v = self.call(src, ID_LLM, payload, LLM_DEADLINE).await?;
        Ok(serde_json::from_value(v).unwrap_or(LlmChatResp {
            ok: false,
            content: None,
            tool_calls: vec![],
            model: String::new(),
            finish_reason: String::new(),
            error: Some(json!({"code": "LLM_BAD_SHAPE", "message": "llm-adapter 返回了无法解析的响应"})),
        }))
    }

    /// 行动：执行单个工具调用（失败合成 ok:false 结果，不中断循环）。
    async fn act(&self, src: &Envelope, tc: &ToolCall) -> Value {
        let payload = json!({"op": "call", "name": tc.name, "args": tc.arguments});
        match self.call(src, ID_TOOLS, payload, TOOLS_DEADLINE).await {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
        }
    }

    /// 观察：写入记忆（尽力而为，失败不致命）。
    async fn observe(&self, src: &Envelope, session_id: &str, msgs: &[MemoryMsg]) {
        if let Err(e) = self
            .call(src, ID_MEMORY, json!({"op": "append", "session_id": session_id, "messages": msgs}), MEM_DEADLINE)
            .await
        {
            tracing::warn!(target: ID, "memory append failed: {e}");
        }
    }

    /// ReAct 主循环。
    async fn handle_chat(&self, env: &Envelope) -> Value {
        let Ok(req) = serde_json::from_value::<ChatReq>(env.payload.clone()) else {
            return json!({"ok": false, "error": {"code": "K400", "message": "chat 请求需 {session_id, user_text}"}});
        };

        // 用户消息先入记忆（持久化），随后拉取全量历史
        let user_msg = MemoryMsg {
            role: "user".into(),
            content: Some(req.user_text.clone()),
            tool_calls: None,
            tool_call_id: None,
        };
        self.observe(env, &req.session_id, &[user_msg.clone()]).await;

        let mut messages: Vec<MemoryMsg> = vec![MemoryMsg {
            role: "system".into(),
            content: Some(SYSTEM_PROMPT.into()),
            tool_calls: None,
            tool_call_id: None,
        }];
        match self.perceive(env, &req.session_id).await {
            Ok(h) => messages.extend(h),
            Err(e) => {
                return json!({"ok": false, "error": {"code": e.code(), "message": format!("memory.get failed: {e}")}});
            }
        }

        // 工具清单（每请求一次；失败视为无工具可用，模型直接作答）
        let tools: Vec<ToolSpec> = match self.call(env, ID_TOOLS, json!({"op": "list"}), TOOLS_DEADLINE).await {
            Ok(v) => serde_json::from_value(v.get("tools").cloned().unwrap_or(Value::Null)).unwrap_or_default(),
            Err(e) => {
                tracing::warn!(target: ID, "tools.list failed, proceeding without tools: {e}");
                vec![]
            }
        };

        let mut rounds: u32 = 0;
        for _round in 0..self.max_rounds {
            rounds += 1;
            let resp = match self.plan(env, &messages, Some(&tools)).await {
                Ok(r) => r,
                Err(e) => {
                    return json!({"ok": false, "error": {"code": e.code(), "message": format!("llm chat failed: {e}")}});
                }
            };
            if !resp.ok {
                return json!({"ok": false, "error": resp.error.clone().unwrap_or_else(|| json!({"code":"LLM_ERROR","message":"llm-adapter 返回失败"}))});
            }
            if resp.tool_calls.is_empty() {
                return self.finish(env, &req.session_id, resp, rounds).await;
            }

            // 行动 + 观察：assistant(tool_calls) + 每个 tool 结果各一条消息
            let assistant_msg = MemoryMsg {
                role: "assistant".into(),
                content: resp.content.clone(),
                tool_calls: Some(resp.tool_calls.clone()),
                tool_call_id: None,
            };
            let mut round_msgs = vec![assistant_msg];
            for tc in &resp.tool_calls {
                let result = self.act(env, tc).await;
                round_msgs.push(MemoryMsg {
                    role: "tool".into(),
                    content: Some(result.to_string()),
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                });
            }
            self.observe(env, &req.session_id, &round_msgs).await;
            messages.extend(round_msgs);
        }

        // 轮次耗尽：最后一轮不带工具，强制收敛
        rounds += 1;
        let resp = match self.plan(env, &messages, None).await {
            Ok(r) => r,
            Err(e) => {
                return json!({"ok": false, "error": {"code": e.code(), "message": format!("llm chat failed: {e}")}});
            }
        };
        if resp.ok && resp.tool_calls.is_empty() {
            return self.finish(env, &req.session_id, resp, rounds).await;
        }
        json!({"ok": false, "error": {"code": "K502", "message": format!("agent loop exhausted max_rounds={}", self.max_rounds)}})
    }

    /// 收敛：最终答案入记忆并返回。
    async fn finish(&self, env: &Envelope, session_id: &str, resp: LlmChatResp, rounds: u32) -> Value {
        let answer = resp.content.clone().unwrap_or_default();
        self.observe(
            env,
            session_id,
            &[MemoryMsg {
                role: "assistant".into(),
                content: Some(answer.clone()),
                tool_calls: None,
                tool_call_id: None,
            }],
        )
        .await;
        json!({"ok": true, "answer": answer, "rounds": rounds, "session_id": session_id})
    }
}

#[async_trait]
impl Plugin for AgentLoopPlugin {
    fn id(&self) -> PluginId {
        self.manifest.name.clone()
    }
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    async fn init(&self, ctx: &PluginContext) -> KernelResult<()> {
        let _ = self.host.set(ctx.kernel.host.clone());
        Ok(())
    }
    async fn on_event(&self, env: Envelope) -> KernelResult<Value> {
        let op = env.payload.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            "chat" => Ok(self.handle_chat(&env).await),
            other => Ok(json!({"ok": false, "error": {"code": "K400", "message": format!("unknown op: {other}")}})),
        }
    }
    fn destroy(&self) -> KernelResult<()> {
        Ok(())
    }
}
