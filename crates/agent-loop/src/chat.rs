//! ReAct 主循环与 LLM 规划（R12 拆分自 lib.rs）。
//!
//! 职责：`chat_body` / `chat_run`（主状态机循环）、`finish`（收敛）、`perceive` /
//! `observe`（记忆读写）、`plan` / `plan_with_retry`（LLM 调用 + T3 瞬态重试 +
//! P8 超限降级）、`max_rounds`（生效轮次上限）、用户消息装配（`build_user_msg` /
//! `text_attach_limit` / `base64_decode_utf8`）与历史窗口（`apply_history_limit`）。

use super::budget::{is_transient_kernel_err, is_transient_llm_error, ChatBudget, StopReason, UsageAcc, WRAP_UP_DEADLINE};
use super::config::AgentLoopConfig;
use super::subagent::task_spec;
use super::tools_exec::truncate_chars;
use super::trace::stream_file_for;
use super::*;
use std::time::Instant;

/// R3：把用户附件装配进 user 消息。
///
/// - 图片（mime 以 image/ 开头）→ 结构化 `attachments` 字段，由 llm-adapter 按
///   provider 协议映射（OpenAI 兼容 image_url data URI / ollama native images 数组）；
/// - 文本文件 → 直接拼入 content（一次内嵌，超限截断并标注）——provider 无需感知；
/// - 其他二进制类型 → 不内嵌内容，content 中注明名称与类型（不静默丢弃）。
pub(super) fn build_user_msg(
    user_text: &str,
    attachments: Option<&[Attachment]>,
    text_attach_limit: usize,
) -> MemoryMsg {
    let Some(list) = attachments else {
        return MemoryMsg {
            role: "user".into(),
            content: Some(user_text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        };
    };
    let limit = text_attach_limit;
    let mut content = String::from(user_text);
    let mut images: Vec<Attachment> = Vec::new();
    for a in list {
        if a.mime.starts_with("image/") {
            images.push(a.clone());
            continue;
        }
        if a.mime.starts_with("text/") || a.mime == "application/json" {
            let decoded = base64_decode_utf8(&a.data_b64);
            let body = truncate_chars(&decoded, limit);
            content.push_str(&format!("\n\n[附件: {}]\n```\n{}\n```", a.name, body));
            continue;
        }
        content.push_str(&format!("\n\n[附件: {}]（类型 {}，内容未内嵌）", a.name, a.mime));
    }
    MemoryMsg {
        role: "user".into(),
        content: Some(content),
        tool_calls: None,
        tool_call_id: None,
        attachments: if images.is_empty() { None } else { Some(images) },
    }
}

/// 裸 base64 → UTF-8 字符串（尽力而为：解码失败按原样透传，由模型侧感知）。
pub(super) fn base64_decode_utf8(b64: &str) -> String {
    use base64::Engine as _;
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    match base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => b64.to_string(),
    }
}

/// 感知窗口（PLAN R3）：HISTORY_LIMIT 只裁剪**发给 LLM 的工作集**，且必须发生在
/// 压缩判断之后——修复前截断在前，LIMIT < TRIGGER 时压缩永不触发（memory 无限增长
/// 且旧史被静默丢弃）。
pub(super) fn apply_history_limit(history_limit: usize, mut msgs: Vec<MemoryMsg>) -> Vec<MemoryMsg> {
    if history_limit > 0 && msgs.len() > history_limit {
        msgs = msgs.split_off(msgs.len() - history_limit);
    }
    msgs
}

impl AgentLoopPlugin {
    /// 感知：拉取会话**全量**历史（不做窗口裁剪）。
    /// 窗口与压缩的顺序见 `apply_history_limit`（PLAN R3：先压缩判断，后窗口裁剪）。
    pub(super) async fn perceive(&self, src: &Envelope, session_id: &str) -> Result<Vec<MemoryMsg>, KernelError> {
        let v = self
            .call(src, CAP_MEMORY, json!({"op": "get", "session_id": session_id}), MEM_DEADLINE)
            .await?;
        let msgs: Vec<MemoryMsg> =
            serde_json::from_value(v.get("messages").cloned().unwrap_or(Value::Null)).unwrap_or_default();
        Ok(msgs)
    }

    /// 规划：调用 LLM（含/不含工具）。
    ///
    /// `stream` = Some((旁路文件绝对路径, 本轮 sid))：llm-adapter 据此在生成过程中把
    /// 增量写往该文件，宿主 tail 后经 SSE 推前端（guest 协议为 unary，插件无反向通道）。
    /// None 时行为与流式改造前完全一致。
    pub(super) async fn plan(
        &self,
        src: &Envelope,
        messages: &[MemoryMsg],
        tools: Option<&[ToolSpec]>,
        stream: Option<(&str, &str)>,
        cfg: &AgentLoopConfig,
    ) -> Result<LlmChatResp, KernelError> {
        let mut payload = json!({"op": "chat", "messages": messages});
        // L2+L3：`LLM_CONTEXT_TOKENS` 语义为「上下文窗口」——随 payload 透传 num_ctx，
        // ollama native 映射 options.num_ctx（本地估算闸与服务端窗口对齐，一处配置两侧生效）。
        // L7：仅本地窗口型 provider 生效（config::from_env 内部判定 LLM_PROVIDER，
        // 云端 API / 非名单 provider 返回 0 = 不下发且 token 闸禁用——避免为本地调小的窗口
        // 误压云端历史）。注意 provider 取自 host 启动时 env：Web 热切换 provider 后本判定
        // 滞后，重启校正（provider 切换低频，可接受）。
        let window = cfg.context_window;
        if window > 0 {
            payload["num_ctx"] = json!(window);
        }
        if let Some(t) = tools {
            if !t.is_empty() {
                payload["tools"] = json!(t);
            }
        }
        if let Some((path, sid)) = stream {
            payload["stream_path"] = json!(path);
            payload["sid"] = json!(sid);
        }
        let v = self.call(src, CAP_LLM, payload, llm_step_deadline()).await?;
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

    /// 规划 + 重试（T3 + P8）。消息按 `&mut Vec` 传入：P8 降级时在原缓冲上收缩窗口，
    /// 调用方（chat_run）随即可见，不引入每轮克隆。
    ///
    /// - T3 瞬态重试：限流/超时/网关类失败按指数退避重试（base×2^n，单次封顶 8s），
    ///   成功或确定性失败立即返回；重试经 trace 落审计日志（type=retry）。
    /// - P8 超限降级（R7/R8）：provider 侧 `CONTEXT_OVERFLOW`（确定性但可行动，估算闸漏网
    ///   时的 provider 终审）→ 未降级过则窗口/额度减半（`context::degrade`）重试一次；
    ///   再超 → 原错误收敛，不进入重试风暴。与 T3 正交：一个接瞬态退避，一个接超限降级。
    pub(super) async fn plan_with_retry(
        &self,
        src: &Envelope,
        session_id: &str,
        messages: &mut Vec<MemoryMsg>,
        tools: Option<&[ToolSpec]>,
        stream: Option<(&str, &str)>,
        cfg: &AgentLoopConfig,
        retries: &mut u32,
    ) -> Result<LlmChatResp, KernelError> {
        let attempts = cfg.retry_attempts;
        let base = cfg.retry_base_ms;
        let mut attempt: u32 = 0;
        let mut degraded = false; // P8：本轮 chat 是否已做过超限降级（只做一次）
        loop {
            let res = self.plan(src, messages, tools, stream, cfg).await;
            if !degraded {
                let overflow = matches!(&res, Ok(r) if
                    r.error.as_ref().and_then(|e| e.get("code")).and_then(Value::as_str) == Some("CONTEXT_OVERFLOW"));
                if overflow {
                    degraded = true;
                    let taken = std::mem::take(messages);
                    *messages = context::degrade(taken, cfg);
                    tracing::warn!(target: ID, session = %session_id, "llm context overflow, halving window/limits and retrying once");
                    self.trace(
                        src,
                        session_id,
                        json!({"type": "retry", "where": "llm.chat", "reason": "CONTEXT_OVERFLOW", "degraded": true}),
                    )
                    .await;
                    continue;
                }
            }
            let transient = match &res {
                Err(e) => is_transient_kernel_err(e),
                Ok(r) => !r.ok && r.error.as_ref().map(is_transient_llm_error).unwrap_or(false),
            };
            if !transient || attempt >= attempts {
                return res;
            }
            let delay = base.saturating_mul(1u64 << attempt.min(4)).min(8000);
            let reason = match &res {
                Err(e) => e.to_string(),
                Ok(r) => r.error.as_ref().map(|e| e.to_string()).unwrap_or_default(),
            };
            tracing::warn!(target: ID, session = %session_id, attempt, delay, "llm transient failure, retrying: {reason}");
            *retries += 1;
            self.trace(
                src,
                session_id,
                json!({"type": "retry", "where": "llm.chat", "attempt": attempt + 1, "delay_ms": delay, "reason": reason}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(delay)).await;
            attempt += 1;
        }
    }

    /// 观察：写入记忆。返回是否成功——LOOP-P2/A2：append 失败意味着持久层与内存工作集
    /// 开始分叉（下一轮 perceive 拉到的历史缺这段），继续跑只会放大错位。由调用方在
    /// 关键写入点（开局用户消息 / 每轮工具结果）按连续失败计数熔断；提醒与最终答案
    /// 仍尽力而为（失败只告警——在最终答案上熔断会把已生成的答案整个丢掉，更糟）。
    pub(super) async fn observe(&self, src: &Envelope, session_id: &str, msgs: &[MemoryMsg]) -> bool {
        match self
            .call(src, CAP_MEMORY, json!({"op": "append", "session_id": session_id, "messages": msgs}), MEM_DEADLINE)
            .await
        {
            Ok(_) => {
                self.append_fails.store(0, Ordering::SeqCst);
                true
            }
            Err(e) => {
                let n = self.append_fails.fetch_add(1, Ordering::SeqCst) + 1;
                tracing::warn!(target: ID, "memory append failed（连续 {n} 次）: {e}");
                false
            }
        }
    }

    /// ReAct 主循环。委派深度由 `ChatReq.depth` 随链携带（0=顶层），
    /// 不使用插件级共享计数——并发下各链深度互不挤占。
    pub(super) async fn chat_body(&self, env: &Envelope) -> Value {
        let Ok(req) = serde_json::from_value::<ChatReq>(env.payload.clone()) else {
            return json!({"ok": false, "error": {"code": "K400", "message": "chat 请求需 {session_id, user_text}"}});
        };
        // 开局清残留取消标志（chat 结束时也会清理——双保险，防误杀同 session 下轮对话）
        self.cancels.lock().unwrap().remove(&req.session_id);
        // 同会话并发闸（LOOP-P2/A1）：拿不到闸 = 该 session 已有进行中的 chat。直接 409
        // 而非排队——排队会把两次对话串成隐式顺序，用户在另一窗口看到的还是错乱。
        let gate = {
            let mut m = self.session_chats.lock().unwrap();
            m.entry(req.session_id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _permit = match gate.try_lock() {
            Ok(p) => p,
            Err(_) => {
                return json!({"ok": false, "error": {"code": "K409", "message": "该会话已有进行中的对话，拒绝并发 chat（会互踩取消/流式/回滚定位）；请等待完成或先停止当前回合"}});
            }
        };
        let out = self.chat_run(env, &req).await;
        // G 轮：回合交接（回填 + 终结）。
        //
        // 为什么放在包装层而不是 chat_run 内部：`chat_run` 有 20 个 return 出口
        // （取消 / 预算硬停 / context 溢出 / LLM 失败 / 空答案 / 轮次耗尽…），
        // 逐口补终结既容易漏，也会让每个出口都变长。这里只有一个出入口，
        // 且 `chat_run` 已经把暂停写进了 turn 文件——包装层**读文件**即得真相，
        // 不必让两个函数共享可变状态。
        let mut out = out;
        // 本轮事件已由 chat_run 全部写完，现在读到的 `next` 就是本轮终点。
        // 失败按 0 → 退化为 `start_cursor`（补播会少给，但不会给错）。
        let trace_next = self
            .call(env, CAP_MEMORY, json!({"op": "trace.read", "session_id": req.session_id, "after": 0}), MEM_DEADLINE)
            .await
            .ok()
            .and_then(|v| v.get("next").and_then(Value::as_u64));
        let turn_meta = (|| {
            let store = turn::TurnStore::open()?;
            // chat_run 里建的 turn 是**最新一条非终结态**；按 started_at 倒序取第一条即可
            // （同一会话有并发闸，不会有第二条 running）。
            let turn_id = out
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    store
                        .list_session(&req.session_id)
                        .into_iter()
                        .find(|t| !turn::is_terminal(t.get("status").and_then(Value::as_str).unwrap_or("")))
                        .and_then(|t| t.get("turn_id").and_then(Value::as_str).map(str::to_string))
                })?;
            let mut turn = store.load(&turn_id)?;
            let paused = out.get("paused").and_then(Value::as_bool) == Some(true)
                || turn.get("status").and_then(Value::as_str) == Some("paused");
            if !paused {
                let ok = out.get("ok").and_then(Value::as_bool) == Some(true);
                let end_cursor = trace_next
                    .unwrap_or_else(|| turn.get("end_cursor").and_then(Value::as_u64).unwrap_or(0));
                turn::mark_terminal(&mut turn, if ok { "done" } else { "failed" }, end_cursor);
                store.save(&turn);
            }
            Some(turn)
        })();
        if out.is_object() {
            match turn_meta {
                Some(t) => {
                    out["turn_id"] = t.get("turn_id").cloned().unwrap_or(Value::Null);
                    out["turn_status"] = t.get("status").cloned().unwrap_or(Value::Null);
                    out["start_cursor"] = t.get("start_cursor").cloned().unwrap_or(Value::Null);
                    out["end_cursor"] = t.get("end_cursor").cloned().unwrap_or(Value::Null);
                    if let Some(p) = t.get("pending").cloned().filter(|p| !p.is_null()) {
                        out["pending"] = p;
                    }
                }
                // 回合持久化不可用（无写权限/无目录）：**不加字段**。加 null 会让前端
                // 以为「这轮没有 id」，而真相是「这套部署不支持补播」——两者处置不同。
                None => {}
            }
        }
        self.cancels.lock().unwrap().remove(&req.session_id);
        // 空间卫生（A4）：会话级瞬态表逐出，防长驻进程无界增长。
        // turn_starts：fresh_artifact 仅回合内消费，chat 结束即失效可清；
        // sid_seq：逐出后 next_sid 以全局 sid_floor 下限重新播种（严格大于历史已发值），
        // 跨回合 sid 不碰撞语义不变。
        self.turn_starts.lock().unwrap().remove(&req.session_id);
        self.sid_seq.lock().unwrap().remove(&req.session_id);
        // 会话闸卫生（LOOP-P2/A1）：strong_count==2（表 + 本链 Arc）= 没有并发等待者，
        // 安全逐出；有等待者时留着，由最后一链的出口清理（等待中的请求自带 Arc 克隆）。
        if Arc::strong_count(&gate) == 2 {
            self.session_chats.lock().unwrap().remove(&req.session_id);
        }
        drop(_permit);
        out
    }

    pub(super) async fn chat_run(&self, env: &Envelope, req: &ChatReq) -> Value {
        // E3：运行参数快照（每次 chat 解析一次；Web 热通道语义不变——改 env 下轮生效）。
        let cfg = AgentLoopConfig::from_env(self.max_rounds);
        // 回合开始时间：兜底产物扫描的新鲜度基准（见 turn_starts 字段注释）
        self.turn_starts
            .lock()
            .unwrap()
            .insert(req.session_id.clone(), std::time::SystemTime::now());
        // G 轮：回合对象。一次 chat 不再只是「一次阻塞调用」——它有一条可寻址的记录
        // （`.rsi/turns/<turn_id>.json`），带 trace 游标区间。补播只读这一轮，
        // 暂停则由本记录承接用户回答（三条边界见 turn.rs 模块注释）。
        //
        // `start_cursor` 取 `trace.read(after=0)` 的 `next`（= 已有事件条数）。
        // 拿不到就按 0：补播会多给一些旧事件但不会漏——错在多给一侧用户能自己扫过去，
        // 错在少给一侧就是静默丢内容。
        let turn_store = turn::TurnStore::open();
        let start_cursor = match self
            .call(env, CAP_MEMORY, json!({"op": "trace.read", "session_id": req.session_id, "after": 0}), MEM_DEADLINE)
            .await
        {
            Ok(v) => v.get("next").and_then(Value::as_u64).unwrap_or(0),
            Err(_) => 0,
        };
        // 复用还是新建：给了 turn_id 且归属同会话才复用（防跨会话串回合）；
        // 否则新建。归属校验不是洁癖——turn_id 来自前端，不校验就能读到别人的回合。
        let mut turn = match req
            .turn_id
            .as_deref()
            .filter(|id| turn::valid_turn_id(id))
            .and_then(|id| turn_store.as_ref()?.load(id))
        {
            Some(t) if t.get("session_id").and_then(Value::as_str) == Some(req.session_id.as_str()) => t,
            _ => turn::new_turn(&turn::new_turn_id(&req.session_id), &req.session_id, start_cursor),
        };
        // 暂停中的回答：先把 turn 从 paused 接回 running（拿到 tool_call_id），再跑。
        // 接不回（状态不是 paused）= 确定性 K409——静默放行会让「回复了却没接上」
        // 表现为「模型又开始重新规划」，那种最难查。
        if let Some(rep) = req.tool_reply.as_ref() {
            if turn::apply_reply(&mut turn, &rep.answer).is_none() {
                return json!({"ok": false, "error": {"code": "K409",
                    "message": format!("该回合当前状态为 '{}'，不接受回复（只有 paused 可接）",
                        turn.get("status").and_then(Value::as_str).unwrap_or("?"))}});
            }
        }
        if let Some(s) = turn_store.as_ref() {
            s.save(&turn);
        }
        // 用户消息先入记忆（持久化），随后拉取全量历史。
        // R3：文本附件拼入 content，图片附件走结构化字段（llm-adapter 按 provider 映射）。
        let user_msg = build_user_msg(&req.user_text, req.attachments.as_deref(), cfg.text_attach_limit);
        // LOOP-P2/A2：开局用户消息都落不了盘 = 会话记录已断档，本轮跑完下一轮也接不回
        // ——立即熔断（K503），不烧 LLM token。
        if !self.observe(env, &req.session_id, &[user_msg.clone()]).await {
            return json!({"ok": false, "error": {"code": "K503", "message": "memory.append 失败，已熔断本轮（持久层与工作集会静默分叉）；请检查 memory 插件后重试"}});
        }
        // trace 带全量原始附件（含文本 b64）：UI 重放展示缩略图/文件条，重新生成需原始 data_b64 重发
        let mut user_event = json!({"type": "user", "text": req.user_text});
        if let Some(att) = req.attachments.as_deref() {
            user_event["attachments"] = json!(att);
        }
        self.trace(env, &req.session_id, user_event).await;

        // 系统提示词 = 组装链（07 §2.1）+ 技能附录（软）
        // B2：技能自扩展长教程按需注入——仅当本轮用户文本明确要求造技能/打包工具。
        let want_skill_build = Self::user_wants_skill_build(&req.user_text);
        let system = format!(
            "{}{}",
            self.resolve_system_prompt(env, &cfg).await,
            self.skills_appendix(env, &cfg, want_skill_build).await
        );
        let mut messages: Vec<MemoryMsg> = vec![MemoryMsg {
            role: "system".into(),
            content: Some(system),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        }];
        match self.perceive(env, &req.session_id).await {
            Ok(h) => {
                // 双闸顺序（PLAN R3）：先对全量历史做压缩判断（超 TRIGGER 则摘要落盘），
                // 再对（可能已压缩的）工作集应用 HISTORY_LIMIT 窗口。
                let compacted = self.maybe_compact(env, &req.session_id, h, &cfg).await;
                messages.extend(apply_history_limit(cfg.history_limit, compacted));
            }
            Err(e) => {
                self.trace(env, &req.session_id, json!({"type": "error", "where": "memory.get", "message": e.to_string()})).await;
                return json!({"ok": false, "error": {"code": e.code(), "message": format!("memory.get failed: {e}")}});
            }
        }
        // G 轮：暂停回答注入。用户答的那句话作为一条 user 消息进工作集——模型看到的是
        // 「我问了、用户答了」，而不是一个孤立的 tool 结果（后者会让它重新规划、
        // 把已有进度丢掉）。这就是「暂停而不是结束」与「结束再开新回合」的区别所在。
        //
        // 用 `req.tool_reply` 的存在与否判定，而不是看 turn 状态：turn 已在
        // chat_body 里被接回 running（apply_reply 把它改了），这里判状态就永远为假。
        if let Some(rep) = req.tool_reply.as_ref() {
            let pending_q = turn::TurnStore::open()
                .and_then(|s| req.turn_id.as_deref().and_then(|id| s.load(id)))
                .and_then(|t| {
                    t.get("pending")
                        .and_then(|p| p.get("question"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let msg = MemoryMsg {
                role: "user".into(),
                content: Some(format!(
                    "[用户回答（回合 {} 暂停中提问）]\n问题: {}\n回答: {}",
                    req.turn_id.as_deref().unwrap_or(""),
                    pending_q,
                    rep.answer
                )),
                tool_calls: None,
                tool_call_id: None,
                attachments: None,
            };
            // 两处都要写，缺一不可：
            // ① memory（持久层）——否则下一轮历史缺这段；
            // ② `messages`（本轮工作集）——第 ① 步只影响**下一次** perceive，
            //    本轮模型根本看不到它。原实现只做了 ①，端到端用例实测
            //    「回答消失」，模型于是重新规划。
            if !self.observe(env, &req.session_id, &[msg.clone()]).await {
                tracing::warn!(target: ID, session = %req.session_id, "暂停回答落库失败，本轮继续但历史会缺这段");
            }
            messages.push(msg);
        }

        // 工具清单（每请求一次；失败视为无工具可用，模型直接作答）+ 保留名 task 声明
        let mut tools: Vec<ToolSpec> = match self.call(env, CAP_TOOLS, json!({"op": "list"}), tools_deadline()).await {
            Ok(v) => serde_json::from_value(v.get("tools").cloned().unwrap_or(Value::Null)).unwrap_or_default(),
            Err(e) => {
                tracing::warn!(target: ID, "tools.list failed, proceeding without tools: {e}");
                vec![]
            }
        };
        tools.push(task_spec());

        // R9 清单组装：tools = 内置启用集 ∪ 当前会话已加载技能的已启用技能工具。
        // 会话技能集从 trace 重放推导（skill_loaded 事件）；失败降级为空不阻断主流程。
        let mut loaded_skills = self.trace_loaded_skills(env, &req.session_id).await;
        if !loaded_skills.is_empty() {
            // R18（重启恢复）：技能工具池是 tools 侧进程内存态（装载≠启用，重启即空）——
            // 重放恢复的作用域若不重装，skill_tools 空手而归、模型看不到已加载技能的工具。
            // 此处按注册声明静默重装（assets.load → tools.install，幂等：同技能覆写入池、
            // preset 装载即启用、用户技能经 TOOLS_ENABLED 延迟启用还原）；不重发
            // skill_loaded/skill_installed 事件（作用域已由重放恢复，避免 trace/前端刷屏）。
            // 技能已不可用（被删/改名）→ 装载自然失败，清单装配为空，不报错打断主流程。
            for name in &loaded_skills {
                match self
                    .call(env, CAP_ASSETS, json!({"op": "skills.load", "name": name}), ASSETS_DEADLINE)
                    .await
                {
                    Ok(v) if v.get("ok") == Some(&json!(true)) => {
                        let _ = self.install_skill_tools(env, name, &v).await;
                    }
                    _ => {}
                }
            }
            tools.extend(self.session_skill_tools(env, &loaded_skills).await);
        }

        // G 轮：按需装载（deferred）分流。工具 schema 随**每一轮**请求重发，MCP 接一台
        // server 就是十几件工具的完整 JSON Schema，而单轮真正用得上的通常只有 1-2 件。
        // 此处把 deferred 工具挡在下发清单之外（`visible`），只留一行名字摘要进系统提示词；
        // 模型调 load_tools 报名后，该工具的完整 schema 自下一轮起进入 `visible`。
        //
        // 为什么在技能工具装配**之后**做：`visible` 要一次建全，否则已加载技能的工具
        // 会漏出清单（「装了却看不见」是本仓库踩过的老坑）。
        // 为什么只挡 MCP：技能工具通常 1-3 件且刚 load_skill 就期待可见；MCP 才是
        // 「多而少用」的形态。默认策略在 tools guest 侧声明（见 tools_plugin.py）。
        let deferred_names: Vec<String> = tools
            .iter()
            .filter(|t| t.deferred == Some(true))
            .map(|t| t.name.clone())
            .collect();
        let mut visible: Vec<ToolSpec> = tools
            .iter()
            .filter(|t| t.deferred != Some(true))
            .cloned()
            .collect();
        if !deferred_names.is_empty() {
            let spec = tools_defer::load_tools_spec();
            tools.push(spec.clone());
            visible.push(spec);
            // 系统提示词在上面已经 format! 完，故此处追加而不重排构造顺序——改动面最小，
            // 且摘要天然落在提示词末尾（紧邻工具区，模型的注意力位置更合理）。
            let line = tools_defer::deferred_line(&deferred_names);
            if let Some(sys) = messages.first_mut().and_then(|m| m.content.as_mut()) {
                sys.push_str(&line);
            }
        }
        // G 轮：ask_user 保留工具随清单下发（模型才看得见可调）。
        //
        // 门控 = 回合持久化可用：拿不到 turns 目录时暂停无处落盘，给了这个工具就是
        // 给一个承诺不了的承诺（模型暂停后用户回复会被丢弃，且没有任何报错）。
        if turn::TurnStore::open().is_some() {
            let spec = turn::ask_user_spec();
            tools.push(spec.clone());
            visible.push(spec);
        }
        // G 轮：本轮是否命中 ask_user（问题文本 + 那条调用的 id）。None = 正常跑。
        let mut paused_by_ask: Option<(String, String)> = None;

        let mut steps: Vec<StepRecord> = Vec::new();
        let mut rounds: u32 = 0;
        // RSI 策略钩子状态（仅 `self.policy` 开启时生效；提醒各只发一次，防死循环）。
        let policy = self.policy;
        let mut acceptance_seen = false;
        let mut contract_reminded = false;
        let mut verify_reminded = false;
        // W9.4：逐轮思考留痕——流式旁路每轮覆写只留末轮，刷新重放看不到中间轮思考。
        // 各轮 reasoning 收集于此，随最终 assistant 事件持久化（轮次 + 文本）。
        let mut round_reasonings: Vec<(u32, String)> = Vec::new();
        // 流式旁路：宿主以 AGENT_STREAM_DIR 下发目录；未配置即退化为一问一答（行为同改造前）。
        let stream_path = stream_file_for(cfg.stream_dir.as_deref(), &req.session_id);
        let mut usage = UsageAcc::default();
        let mut llm_ms: u64 = 0;
        // E4-minimal：指标随 assistant 事件外抛（复用既有 trace 通道，不建新系统）——
        // llm_calls = 实际 LLM 轮数；retries = 瞬态重试次数；tool_failures = 失败工具数。
        let mut llm_calls: u32 = 0;
        let mut retries: u32 = 0;
        let mut tool_failures: u32 = 0;
        // 墙钟到点后的「强制收敛」状态（T4 语义修正）：置位后不再发起新工具，
        // 由下一轮 LLM 以 wrap-up 指令交出「已完成 / 未完成 / 下一步」。None = 未触发。
        let mut wrap_up: Option<StopReason> = None;
        // N1 空答案自愈闸：nudge 续跑只做一次，防网络持续故障时空转烧钱。
        let mut empty_nudged = false;
        // 总预算（T4）：顶层取配置缺省；子代理继承父链衰减后的剩余（req.*_left）。
        let budget = ChatBudget {
            deadline: req
                .budget_ms_left
                .map(|ms| Instant::now() + Duration::from_millis(ms))
                .or_else(|| cfg.budget.map(|d| Instant::now() + d)),
            tokens_left: req.tokens_left.or(cfg.token_budget),
        };
        let max_rounds = cfg.max_rounds;
        for _round in 0..max_rounds {
            rounds += 1;
            // 停车检查点①：每轮开头——取消（硬中止，用户意愿最高）优先，
            // 其次预算：token 硬停，墙钟软收敛（见 budget::StopReason）。
            if let Some(v) = self.check_cancel(env, &req.session_id).await {
                return v;
            }
            if let Some(reason) = self.check_budget(&budget, &usage) {
                if reason.is_hard() {
                    return self
                        .budget_stop(env, &req.session_id, rounds, &steps, reason, usage, llm_ms, llm_calls, retries, tool_failures)
                        .await;
                }
                self.wrap_up_once(env, &req.session_id, &mut messages, &mut wrap_up, reason).await;
            }
            let sid = self.next_sid(&req.session_id); // 单调唯一（跨回合不碰撞），形状不变
            let stream = stream_path.as_deref().map(|p| (p, sid.as_str()));
            // P7/R5：发送前逐级收紧（token 闸启用且工作集超发送预算时）——窗口减半 →
            // tool_result 限额减半 → 仍超限即 CONTEXT_OVERFLOW，请求不发出。
            // 裁剪只影响本轮工作集，memory 全量历史不受影响。
            messages = match context::tighten_for_context(messages, &cfg) {
                Ok(m) => m,
                Err(v) => {
                    let msg = v["error"]["message"].as_str().unwrap_or_default().to_string();
                    self.trace(env, &req.session_id, json!({"type": "error", "where": "context", "message": msg})).await;
                    return v;
                }
            };
            // 收敛轮必须剥掉工具：手里还有工具，模型一定继续探索——预算就形同虚设。
            // 剥掉后它只剩「说话」这一个动作，被迫交出已完成/未完成。
            // G 轮按需装载：下发 `visible`（deferred 工具不在其中）。若模型上一轮
            // 调过 load_tools，清单已在本轮开头按报名并回 `visible`（见下方 loaded 并集）。
            let round_tools: Option<&[ToolSpec]> = if wrap_up.is_some() { None } else { Some(&visible[..]) };
            let resp = match self
                .plan_with_retry(env, &req.session_id, &mut messages, round_tools, stream, &cfg, &mut retries)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    self.trace(env, &req.session_id, json!({"type": "error", "where": "llm.chat", "message": e.to_string()})).await;
                    return json!({"ok": false, "error": {"code": e.code(), "message": format!("llm chat failed: {e}")}});
                }
            };
            llm_calls += 1;
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
            let answer_text = resp.content.clone().unwrap_or_default();
            if has_acceptance_marker(&answer_text) {
                acceptance_seen = true;
            }
            // contract_first（默认关闭）：首个工具调用前要求验收清单；命中即放行，只拦一次。
            if policy.contract_first && !contract_reminded && !acceptance_seen && !resp.tool_calls.is_empty() {
                contract_reminded = true;
                self.trace(
                    env,
                    &req.session_id,
                    json!({"type": "policy", "where": "contract_first", "message": "缺少验收清单，已拦截首轮工具调用"}),
                )
                .await;
                let reminder = MemoryMsg {
                    role: "user".into(),
                    content: Some(CONTRACT_REMINDER.into()),
                    tool_calls: None,
                    tool_call_id: None,
                    attachments: None,
                };
                self.observe(env, &req.session_id, &[reminder.clone()]).await;
                messages.push(reminder);
                continue;
            }
            // 墙钟已到点而模型仍想调工具：不再执行——这是收敛轮的「不配合」，
            // 不能落回尾部再调一次 LLM（那是多花一次钱）。就地交进度报告收尾：
            // 已回喂的工具结果与思考都留在 messages 里，成果不丢。
            if let Some(reason) = wrap_up {
                if !resp.tool_calls.is_empty() {
                    let mut resp = resp;
                    resp.content = Some(Self::wrap_up_report(rounds, &steps, reason, &cfg));
                    resp.tool_calls.clear();
                    return self
                        .finish(
                            env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms,
                            std::mem::take(&mut round_reasonings), &cfg, llm_calls, retries, tool_failures,
                            wrap_up,
                        )
                        .await;
                }
            }
            if resp.tool_calls.is_empty() {
                // verify_before_final（默认关闭）：非平凡任务（有工具步骤或验收痕迹）最终答复前
                // 需逐条核对验收清单；只提醒一次，答复后无论是否核对均收敛（防死循环）。
                let trivial = steps.is_empty() && !acceptance_seen;
                if policy.verify_before_final && !verify_reminded && !trivial && !has_acceptance_marker(&answer_text) {
                    verify_reminded = true;
                    self.trace(
                        env,
                        &req.session_id,
                        json!({"type": "policy", "where": "verify_before_final", "message": "最终答复缺少验收核对，已提醒一次"}),
                    )
                    .await;
                    let reminder = MemoryMsg {
                        role: "user".into(),
                        content: Some(VERIFY_REMINDER.into()),
                        tool_calls: None,
                        tool_call_id: None,
                        attachments: None,
                    };
                    self.observe(env, &req.session_id, &[reminder.clone()]).await;
                    messages.push(reminder);
                    continue;
                }
                // N1 空答案自愈（用户切网实测：网络瞬断/上游异常会产出 200 空内容
                // 响应，原逻辑按 K502 整轮失败——已回喂的工具结果全部作废）。
                // 截断（length）意味着上下文末尾被思考/前文占满、无正文可用，重试
                // 同输入大概率复现，定向收敛；其余空内容（瞬断类）追加一条 nudge
                // 消息就地续跑一轮，模型看到「上一轮为空」的显式反馈后重新作答。
                // wrap-up 收敛轮不介入（finish 会以进度报告兜底答案）；自愈只此
                // 一次：续轮再空按 K502 收敛，防网络持续故障时空转烧钱。
                if wrap_up.is_none() && resp.content.as_deref().map_or(true, |s| s.trim().is_empty()) {
                    if resp.finish_reason == "length" {
                        let msg = "模型输出被 max_tokens 截断且无正文（finish_reason=length）。请缩小任务或清空会话历史后重试。";
                        self.trace(env, &req.session_id, json!({"type": "error", "where": "finish", "message": msg})).await;
                        self.summary_after_stop(env, &req.session_id, rounds, &steps, None, &usage, llm_ms, llm_calls, retries, tool_failures).await;
                        return json!({"ok": false, "error": {"code": "K502", "reason": "length", "message": msg}});
                    }
                    if empty_nudged {
                        self.trace(env, &req.session_id, json!({"type": "error", "where": "finish", "message": "llm 返回了空答案"})).await;
                        self.summary_after_stop(env, &req.session_id, rounds, &steps, None, &usage, llm_ms, llm_calls, retries, tool_failures).await;
                        return json!({"ok": false, "error": {"code": "K502", "message": "llm 返回了空答案"}});
                    }
                    empty_nudged = true;
                    self.trace(env, &req.session_id, json!({"type": "policy", "where": "empty_answer_nudge", "message": "空答案（可能网络瞬断），已请求模型重新作答"})).await;
                    let nudge = MemoryMsg {
                        role: "user".into(),
                        content: Some("[系统] 上一轮回复为空（可能网络瞬断），请直接给出最终答复。".into()),
                        tool_calls: None,
                        tool_call_id: None,
                        attachments: None,
                    };
                    self.observe(env, &req.session_id, &[nudge.clone()]).await;
                    messages.push(nudge);
                    continue;
                }
                return self
                    .finish(
                        env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms,
                        std::mem::take(&mut round_reasonings), &cfg, llm_calls, retries, tool_failures,
                        wrap_up,
                    )
                    .await;
            }
            // 中间轮（带工具调用的轮次）思考留痕：最终轮由 finish 自行追加
            if let Some(t) = resp.reasoning.clone().filter(|s| !s.trim().is_empty()) {
                round_reasonings.push((rounds, t));
            }

            // 子代理预算快照（T4）：父链已用部分扣除后才传给子——随链衰减，多子代理共享同一剩余。
            let budget_snap = if budget.deadline.is_some() || budget.tokens_left.is_some() {
                Some((
                    budget.deadline.map(|d| d.saturating_duration_since(Instant::now()).as_millis() as u64),
                    budget.tokens_left.map(|t| t.saturating_sub(usage.total())),
                ))
            } else {
                None
            };

            // 行动 + 观察（P4/T2 并行）：
            // ① 全部 tool_call 事件按声明顺序先发（trace 顺序稳定，前端同轮卡片齐出）；
            // ② 按波次并发执行（波宽 = 自身 manifest.max_inflight，与内核在途许可对齐）；
            // ③ 结果按声明顺序回喂——tool_call_id 对应与 steps 顺序不变。
            // memory 即上下文来源，截断（PLAN R2）必须在入 memory 之前。
            for tc in &resp.tool_calls {
                self.act_begin(env, &req.session_id, rounds, tc).await;
            }
            let assistant_msg = MemoryMsg {
                role: "assistant".into(),
                content: resp.content.clone(),
                tool_calls: Some(resp.tool_calls.clone()),
                tool_call_id: None,
                attachments: None,
            };
            let mut round_msgs = vec![assistant_msg];
            let wave = self.manifest.max_inflight.unwrap_or(4).max(1);
            for group in resp.tool_calls.chunks(wave) {
                // 停车检查点②'：波次间取消。工具执行可能很长（bash 等到命令超时 / 技能
                // 工具 60s / web_search 引擎链），若只在轮末检查，「点了停止后台还在跑」
                // 的卡顿即来源于此。此刻 round_msgs 尚未入 memory（append 在波次循环
                // 之后），直接丢弃无孤儿消息。预算检查仍留在轮次边界，不在此重复。
                if let Some(v) = self.check_cancel(env, &req.session_id).await {
                    return v;
                }
                let execs = group.iter().map(|tc| {
                    self.act_exec(env, &req.session_id, req.depth, tc, budget_snap, &deferred_names)
                });
                let done = futures::future::join_all(execs).await;
                let mut newly: HashSet<String> = HashSet::new();
                // G 轮：本波次里模型通过 load_tools 报名的工具名（自下一轮起下发 schema）
                let mut newly_visible: Vec<String> = Vec::new();
                for (tc, (result, ms)) in group.iter().zip(done) {
                    // R9/R9b：load_skill 或 skill_install 成功 → 已启用工具并入后续轮次清单
                    //（skill_loaded 事件已在 act_exec 内发出；HashSet 去重防重复并入）
                    if (tc.name == RESERVED_LOAD_SKILL || tc.name == RESERVED_SKILL_INSTALL)
                        && result.get("ok") == Some(&json!(true))
                    {
                        if let Some(name) =
                            tc.arguments.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
                        {
                            if loaded_skills.insert(name.to_string()) {
                                newly.insert(name.to_string());
                            }
                        }
                    }
                    // G 轮：ask_user 成功 → 记下问题与调用 id，波次末统一收敛为 paused。
                    // 只记第一条：模型一轮里问两个问题是设计外的（第二个问题会随
                    // 回合挂起丢失），记第一条至少保住一个。
                    if paused_by_ask.is_none() && result.get("paused") == Some(&json!(true)) {
                        if let Some(q) = result.get("question").and_then(Value::as_str) {
                            let cid = result
                                .get("tool_call_id")
                                .and_then(Value::as_str)
                                .unwrap_or(tc.id.as_str())
                                .to_string();
                            paused_by_ask = Some((q.to_string(), cid));
                        }
                    }
                    // G 轮：load_tools 报名成功 → 这些名字的完整 schema 自下一轮起下发。
                    // 与技能工具那条并集互不干扰（技能工具走 loaded_skills，这里走 visible）。
                    if tc.name == RESERVED_LOAD_TOOLS && result.get("ok") == Some(&json!(true)) {
                        if let Some(names) = result.get("loaded").and_then(Value::as_array) {
                            for n in names.iter().filter_map(Value::as_str) {
                                newly_visible.push(n.to_string());
                            }
                        }
                    }
                    steps.push(StepRecord { round: rounds, tool: tc.name.clone(), ms });
                    if tc.name == "todo" {
                        // 验收清单落 todo = 「清单已建立」，contract_first 后续轮次放行。
                        acceptance_seen = true;
                    }
                    if result.get("ok") != Some(&json!(true)) {
                        tool_failures += 1;
                    }
                    // R16：胸牌查找（含波次间并入的技能工具）——观测分派元数据化
                    let obs = tools.iter().find(|t| t.name == tc.name).and_then(|t| t.obs.as_ref());
                    self.act_end(env, &req.session_id, rounds, tc, &result, ms, &cfg, obs).await;
                    round_msgs.push(MemoryMsg {
                        role: "tool".into(),
                        // 回喂预算按工具分化（PLAN tools §十一）：信息密集型工具放宽
                        content: Some(truncate_chars(
                            &result.to_string(),
                            super::tools_exec::effective_result_limit(&tc.name, &cfg),
                        )),
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                        attachments: None,
                    });
                }
                // G 轮：ask_user 命中 → 本回合**暂停**而不是结束。
                //
                // 判定放在波次末：同波次里可能还有别的工具在跑（并行），必须等它们
                // 都收尾、结果都落库，回合才能安全挂起——否则那些结果会随回合收敛丢失。
                // 收敛方式：写 `paused` + 问题与 tool_call_id 到 turn 文件，然后把
                // assistant 的「问题」作为本回合最终答复返回（前端据此渲染提问卡）。
                // **不进下一轮 LLM**：那会花一次钱再让模型自己说「我在等回答」。
                if let Some((q, call_id)) = paused_by_ask.as_ref() {
                    let end_cursor = self
                        .call(env, CAP_MEMORY, json!({"op": "trace.read", "session_id": req.session_id, "after": 0}), MEM_DEADLINE)
                        .await
                        .ok()
                        .and_then(|v| v.get("next").and_then(Value::as_u64))
                        .unwrap_or(start_cursor);
                    if turn::mark_paused(&mut turn, q, call_id, end_cursor) {
                        if let Some(s) = turn_store.as_ref() {
                            s.save(&turn);
                        }
                        self.trace(
                            env,
                            &req.session_id,
                            json!({"type": "turn_paused", "turn_id": turn.get("turn_id"), "question": q, "tool_call_id": call_id}),
                        )
                        .await;
                        // 暂停不是失败：ok=true + paused=true，答案位放问题本身
                        // （前端可直接渲染成提问卡，不必再拼一句「我在等你说」）。
                        let mut out = json!({
                            "ok": true,
                            "answer": q,
                            "paused": true,
                            "turn_id": turn.get("turn_id"),
                            "pending": turn.get("pending"),
                            "rounds": rounds,
                            "steps": steps,
                            "session_id": req.session_id,
                        });
                        out["turn_status"] = json!("paused");
                        out["end_cursor"] = json!(end_cursor);
                        return out;
                    }
                }
                if !newly.is_empty() {
                    // 技能工具并集：tools 与 visible 同步（否则「装了却看不见」）
                    let extra = self.session_skill_tools(env, &newly).await;
                    tools.extend(extra.clone());
                    visible.extend(extra);
                }
                // G 轮：load_tools 报名过的工具并入 visible（已在下发集里则跳过，防同名双份）
                for n in newly_visible {
                    if visible.iter().any(|t| t.name == n) {
                        continue;
                    }
                    if let Some(spec) = tools.iter().find(|t| t.name == n).cloned() {
                        visible.push(spec);
                    }
                }
            }
            // LOOP-P2/A2：每轮工具/观察结果必须落持久层，否则下一轮 perceive 的历史缺这段；
        // 连续失败 ≥3 才熔断（容忍偶发抖动），已产出内容仍在 trace 与前端可见。
        if !self.observe(env, &req.session_id, &round_msgs).await
            && self.append_fails.load(Ordering::SeqCst) >= 3
        {
            return json!({"ok": false, "error": {"code": "K503", "message": "memory.append 连续失败 3 次，熔断本轮（防工作集与持久层静默分叉）"}});
        }
        messages.extend(round_msgs);
            // 停车检查点②：工具波次完成后（不必等下一轮 LLM 调用才知道要停）
            if let Some(reason) = self.check_budget(&budget, &usage) {
                if reason.is_hard() {
                    return self
                        .budget_stop(env, &req.session_id, rounds, &steps, reason, usage, llm_ms, llm_calls, retries, tool_failures)
                        .await;
                }
                self.wrap_up_once(env, &req.session_id, &mut messages, &mut wrap_up, reason).await;
            }
        }

        // 停车检查点③：轮次耗尽前的强制收敛轮（预算可能恰在此间耗尽）
        if let Some(reason) = self.check_budget(&budget, &usage) {
            if reason.is_hard() {
                return self
                    .budget_stop(env, &req.session_id, rounds, &steps, reason, usage, llm_ms, llm_calls, retries, tool_failures)
                    .await;
            }
            wrap_up = Some(reason);
        }
        // 轮次耗尽：最后一轮不带工具，强制收敛（同样先过发送前收紧）
        rounds += 1;
        let sid = self.next_sid(&req.session_id); // 单调唯一，形状不变
        let stream = stream_path.as_deref().map(|p| (p, sid.as_str()));
        messages = match context::tighten_for_context(messages, &cfg) {
            Ok(m) => m,
            Err(v) => {
                let msg = v["error"]["message"].as_str().unwrap_or_default().to_string();
                self.trace(env, &req.session_id, json!({"type": "error", "where": "context", "message": msg})).await;
                return v;
            }
        };
        let resp = match self
            .plan_with_retry(env, &req.session_id, &mut messages, None, stream, &cfg, &mut retries)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                self.trace(env, &req.session_id, json!({"type": "error", "where": "llm.chat", "message": e.to_string()})).await;
                return json!({"ok": false, "error": {"code": e.code(), "message": format!("llm chat failed: {e}")}});
            }
        };
        usage.add(resp.usage.as_ref());
        llm_ms += resp.elapsed_ms.unwrap_or(0);
        llm_calls += 1;
        if resp.ok && resp.tool_calls.is_empty() {
            return self
                .finish(
                    env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms,
                    std::mem::take(&mut round_reasonings), &cfg, llm_calls, retries, tool_failures,
                    wrap_up,
                )
                .await;
        }
        // 收敛轮没交出答案（模型仍在发工具调用 / LLM 失败）：预算已到点，
        // 宁可交一份「进度报告」也不把本轮成果整批丢掉。
        if let Some(reason) = wrap_up {
            let report = Self::wrap_up_report(rounds, &steps, reason, &cfg);
            let mut resp = resp;
            resp.content = Some(report);
            resp.tool_calls.clear();
            return self
                .finish(
                    env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms,
                    std::mem::take(&mut round_reasonings), &cfg, llm_calls, retries, tool_failures,
                    wrap_up,
                )
                .await;
        }
        self.trace(env, &req.session_id, json!({"type": "error", "where": "max_rounds", "message": format!("agent loop exhausted max_rounds={max_rounds}")})).await;
        // E7-C2：轮次耗尽出口也交画像（三出口闭环：自然收敛 / 预算停 / 失败收敛）。
        self.summary_after_stop(env, &req.session_id, rounds, &steps, None, &usage, llm_ms, llm_calls, retries, tool_failures)
            .await;
        json!({"ok": false, "error": {"code": "K502", "message": format!("agent loop exhausted max_rounds={max_rounds}")}})
    }

    /// 预算硬停（仅 token）：成本上限已到，不再花任何一次 LLM 调用。
    /// 与旧实现一致地返回 K508——调用方（宿主/子代理）据此判断「被预算掐断」。
    async fn budget_stop(
        &self,
        env: &Envelope,
        session_id: &str,
        rounds: u32,
        steps: &[StepRecord],
        reason: StopReason,
        usage: UsageAcc,
        llm_ms: u64,
        llm_calls: u32,
        retries: u32,
        tool_failures: u32,
    ) -> Value {
        let msg = match reason {
            StopReason::Tokens => "chat 预算耗尽：token 超限",
            StopReason::Deadline => "chat 预算耗尽：时长超限",
        };
        self.trace(env, session_id, json!({"type": "error", "where": "budget", "message": msg})).await;
        // E7-C1：预算硬停也要交 chat_summary——「被掐断」的回合不该在效率画像里缺段。
        self.summary_after_stop(env, session_id, rounds, steps, Some(reason.tag()), &usage, llm_ms, llm_calls, retries, tool_failures)
            .await;
        json!({"ok": false, "error": {"code": "K508", "message": msg}})
    }

    /// E7-C1/C2：非收敛出口（预算硬停 / 空答案 / 轮次耗尽）的效率画像兜底。
    /// 此前三类出口只发 error、不发 chat_summary——脚本无法区分「没跑」与「跑了但失败」。
    /// 字段与 finish 的收敛版对齐，仅少 budget_secs/max_rounds（静态配置，失败画像非必需）；
    /// stopped_by=None 表示非预算原因（类型沿用收敛版："tokens"/"deadline"/null）。
    #[allow(clippy::too_many_arguments)]
    async fn summary_after_stop(
        &self,
        env: &Envelope,
        session_id: &str,
        rounds: u32,
        steps: &[StepRecord],
        stopped_by: Option<&str>,
        usage: &UsageAcc,
        llm_ms: u64,
        llm_calls: u32,
        retries: u32,
        tool_failures: u32,
    ) {
        let step_count = steps.len();
        self.trace(
            env,
            session_id,
            json!({
                "type": "chat_summary",
                "calls": step_count,
                "rounds": rounds,
                "steps": step_count,
                "llm_calls": llm_calls,
                "retries": retries,
                "failures": tool_failures,
                "failure_rate": if step_count == 0 { 0.0 } else {
                    (tool_failures as f64) / (step_count as f64)
                },
                "tokens": usage.to_value(),
                "llm_ms": llm_ms,
                "stopped_by": stopped_by,
            }),
        )
        .await;
    }

    /// 墙钟到点：插入一次性 wrap-up 指令并置位软停状态（之后不再执行工具）。
    /// 只提醒一次（`wrap_up` 已置位即返回），防重复注入。
    async fn wrap_up_once(
        &self,
        env: &Envelope,
        session_id: &str,
        messages: &mut Vec<MemoryMsg>,
        wrap_up: &mut Option<StopReason>,
        reason: StopReason,
    ) {
        if wrap_up.is_some() {
            return;
        }
        *wrap_up = Some(reason);
        self.trace(
            env,
            session_id,
            json!({"type": "policy", "where": "budget_wrap_up", "message": "墙钟预算到点，转入强制收敛轮（成果保留）"}),
        )
        .await;
        let reminder = MemoryMsg {
            role: "user".into(),
            content: Some(WRAP_UP_DEADLINE.into()),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        };
        self.observe(env, session_id, &[reminder.clone()]).await;
        messages.push(reminder);
    }

    /// 进度报告兜底：收敛轮没能给出答案时，用已记录的步骤自述进度。
    /// 宁可交一份可续做的清单，也不让本轮成果随中止消失。
    fn wrap_up_report(rounds: u32, steps: &[StepRecord], reason: StopReason, cfg: &AgentLoopConfig) -> String {
        let mut by_tool: Vec<(String, usize)> = Vec::new();
        for s in steps {
            match by_tool.iter_mut().find(|(t, _)| t == &s.tool) {
                Some((_, n)) => *n += 1,
                None => by_tool.push((s.tool.clone(), 1)),
            }
        }
        let detail = if by_tool.is_empty() {
            "（本次未执行工具调用）".to_string()
        } else {
            by_tool
                .iter()
                .map(|(t, n)| format!("{t}×{n}"))
                .collect::<Vec<_>>()
                .join("、")
        };
        format!(
            "[预算到点，自动收敛] 原因：{}。\n\
             已完成：{} 轮、{} 个工具调用（{detail}）。\n\
             未完成：任务尚未收尾，模型未在收敛轮交出答案。\n\
             下一步：按上述进度从断点续做；如需更长单轮时长，调大 agent.budget_secs。",
            match reason {
                StopReason::Tokens => "token 预算耗尽".to_string(),
                StopReason::Deadline => format!("墙钟预算 {}s 到点", cfg.budget.map(|d| d.as_secs()).unwrap_or(0)),
            },
            rounds,
            steps.len(),
        )
    }

    /// 收敛：最终答案入记忆并返回（含 steps）。
    ///
    /// 事件带上 sid（与流式增量对位，供前端复用同一气泡）、reasoning、累计 usage 与
    /// LLM 总耗时。流式增量只经旁路文件实时外抛，不落日志；这条事件才是持久化与
    /// 刷新恢复的唯一依据，因此内容必须与流式所见一致（含思考）。
    /// W9.4：reasonings 携带全部轮次的思考（round + 文本，最终轮在末尾追加），
    /// 前端刷新重放按轮渲染——旁路每轮覆写只留末轮的局限由此补齐。
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish(
        &self,
        env: &Envelope,
        session_id: &str,
        resp: LlmChatResp,
        rounds: u32,
        steps: Vec<StepRecord>,
        sid: String,
        usage: UsageAcc,
        llm_ms: u64,
        mut round_reasonings: Vec<(u32, String)>,
        cfg: &AgentLoopConfig,
        llm_calls: u32,
        retries: u32,
        tool_failures: u32,
        wrap_up: Option<StopReason>,
    ) -> Value {
        let mut answer = resp.content.clone().unwrap_or_default();
        if answer.trim().is_empty() && wrap_up.is_some() {
            // 预算软停：模型没给答案也要交进度报告（不能把成果随空答案一起丢掉）
            answer = Self::wrap_up_report(rounds, &steps, wrap_up.unwrap_or(StopReason::Deadline), cfg);
        }
        if answer.trim().is_empty() {
            // N1：空答案自愈已前移到主循环（本函数唯一的 wrap-up 入口在预算软停
            // 路径，nudge 续跑对其无意义——进度报告兜底已先行）。截断类在此处
            // 仍可能到达（wrap-up 收敛轮），给出与主循环一致的可行动错误。
            let truncated = resp.finish_reason == "length";
            let msg = if truncated {
                "模型输出被 max_tokens 截断且无正文（finish_reason=length）。请缩小任务或清空会话历史后重试。"
            } else {
                "llm 返回了空答案"
            };
            self.trace(env, session_id, json!({"type": "error", "where": "finish", "message": msg})).await;
            // E7-C2：失败出口同样交画像（steps 已落地的成果数如实上报）。
            self.summary_after_stop(env, session_id, rounds, &steps, None, &usage, llm_ms, llm_calls, retries, tool_failures)
                .await;
            let mut err_val = json!({"code": "K502", "message": msg});
            if truncated {
                err_val["error"]["reason"] = json!("length");
            }
            return json!({"ok": false, "error": err_val});
        }
        self.observe(
            env,
            session_id,
            &[MemoryMsg {
                role: "assistant".into(),
                content: Some(answer.clone()),
                tool_calls: None,
                tool_call_id: None,
                attachments: None,
            }],
        )
        .await;
        let reasoning = resp.reasoning.clone();
        // 最终轮思考追加到留痕末尾（中间轮已在 chat_body 循环中收集）
        if let Some(t) = reasoning.clone().filter(|s| !s.trim().is_empty()) {
            round_reasonings.push((rounds, t));
        }
        let reasonings_json: Vec<Value> = round_reasonings
            .iter()
            .map(|(r, t)| json!({"round": r, "text": t}))
            .collect();
        // 答案文本兜底扫描（R10 前的既有设计，过滤改造时调用点曾遗失致 e2e 回归）：
        // 答案里提到的成品路径登记为 artifact（与 bash 输出同款启发式）；同一路径若
        // 已由 write_file 结构化登记，前端按回合+路径去重，不重复出卡。
        for p in Self::scan_artifact_paths(&answer, &cfg.output_dir) {
            if !self.fresh_artifact(session_id, &p) {
                continue; // 答案提及但非本轮生成（历史文件）：不上卡，点了也是 404
            }
            self.trace(env, session_id, json!({"type": "artifact", "path": p, "tool": "answer"}))
                .await;
        }
        self.trace(
            env,
            session_id,
            json!({
                "type": "assistant",
                "answer": answer,
                "rounds": rounds,
                "sid": sid,
                "reasoning": reasoning,
                "reasonings": reasonings_json,
                "usage": usage.to_value(),
                "elapsed_ms": llm_ms,
                "metrics": {"llm_calls": llm_calls, "retries": retries, "tool_failures": tool_failures},
            }),
        )
        .await;
        // D1 效率可观测：单条 chat_summary 汇总本轮效率画像，脚本无需重扫全部事件。
        // `calls`/`rounds` 同值（本实现每轮恰一次工具波次），保留两键以便未来分离。
        // `steps` = 实际执行的工具调用数（含并行波次内多个），是「调用量」真相源。
        let step_count = steps.len();
        self.trace(
            env,
            session_id,
            json!({
                "type": "chat_summary",
                "calls": step_count,
                "rounds": rounds,
                "steps": step_count,
                "llm_calls": llm_calls,
                "retries": retries,
                "failures": tool_failures,
                "failure_rate": if step_count == 0 { 0.0 } else {
                    (tool_failures as f64) / (step_count as f64)
                },
                "tokens": usage.to_value(),
                "llm_ms": llm_ms,
                "budget_secs": cfg.budget.map(|d| d.as_secs()).unwrap_or(0),
                "max_rounds": cfg.max_rounds,
                // 预算停车原因（None = 自然收敛）：`tokens` 硬停 / `deadline` 软收敛。
                // 观测口径：前端不渲染，脚本据此区分「任务做完了」与「被时长掐断」。
                "stopped_by": wrap_up.map(|r| r.tag()),
            }),
        )
        .await;
        json!({"ok": true, "answer": answer, "rounds": rounds, "steps": steps, "session_id": session_id})
    }
}
