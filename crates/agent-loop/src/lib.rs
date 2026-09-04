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
//! 依赖（软）：`assets.registry`——不可用时降级：无技能附录、具名提示词模板不可用、
//! `load_skill` 返回字段级错误；行为与无 assets 环境一致。
//!
//! 保留名路由（03 §3）：工具调用名 `load_skill` 不进 tools 分发，由本插件路由到
//! assets `skills.load`。它不出现在 tools.list——模型可见性来自系统提示词附录。

mod contract;

pub use contract::{ChatReq, LlmChatResp, MemoryMsg, StepRecord, ToolCall, ToolSpec};

use agent_kernel_sdk::*;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const ID: &str = "agent-loop";
pub const ID_MEMORY: &str = "memory";
pub const ID_LLM: &str = "llm-adapter";
pub const ID_TOOLS: &str = "tools";
pub const ID_ASSETS: &str = "assets";

/// 保留工具名：路由 assets，不下发 tools（见 03 §3）。
pub const RESERVED_LOAD_SKILL: &str = "load_skill";

/// 保留工具名：子代理委派（Phase 3-3）——复用 agent.chat 全链路（新 session_id），不下发 tools。
pub const RESERVED_TASK: &str = "task";

/// 各转发步的相对截止（A2：Envelope.deadline 为相对时长）。
const MEM_DEADLINE: Duration = Duration::from_secs(5);
const TOOLS_DEADLINE: Duration = Duration::from_secs(60);
const LLM_DEADLINE: Duration = Duration::from_secs(120);
const ASSETS_DEADLINE: Duration = Duration::from_secs(5);

const DEFAULT_SYSTEM_PROMPT: &str = "You are a capable agent working in a workspace. \
Answer the user's request. When tools are provided and useful, call them (one batch per round); \
after receiving tool results, continue reasoning until you can produce the final answer in plain text. \
Presemble precise tool arguments: read before writing files, and prefer edit_file over rewriting whole files.";

pub struct AgentLoopPlugin {
    max_rounds: usize,
    manifest: Manifest,
    host: OnceLock<Arc<dyn HostApi>>,
    /// 委派深度（0=空闲；顶层 chat=1；子代理 chat=2）。>1 时拒绝再嵌套 task。
    depth: AtomicU32,
    /// 子会话计数（session_id 唯一性）。
    sub_counter: AtomicU64,
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
    Arc::new(AgentLoopPlugin {
        max_rounds,
        manifest,
        host: OnceLock::new(),
        depth: AtomicU32::new(0),
        sub_counter: AtomicU64::new(0),
    })
}

/// 委派深度守卫：构造时 depth+1，退出（含早退）自动 -1。
struct DepthGuard<'a>(&'a AtomicU32);
impl<'a> DepthGuard<'a> {
    fn new(d: &'a AtomicU32) -> Self {
        d.fetch_add(1, Ordering::SeqCst);
        Self(d)
    }
}
impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 路径前缀包含判断（L1 可达性探测）：分隔符归一 + Windows 大小写不敏感。
/// 尽力而为的声明级判断——真正的硬边界是文件工具的 realpath 前缀拦截。
fn path_within(child: &str, parent: &str) -> bool {
    fn norm(p: &str) -> String {
        let mut s = p.replace('/', "\\");
        while s.ends_with('\\') {
            s.pop();
        }
        s.to_ascii_lowercase()
    }
    let (c, p) = (norm(child), norm(parent));
    c != p && c.starts_with(&format!("{p}\\"))
}

/// `task` 保留工具的声明（随 tools 传给模型，使其在严格函数调用协议下可见可调）。
fn task_spec() -> ToolSpec {
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

/// 流式旁路目录（宿主以 AGENT_STREAM_DIR 下发）；未配置 → 无流式（行为同改造前）。
fn stream_dir() -> Option<PathBuf> {
    std::env::var_os("AGENT_STREAM_DIR").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// 旁路文件路径：session 名来自 URL 参数，必须过安全校验（防路径穿越）。
fn stream_file_for(session: &str) -> Option<String> {
    let dir = stream_dir()?;
    let safe = !session.is_empty()
        && session.len() <= 64
        && !session.contains("..")
        && session.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !safe {
        return None;
    }
    Some(dir.join(format!("{session}.jsonl")).to_string_lossy().into_owned())
}

/// 多轮累计用量：ReAct 一轮可能多次调用 LLM，用户要的是总消耗而非单轮。
#[derive(Default)]
struct UsageAcc {
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
    seen: bool,
}

impl UsageAcc {
    fn add(&mut self, u: Option<&Value>) {
        let Some(u) = u else { return };
        self.seen = true;
        self.input += u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.output += u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.cache_read += u.get("cache_read_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.reasoning += u.get("reasoning_tokens").and_then(Value::as_u64).unwrap_or(0);
    }

    /// provider 未上报用量时返回 Null（前端据此不显示统计条，而非显示 0）。
    fn to_value(&self) -> Value {
        if !self.seen {
            return Value::Null;
        }
        json!({
            "input_tokens": self.input,
            "output_tokens": self.output,
            "cache_read_tokens": self.cache_read,
            "reasoning_tokens": self.reasoning,
        })
    }
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

    /// 提示词组装链（07 §2.1）：env > WORKSPACE_ROOT/SYSTEM.md > PROMPT 具名模板（assets）> 内置缺省。
    async fn resolve_system_prompt(&self, src: &Envelope) -> String {
        if let Ok(s) = std::env::var("AGENT_SYSTEM_PROMPT") {
            if !s.trim().is_empty() {
                return s;
            }
        }
        if let Ok(ws) = std::env::var("WORKSPACE_ROOT") {
            if let Ok(s) = std::fs::read_to_string(std::path::Path::new(&ws).join("SYSTEM.md")) {
                if !s.trim().is_empty() {
                    return s;
                }
            }
        }
        if let Ok(name) = std::env::var("PROMPT") {
            if !name.trim().is_empty() {
                if let Ok(v) = self
                    .call(src, ID_ASSETS, json!({"op": "prompts.get", "name": name}), ASSETS_DEADLINE)
                    .await
                {
                    if let Some(c) = v.get("content").and_then(Value::as_str) {
                        if !c.trim().is_empty() {
                            return c.to_string();
                        }
                    }
                }
            }
        }
        DEFAULT_SYSTEM_PROMPT.into()
    }

    /// 技能附录（Discovery，07 §2.1）：assets 不可用/空列表 → 省略（不花 token）。
    /// skills.list 附带 root（08 §L1）：root ⊆ WORKSPACE_ROOT 时追加「技能自扩展」授权段——
    /// 模型可用 write_file 创建新技能（文件即注册表，list 每次重扫，下轮对话自动可见）。
    async fn skills_appendix(&self, src: &Envelope) -> String {
        let Ok(v) = self.call(src, ID_ASSETS, json!({"op": "skills.list"}), ASSETS_DEADLINE).await else {
            return String::new();
        };
        let Some(skills) = v.get("skills").and_then(Value::as_array) else {
            return String::new();
        };
        let root = v.get("root").and_then(Value::as_str).unwrap_or("");
        let mut lines = vec![String::new()];
        if !skills.is_empty() {
            lines.push("## Available skills".into());
            lines.push(
                "To use a skill, call the reserved tool load_skill with {\"name\": \"...\"} to load its full instructions."
                    .into(),
            );
            for s in skills {
                let name = s.get("name").and_then(Value::as_str).unwrap_or("");
                let desc = s.get("description").and_then(Value::as_str).unwrap_or("");
                if !name.is_empty() {
                    lines.push(format!("- {name}: {desc}"));
                }
            }
        }
        if let Some(section) = Self::self_extension_section(root) {
            lines.push(section);
        }
        lines.join("\n")
    }

    /// L1 技能自扩展授权段（08 §三）：仅当 skills 根目录落在 WORKSPACE_ROOT 内（模型经
    /// write_file 可物理写入）时注入。这是授权声明而非新边界——真正的硬边界仍是
    /// 文件工具的越界拦截（提示词约束≠执行边界）。
    fn self_extension_section(skills_root: &str) -> Option<String> {
        if skills_root.is_empty() {
            return None;
        }
        let ws = std::env::var("WORKSPACE_ROOT").ok()?;
        if !path_within(skills_root, &ws) {
            return None;
        }
        Some(
            "\n## Skill self-extension\n\
             You can extend your own skills: use write_file to create `<skills-root>/<name>/SKILL.md` \
             (directory name must equal the frontmatter `name`; frontmatter requires `name` and \
             `description`; the body holds execution guidance, optionally referencing `references/` files \
             you also write). New/changed skills become visible in this catalog on the NEXT chat round — \
             no reload call needed. Keep skills small and focused; invalid frontmatter is silently skipped."
                .replace("<skills-root>", skills_root)
                + "\n",
        )
    }

    /// 感知：拉取会话历史，并按 HISTORY_LIMIT 截断（保留最近 N 条）。
    async fn perceive(&self, src: &Envelope, session_id: &str) -> Result<Vec<MemoryMsg>, KernelError> {
        let v = self
            .call(src, ID_MEMORY, json!({"op": "get", "session_id": session_id}), MEM_DEADLINE)
            .await?;
        let mut msgs: Vec<MemoryMsg> =
            serde_json::from_value(v.get("messages").cloned().unwrap_or(Value::Null)).unwrap_or_default();
        if let Ok(lim) = std::env::var("HISTORY_LIMIT") {
            if let Ok(n) = lim.trim().parse::<usize>() {
                if n > 0 && msgs.len() > n {
                    msgs = msgs.split_off(msgs.len() - n);
                }
            }
        }
        Ok(msgs)
    }

    /// 压缩标记消息（与 memory 插件 summarize 的合成消息保持同构）。
    fn compaction_marker(summary: &str) -> MemoryMsg {
        MemoryMsg {
            role: "user".into(),
            content: Some(format!(
                "[Context compaction] 之前的会话历史已压缩为以下摘要：\n{summary}\n请基于该摘要与后续消息继续任务，不要声称记得被压缩的原文。"
            )),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// 事件日志（Phase 3-1，dsh：Model-visible means logged）：只追加 JSONL，
    /// 服务于审计/恢复/UI 重放。尽力而为——失败仅 debug，不阻断主流程。
    async fn trace(&self, src: &Envelope, session_id: &str, mut event: Value) {
        if let Some(obj) = event.as_object_mut() {
            obj.entry("ts".to_string()).or_insert_with(|| {
                json!(std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0))
            });
        }
        if let Err(e) = self
            .call(
                src,
                ID_MEMORY,
                json!({"op": "trace.append", "session_id": session_id, "events": [event]}),
                MEM_DEADLINE,
            )
            .await
        {
            tracing::debug!(target: ID, "trace.append failed: {e}");
        }
    }

    /// 上下文压缩（Phase 2-2，dsh：压缩是独立可选能力，不焊进 Loop 状态机）：
    /// 历史超过 COMPACT_TRIGGER（默认 40；0=禁用）时，把除最近 COMPACT_KEEP（默认 10）条
    /// 之外的旧史交 LLM 摘要，经 memory `summarize` op 持久化（含孤儿 tool 消息防撕裂），
    /// 并就地替换本轮工作集。任何失败（llm/memory）→ 降级为不压缩（warn），主流程不受影响。
    async fn maybe_compact(&self, src: &Envelope, session_id: &str, history: Vec<MemoryMsg>) -> Vec<MemoryMsg> {
        fn env_num(key: &str, default: usize) -> usize {
            std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
        }
        let trigger = env_num("COMPACT_TRIGGER", 40);
        let keep = env_num("COMPACT_KEEP", 10).min(history.len());
        if trigger == 0 || history.len() <= trigger {
            return history;
        }
        let split = history.len() - keep;
        let (older, recent) = history.split_at(split);
        let older = older.to_vec();

        // LLM 摘要旧史（不带工具）
        let mut sum_msgs: Vec<MemoryMsg> = vec![MemoryMsg {
            role: "system".into(),
            content: Some(
                "Summarize the conversation history for an AI agent. Capture: the user's goal, decisions made, \
facts learned, files/actions taken, and pending work. Be concise (<= 300 words). Output only the summary."
                    .into(),
            ),
            tool_calls: None,
            tool_call_id: None,
        }];
        sum_msgs.extend(older.iter().cloned());
        sum_msgs.push(MemoryMsg {
            role: "user".into(),
            content: Some("Summarize the above history now.".into()),
            tool_calls: None,
            tool_call_id: None,
        });
        // 压缩摘要不是用户可见输出 → 不走流式旁路
        let summary = match self.plan(src, &sum_msgs, None, None).await {
            Ok(r) if r.ok => r.content.unwrap_or_default(),
            Ok(r) => {
                tracing::warn!(target: ID, "compaction llm failed, keeping full history: {:?}", r.error);
                return history;
            }
            Err(e) => {
                tracing::warn!(target: ID, "compaction llm failed, keeping full history: {e}");
                return history;
            }
        };
        if summary.trim().is_empty() {
            tracing::warn!(target: ID, "compaction produced empty summary, keeping full history");
            return history;
        }

        // 持久化压缩（memory 侧同构：标记消息 + 最近 keep 条）
        match self
            .call(
                src,
                ID_MEMORY,
                json!({"op": "summarize", "session_id": session_id, "summary": summary, "keep_last": keep}),
                MEM_DEADLINE,
            )
            .await
        {
            Ok(v) if v.get("ok") == Some(&json!(true)) => {
                tracing::info!(target: ID, "context compacted: {} older messages summarized, kept {keep}", older.len());
                self.trace(src, session_id, json!({"type": "compaction", "summarized": older.len(), "kept": keep, "summary": summary})).await;
                let mut compacted = vec![Self::compaction_marker(&summary)];
                compacted.extend(recent.iter().cloned());
                compacted
            }
            Ok(v) => {
                tracing::warn!(target: ID, "memory.summarize rejected, keeping full history: {v}");
                history
            }
            Err(e) => {
                tracing::warn!(target: ID, "memory.summarize failed, keeping full history: {e}");
                history
            }
        }
    }

    /// 规划：调用 LLM（含/不含工具）。
    ///
    /// `stream` = Some((旁路文件绝对路径, 本轮 sid))：llm-adapter 据此在生成过程中把
    /// 增量写往该文件，宿主 tail 后经 SSE 推前端（guest 协议为 unary，插件无反向通道）。
    /// None 时行为与流式改造前完全一致。
    async fn plan(
        &self,
        src: &Envelope,
        messages: &[MemoryMsg],
        tools: Option<&[ToolSpec]>,
        stream: Option<(&str, &str)>,
    ) -> Result<LlmChatResp, KernelError> {
        let mut payload = json!({"op": "chat", "messages": messages});
        if let Some(t) = tools {
            if !t.is_empty() {
                payload["tools"] = json!(t);
            }
        }
        if let Some((path, sid)) = stream {
            payload["stream_path"] = json!(path);
            payload["sid"] = json!(sid);
        }
        let v = self.call(src, ID_LLM, payload, LLM_DEADLINE).await?;
        Ok(serde_json::from_value(v).unwrap_or(LlmChatResp {
            ok: false,
            content: None,
            tool_calls: vec![],
            model: String::new(),
            finish_reason: String::new(),
            error: Some(json!({"code": "LLM_BAD_SHAPE", "message": "llm-adapter 返回了无法解析的响应"})),
            reasoning: None,
            usage: None,
            elapsed_ms: None,
        }))
    }

    /// 行动：保留名 `task` 路由子代理（07 §2.2），`load_skill` 路由 assets（07 §2.2），
    /// 其余走 tools.exec。失败合成 ok:false 结果回喂（不中断循环）。
    /// 逐轮 tracing 事件供 host 实时回显。返回 (工具结果, 耗时ms)。
    async fn act(&self, src: &Envelope, session_id: &str, round: u32, tc: &ToolCall) -> (Value, u64) {
        let started = Instant::now();
        tracing::info!(target: "react_progress", round, tool = %tc.name, "▶ round {round}: {}", tc.name);
        self.trace(src, session_id, json!({"type": "tool_call", "round": round, "name": tc.name, "args": tc.arguments}))
            .await;
        let result = if tc.name == RESERVED_TASK {
            // 子代理委派（Phase 3-3）：全新会话复用 agent.chat 全链路，仅回传最终答案
            let task_text = tc.arguments.get("task").and_then(Value::as_str).unwrap_or("");
            if task_text.trim().is_empty() {
                json!({"ok": false, "error": {"code": "K400", "field": "task", "message": "task 工具需非空参数 {\"task\": str}（子任务自包含描述）"}})
            } else {
                self.run_subagent(src, session_id, task_text).await
            }
        } else {
            let (payload, target, deadline) = if tc.name == RESERVED_LOAD_SKILL {
                let name = tc.arguments.get("name").and_then(Value::as_str).unwrap_or("");
                (json!({"op": "skills.load", "name": name}), ID_ASSETS, ASSETS_DEADLINE)
            } else {
                (json!({"op": "call", "name": tc.name, "args": tc.arguments}), ID_TOOLS, TOOLS_DEADLINE)
            };
            match self.call(src, target, payload, deadline).await {
                Ok(v) => v,
                Err(e) => json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
            }
        };
        let ms = started.elapsed().as_millis() as u64;
        tracing::info!(target: "react_progress", round, tool = %tc.name, ms, "✓ round {round}: {} ({}ms)", tc.name, ms);
        // 事件日志：结果截断（防大输出撑爆审计文件），完整结果仍在 memory 会话消息里
        let result_str = result.to_string();
        let truncated: String = result_str.chars().take(2000).collect();
        self.trace(
            src,
            session_id,
            json!({
                "type": "tool_result", "round": round, "name": tc.name, "ms": ms,
                "ok": result.get("ok") == Some(&json!(true)),
                "result_truncated": truncated,
                "result_full_in_memory": true,
            }),
        )
        .await;
        (result, ms)
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

    /// 子代理（Phase 3-3）：新 session_id 复用 agent.chat 全链路（提示词组装/记忆/工具/事件日志）。
    /// 深度 > 1（子代理内再委派）→ 字段级拒绝；子会话事件日志独立（session_id 关联可追溯）。
    async fn run_subagent(&self, src: &Envelope, parent_session: &str, task_text: &str) -> Value {
        if self.depth.load(Ordering::SeqCst) >= 2 {
            return json!({"ok": false, "error": {"code": "K400", "message": "task 不支持嵌套委派（子代理内不可再调用 task）"}});
        }
        let n = self.sub_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let sub_session = format!("{parent_session}#sub-{n}");
        self.trace(src, parent_session, json!({"type": "subagent", "sub_session": sub_session, "task": task_text}))
            .await;
        let env = Envelope::new(
            PluginId::new(ID),
            json!({"op": "chat", "session_id": sub_session, "user_text": task_text}),
        );
        // 递归委派：Box::pin 打断未来大小的无限递归（深度由 DepthGuard 硬限）
        let resp = Box::pin(self.handle_chat(&env)).await;
        if resp.get("ok") == Some(&json!(true)) {
            json!({
                "ok": true,
                "answer": resp.get("answer").cloned().unwrap_or(Value::Null),
                "sub_session": sub_session,
            })
        } else {
            json!({"ok": false, "error": {"code": "SUBAGENT_FAILED", "message": format!("子代理失败: {resp}")}})
        }
    }

    /// ReAct 主循环。深度守卫包裹整个循环体（含子代理递归）。
    async fn handle_chat(&self, env: &Envelope) -> Value {
        let _guard = DepthGuard::new(&self.depth);
        self.chat_body(env).await
    }

    async fn chat_body(&self, env: &Envelope) -> Value {
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
        self.trace(env, &req.session_id, json!({"type": "user", "text": req.user_text})).await;

        // 系统提示词 = 组装链（07 §2.1）+ 技能附录（软）
        let system = format!("{}{}", self.resolve_system_prompt(env).await, self.skills_appendix(env).await);
        let mut messages: Vec<MemoryMsg> = vec![MemoryMsg {
            role: "system".into(),
            content: Some(system),
            tool_calls: None,
            tool_call_id: None,
        }];
        match self.perceive(env, &req.session_id).await {
            Ok(h) => messages.extend(self.maybe_compact(env, &req.session_id, h).await),
            Err(e) => {
                self.trace(env, &req.session_id, json!({"type": "error", "where": "memory.get", "message": e.to_string()})).await;
                return json!({"ok": false, "error": {"code": e.code(), "message": format!("memory.get failed: {e}")}});
            }
        }

        // 工具清单（每请求一次；失败视为无工具可用，模型直接作答）+ 保留名 task 声明
        let mut tools: Vec<ToolSpec> = match self.call(env, ID_TOOLS, json!({"op": "list"}), TOOLS_DEADLINE).await {
            Ok(v) => serde_json::from_value(v.get("tools").cloned().unwrap_or(Value::Null)).unwrap_or_default(),
            Err(e) => {
                tracing::warn!(target: ID, "tools.list failed, proceeding without tools: {e}");
                vec![]
            }
        };
        tools.push(task_spec());

        let mut steps: Vec<StepRecord> = Vec::new();
        let mut rounds: u32 = 0;
        // 流式旁路：宿主以 AGENT_STREAM_DIR 下发目录；未配置即退化为一问一答（行为同改造前）。
        let stream_path = stream_file_for(&req.session_id);
        let mut usage = UsageAcc::default();
        let mut llm_ms: u64 = 0;
        for _round in 0..self.max_rounds {
            rounds += 1;
            let sid = format!("{}-r{}", req.session_id, rounds);
            let stream = stream_path.as_deref().map(|p| (p, sid.as_str()));
            let resp = match self.plan(env, &messages, Some(&tools), stream).await {
                Ok(r) => r,
                Err(e) => {
                    self.trace(env, &req.session_id, json!({"type": "error", "where": "llm.chat", "message": e.to_string()})).await;
                    return json!({"ok": false, "error": {"code": e.code(), "message": format!("llm chat failed: {e}")}});
                }
            };
            usage.add(resp.usage.as_ref());
            llm_ms += resp.elapsed_ms.unwrap_or(0);
            if !resp.ok {
                let err_val = resp.error.clone().unwrap_or_else(|| json!({"code":"LLM_ERROR","message":"llm-adapter 返回失败"}));
                let emsg = err_val
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "llm-adapter 返回失败".into());
                self.trace(env, &req.session_id, json!({"type": "error", "where": "llm.chat", "message": emsg})).await;
                return json!({"ok": false, "error": err_val});
            }
            if resp.tool_calls.is_empty() {
                return self.finish(env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms).await;
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
                let (result, ms) = self.act(env, &req.session_id, rounds, tc).await;
                steps.push(StepRecord { round: rounds, tool: tc.name.clone(), ms });
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
        let sid = format!("{}-r{}", req.session_id, rounds);
        let stream = stream_path.as_deref().map(|p| (p, sid.as_str()));
        let resp = match self.plan(env, &messages, None, stream).await {
            Ok(r) => r,
            Err(e) => {
                self.trace(env, &req.session_id, json!({"type": "error", "where": "llm.chat", "message": e.to_string()})).await;
                return json!({"ok": false, "error": {"code": e.code(), "message": format!("llm chat failed: {e}")}});
            }
        };
        usage.add(resp.usage.as_ref());
        llm_ms += resp.elapsed_ms.unwrap_or(0);
        if resp.ok && resp.tool_calls.is_empty() {
            return self.finish(env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms).await;
        }
        self.trace(env, &req.session_id, json!({"type": "error", "where": "max_rounds", "message": format!("agent loop exhausted max_rounds={}", self.max_rounds)})).await;
        json!({"ok": false, "error": {"code": "K502", "message": format!("agent loop exhausted max_rounds={}", self.max_rounds)}})
    }

    /// 收敛：最终答案入记忆并返回（含 steps）。
    ///
    /// 事件带上 sid（与流式增量对位，供前端复用同一气泡）、reasoning、累计 usage 与
    /// LLM 总耗时。流式增量只经旁路文件实时外抛，不落日志；这条事件才是持久化与
    /// 刷新恢复的唯一依据，因此内容必须与流式所见一致（含思考）。
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        env: &Envelope,
        session_id: &str,
        resp: LlmChatResp,
        rounds: u32,
        steps: Vec<StepRecord>,
        sid: String,
        usage: UsageAcc,
        llm_ms: u64,
    ) -> Value {
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
        let reasoning = resp.reasoning.clone();
        self.trace(
            env,
            session_id,
            json!({
                "type": "assistant",
                "answer": answer,
                "rounds": rounds,
                "sid": sid,
                "reasoning": reasoning,
                "usage": usage.to_value(),
                "elapsed_ms": llm_ms,
            }),
        )
        .await;
        json!({"ok": true, "answer": answer, "rounds": rounds, "steps": steps, "session_id": session_id})
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_within_prefix_and_case_insensitive() {
        // 直接子目录
        assert!(path_within(r"C:\ws\skills", r"C:\ws"));
        // 混合分隔符
        assert!(path_within("C:/ws/skills/x", r"C:\WS"));
        // 同目录不算 within（skills 根 = workspace 根时拒绝授权）
        assert!(!path_within(r"C:\ws", r"C:\ws"));
        // 仅前缀字符串相同但非目录边界
        assert!(!path_within(r"C:\ws2\skills", r"C:\ws"));
        // 完全无关
        assert!(!path_within(r"D:\other\skills", r"C:\ws"));
    }

    #[test]
    fn self_extension_section_gated_by_workspace_reachability() {
        let saved = std::env::var("WORKSPACE_ROOT").ok();
        std::env::set_var("WORKSPACE_ROOT", r"C:\ws");

        // root 为空 → 不授权
        assert!(AgentLoopPlugin::self_extension_section("").is_none());
        // skills 根在 workspace 外 → 不授权
        assert!(AgentLoopPlugin::self_extension_section(r"D:\elsewhere\skills").is_none());
        // skills 根在 workspace 内 → 授权段含路径与 write_file 指引
        let section = AgentLoopPlugin::self_extension_section(r"C:\ws\skills").expect("in-workspace");
        assert!(section.contains(r"C:\ws\skills"));
        assert!(section.contains("write_file"));
        assert!(section.contains("Skill self-extension"));

        match saved {
            Some(v) => std::env::set_var("WORKSPACE_ROOT", v),
            None => std::env::remove_var("WORKSPACE_ROOT"),
        }
    }
}
