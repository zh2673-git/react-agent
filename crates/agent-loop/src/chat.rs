//! ReAct 主循环与 LLM 规划（R12 拆分自 lib.rs）。
//!
//! 职责：`chat_body` / `chat_run`（主状态机循环）、`finish`（收敛）、`perceive` /
//! `observe`（记忆读写）、`plan` / `plan_with_retry`（LLM 调用 + T3 瞬态重试 +
//! P8 超限降级）、`max_rounds`（生效轮次上限）、用户消息装配（`build_user_msg` /
//! `text_attach_limit` / `base64_decode_utf8`）与历史窗口（`apply_history_limit`）。

use super::budget::{
    budget_secs, is_transient_kernel_err, is_transient_llm_error, retry_attempts, retry_base_ms, token_budget,
    ChatBudget, UsageAcc,
};
use super::subagent::task_spec;
use super::tools_exec::truncate_chars;
use super::trace::stream_file_for;
use super::*;
use std::time::Instant;

/// 文本附件内嵌上限（字符）：0 = 禁用截断。
pub(super) fn text_attach_limit() -> usize {
    std::env::var("TEXT_ATTACH_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_TEXT_ATTACH_CHARS)
}

/// R3：把用户附件装配进 user 消息。
///
/// - 图片（mime 以 image/ 开头）→ 结构化 `attachments` 字段，由 llm-adapter 按
///   provider 协议映射（OpenAI 兼容 image_url data URI / ollama native images 数组）；
/// - 文本文件 → 直接拼入 content（一次内嵌，超限截断并标注）——provider 无需感知；
/// - 其他二进制类型 → 不内嵌内容，content 中注明名称与类型（不静默丢弃）。
pub(super) fn build_user_msg(user_text: &str, attachments: Option<&[Attachment]>) -> MemoryMsg {
    let Some(list) = attachments else {
        return MemoryMsg {
            role: "user".into(),
            content: Some(user_text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        };
    };
    let limit = text_attach_limit();
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
pub(super) fn apply_history_limit(mut msgs: Vec<MemoryMsg>) -> Vec<MemoryMsg> {
    if let Ok(lim) = std::env::var("HISTORY_LIMIT") {
        if let Ok(n) = lim.trim().parse::<usize>() {
            if n > 0 && msgs.len() > n {
                msgs = msgs.split_off(msgs.len() - n);
            }
        }
    }
    msgs
}

impl AgentLoopPlugin {
    /// 生效轮次上限（E1 收编）：构造值兜底，`MAX_ROUNDS` env 每轮可热改
    /// （web 设置面板保存即下轮对话生效，无需重启）。
    pub(super) fn max_rounds(&self) -> usize {
        std::env::var("MAX_ROUNDS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(self.max_rounds)
    }

    /// 感知：拉取会话**全量**历史（不做窗口裁剪）。
    /// 窗口与压缩的顺序见 `apply_history_limit`（PLAN R3：先压缩判断，后窗口裁剪）。
    pub(super) async fn perceive(&self, src: &Envelope, session_id: &str) -> Result<Vec<MemoryMsg>, KernelError> {
        let v = self
            .call(src, ID_MEMORY, json!({"op": "get", "session_id": session_id}), MEM_DEADLINE)
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
    ) -> Result<LlmChatResp, KernelError> {
        let mut payload = json!({"op": "chat", "messages": messages});
        // L2+L3：`LLM_CONTEXT_TOKENS` 语义为「上下文窗口」——随 payload 透传 num_ctx，
        // ollama native 映射 options.num_ctx（本地估算闸与服务端窗口对齐，一处配置两侧生效）。
        // L7：仅本地窗口型 provider 生效（context::context_window_tokens 内部判定 LLM_PROVIDER，
        // 云端 API / 非名单 provider 返回 0 = 不下发且 token 闸禁用——避免为本地调小的窗口
        // 误压云端历史）。注意 provider 取自 host 启动时 env：Web 热切换 provider 后本判定
        // 滞后，重启校正（provider 切换低频，可接受）。
        let window = context::context_window_tokens();
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
    ) -> Result<LlmChatResp, KernelError> {
        let attempts = retry_attempts();
        let base = retry_base_ms();
        let mut attempt: u32 = 0;
        let mut degraded = false; // P8：本轮 chat 是否已做过超限降级（只做一次）
        loop {
            let res = self.plan(src, messages, tools, stream).await;
            if !degraded {
                let overflow = matches!(&res, Ok(r) if
                    r.error.as_ref().and_then(|e| e.get("code")).and_then(Value::as_str) == Some("CONTEXT_OVERFLOW"));
                if overflow {
                    degraded = true;
                    let taken = std::mem::take(messages);
                    *messages = context::degrade(taken);
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

    /// 观察：写入记忆（尽力而为，失败不致命）。
    pub(super) async fn observe(&self, src: &Envelope, session_id: &str, msgs: &[MemoryMsg]) {
        if let Err(e) = self
            .call(src, ID_MEMORY, json!({"op": "append", "session_id": session_id, "messages": msgs}), MEM_DEADLINE)
            .await
        {
            tracing::warn!(target: ID, "memory append failed: {e}");
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
        let out = self.chat_run(env, &req).await;
        self.cancels.lock().unwrap().remove(&req.session_id);
        out
    }

    pub(super) async fn chat_run(&self, env: &Envelope, req: &ChatReq) -> Value {
        // 回合开始时间：兜底产物扫描的新鲜度基准（见 turn_starts 字段注释）
        self.turn_starts
            .lock()
            .unwrap()
            .insert(req.session_id.clone(), std::time::SystemTime::now());
        // 用户消息先入记忆（持久化），随后拉取全量历史。
        // R3：文本附件拼入 content，图片附件走结构化字段（llm-adapter 按 provider 映射）。
        let user_msg = build_user_msg(&req.user_text, req.attachments.as_deref());
        self.observe(env, &req.session_id, &[user_msg.clone()]).await;
        // trace 带全量原始附件（含文本 b64）：UI 重放展示缩略图/文件条，重新生成需原始 data_b64 重发
        let mut user_event = json!({"type": "user", "text": req.user_text});
        if let Some(att) = req.attachments.as_deref() {
            user_event["attachments"] = json!(att);
        }
        self.trace(env, &req.session_id, user_event).await;

        // 系统提示词 = 组装链（07 §2.1）+ 技能附录（软）
        let system = format!("{}{}", self.resolve_system_prompt(env).await, self.skills_appendix(env).await);
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
                let compacted = self.maybe_compact(env, &req.session_id, h).await;
                messages.extend(apply_history_limit(compacted));
            }
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

        // R9 清单组装：tools = 内置启用集 ∪ 当前会话已加载技能的已启用技能工具。
        // 会话技能集从 trace 重放推导（skill_loaded 事件）；失败降级为空不阻断主流程。
        let mut loaded_skills = self.trace_loaded_skills(env, &req.session_id).await;
        if !loaded_skills.is_empty() {
            tools.extend(self.session_skill_tools(env, &loaded_skills).await);
        }

        let mut steps: Vec<StepRecord> = Vec::new();
        let mut rounds: u32 = 0;
        // W9.4：逐轮思考留痕——流式旁路每轮覆写只留末轮，刷新重放看不到中间轮思考。
        // 各轮 reasoning 收集于此，随最终 assistant 事件持久化（轮次 + 文本）。
        let mut round_reasonings: Vec<(u32, String)> = Vec::new();
        // 流式旁路：宿主以 AGENT_STREAM_DIR 下发目录；未配置即退化为一问一答（行为同改造前）。
        let stream_path = stream_file_for(&req.session_id);
        let mut usage = UsageAcc::default();
        let mut llm_ms: u64 = 0;
        // 总预算（T4）：顶层取 env 缺省；子代理继承父链衰减后的剩余（req.*_left）。
        let budget = ChatBudget {
            deadline: req
                .budget_ms_left
                .map(|ms| Instant::now() + Duration::from_millis(ms))
                .or_else(|| budget_secs().map(|d| Instant::now() + d)),
            tokens_left: req.tokens_left.or_else(token_budget),
        };
        let max_rounds = self.max_rounds();
        for _round in 0..max_rounds {
            rounds += 1;
            // 停车检查点①：每轮开头（取消 > 时长 > token）
            if let Some(v) = self.check_stop(env, &req.session_id, &budget, &usage).await {
                return v;
            }
            let sid = self.next_sid(&req.session_id); // 单调唯一（跨回合不碰撞），形状不变
            let stream = stream_path.as_deref().map(|p| (p, sid.as_str()));
            // P7/R5：发送前逐级收紧（token 闸启用且工作集超发送预算时）——窗口减半 →
            // tool_result 限额减半 → 仍超限即 CONTEXT_OVERFLOW，请求不发出。
            // 裁剪只影响本轮工作集，memory 全量历史不受影响。
            messages = match context::tighten_for_context(messages) {
                Ok(m) => m,
                Err(v) => {
                    let msg = v["error"]["message"].as_str().unwrap_or_default().to_string();
                    self.trace(env, &req.session_id, json!({"type": "error", "where": "context", "message": msg})).await;
                    return v;
                }
            };
            let resp = match self.plan_with_retry(env, &req.session_id, &mut messages, Some(&tools), stream).await {
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
                return self
                    .finish(env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms, std::mem::take(&mut round_reasonings))
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
            let limit = tool_result_limit();
            let wave = self.manifest.max_inflight.unwrap_or(4).max(1);
            for group in resp.tool_calls.chunks(wave) {
                // 停车检查点②'：波次间取消。工具执行可能很长（bash 等到命令超时 / 技能
                // 工具 60s / web_search 引擎链），若只在轮末检查，「点了停止后台还在跑」
                // 的卡顿即来源于此。此刻 round_msgs 尚未入 memory（append 在波次循环
                // 之后），直接丢弃无孤儿消息。预算检查仍留在轮次边界，不在此重复。
                if let Some(v) = self.check_cancel(env, &req.session_id).await {
                    return v;
                }
                let execs = group
                    .iter()
                    .map(|tc| self.act_exec(env, &req.session_id, req.depth, tc, budget_snap));
                let done = futures::future::join_all(execs).await;
                let mut newly: HashSet<String> = HashSet::new();
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
                    steps.push(StepRecord { round: rounds, tool: tc.name.clone(), ms });
                    self.act_end(env, &req.session_id, rounds, tc, &result, ms).await;
                    round_msgs.push(MemoryMsg {
                        role: "tool".into(),
                        content: Some(truncate_chars(&result.to_string(), limit)),
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                        attachments: None,
                    });
                }
                if !newly.is_empty() {
                    tools.extend(self.session_skill_tools(env, &newly).await);
                }
            }
            self.observe(env, &req.session_id, &round_msgs).await;
            messages.extend(round_msgs);
            // 停车检查点②：工具波次完成后（不必等下一轮 LLM 调用才知道要停）
            if let Some(v) = self.check_stop(env, &req.session_id, &budget, &usage).await {
                return v;
            }
        }

        // 停车检查点③：轮次耗尽前的强制收敛轮（预算可能恰在此间耗尽）
        if let Some(v) = self.check_stop(env, &req.session_id, &budget, &usage).await {
            return v;
        }
        // 轮次耗尽：最后一轮不带工具，强制收敛（同样先过发送前收紧）
        rounds += 1;
        let sid = self.next_sid(&req.session_id); // 单调唯一，形状不变
        let stream = stream_path.as_deref().map(|p| (p, sid.as_str()));
        messages = match context::tighten_for_context(messages) {
            Ok(m) => m,
            Err(v) => {
                let msg = v["error"]["message"].as_str().unwrap_or_default().to_string();
                self.trace(env, &req.session_id, json!({"type": "error", "where": "context", "message": msg})).await;
                return v;
            }
        };
        let resp = match self.plan_with_retry(env, &req.session_id, &mut messages, None, stream).await {
            Ok(r) => r,
            Err(e) => {
                self.trace(env, &req.session_id, json!({"type": "error", "where": "llm.chat", "message": e.to_string()})).await;
                return json!({"ok": false, "error": {"code": e.code(), "message": format!("llm chat failed: {e}")}});
            }
        };
        usage.add(resp.usage.as_ref());
        llm_ms += resp.elapsed_ms.unwrap_or(0);
        if resp.ok && resp.tool_calls.is_empty() {
            return self
                .finish(env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms, std::mem::take(&mut round_reasonings))
                .await;
        }
        self.trace(env, &req.session_id, json!({"type": "error", "where": "max_rounds", "message": format!("agent loop exhausted max_rounds={max_rounds}")})).await;
        json!({"ok": false, "error": {"code": "K502", "message": format!("agent loop exhausted max_rounds={max_rounds}")}})
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
    ) -> Value {
        let answer = resp.content.clone().unwrap_or_default();
        if answer.trim().is_empty() {
            // PLAN R4：空答案视为失败——ok:true + answer:"" 是假收敛；不落 memory、
            // 不发 assistant 事件，按错误 payload 收敛。
            self.trace(env, session_id, json!({"type": "error", "where": "finish", "message": "llm 返回了空答案"})).await;
            return json!({"ok": false, "error": {"code": "K502", "message": "llm 返回了空答案"}});
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
        for p in Self::scan_artifact_paths(&answer) {
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
            }),
        )
        .await;
        json!({"ok": true, "answer": answer, "rounds": rounds, "steps": steps, "session_id": session_id})
    }
}
