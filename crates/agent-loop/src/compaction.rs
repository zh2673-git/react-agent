//! 上下文压缩（R12 拆分自 lib.rs）。
//!
//! 职责：`maybe_compact`（压缩双闸：条数 or 估算 token → LLM 摘要 → memory `summarize`
//! 持久化 → 就地替换本轮工作集）与 `compaction_marker`（压缩标记消息，与 memory 插件
//! summarize 的合成消息保持同构）。

use super::config::AgentLoopConfig;
use super::*;

impl AgentLoopPlugin {
    /// 压缩标记消息（与 memory 插件 summarize 的合成消息保持同构）。
    pub(super) fn compaction_marker(summary: &str) -> MemoryMsg {
        MemoryMsg {
            role: "user".into(),
            content: Some(format!(
                "[Context compaction] 之前的会话历史已压缩为以下摘要：\n{summary}\n请基于该摘要与后续消息继续任务，不要声称记得被压缩的原文。"
            )),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        }
    }

    /// 上下文压缩（Phase 2-2，dsh：压缩是独立可选能力，不焊进 Loop 状态机）：
    /// 历史超过 COMPACT_TRIGGER（默认 40；0=禁用）**或**估算 token 超发送预算
    /// （P7/R6 token 闸，LLM_CONTEXT_TOKENS>0 时启用）时，把除最近 COMPACT_KEEP（默认 10）条
    /// 之外的旧史交 LLM 摘要，经 memory `summarize` op 持久化（含孤儿 tool 消息防撕裂），
    /// 并就地替换本轮工作集。任何失败（llm/memory）→ 降级为不压缩（warn），主流程不受影响。
    pub(super) async fn maybe_compact(
        &self,
        src: &Envelope,
        session_id: &str,
        history: Vec<MemoryMsg>,
        cfg: &AgentLoopConfig,
    ) -> Vec<MemoryMsg> {
        self.maybe_compact_with_keep(src, session_id, history, cfg, cfg.compact_keep).await
    }

    /// 压缩（显式保留条数）。`keep_override` = 保留最近多少条；`None` 用配置缺省。
    ///
    /// 为什么要显式参数：chat 开头的压缩按 `COMPACT_KEEP`（默认 30）保留——那是
    /// 「给用户回看的一段」；轮内压缩（P0-B）的目的是把**下一轮要发的上下文**
    /// 压回有界，尾巴太大省不下多少（每条工具结果可达 8K 字符）。两种场景
    /// 对「保留多少」的诉求不同，用同一入口 + 显式参数分开，不靠调用点猜。
    pub(super) async fn maybe_compact_with_keep(
        &self,
        src: &Envelope,
        session_id: &str,
        history: Vec<MemoryMsg>,
        cfg: &AgentLoopConfig,
        keep_override: usize,
    ) -> Vec<MemoryMsg> {
        let trigger = cfg.compact_trigger;
        let keep = keep_override.min(history.len());
        // 双闸（PLAN P7/R6）：条数闸 **或** token 闸任一命中即压缩——
        // 单条大结果在条数闸（40 条）之前就能撑爆 LLM 窗口。
        let count_gate = trigger > 0 && history.len() > trigger;
        // token 闸用**工作集预算**（workset_budget）：云端 provider 的 token 闸此前
        // 完全禁用（ctx_budget 只对本地窗口型 provider 生效），于是长会话里历史
        // 每轮全量重发、成本 O(N²) 涨（实测单会话 371 轮累计 cache_read 47.3M，
        // 而当日 77.1% 的 credits 来自 cache_read）。
        // 压缩是**软动作**（摘要旧史，不中止），故用独立预算而非 ctx_budget——
        // 后者驱动 tighten_for_context 的硬收紧（超了会减半窗口甚至报错中止）。
        let budget = cfg.workset_budget();
        let token_gate = budget > 0 && context::estimate_messages(&history) > budget;
        if !count_gate && !token_gate {
            return history;
        }
        let split = history.len() - keep;
        let (older, _recent) = history.split_at(split);


        // LLM 摘要旧史（不带工具）。R3：摘要输入剥离附件（图片 b64 对文本摘要模型
        // 无意义且易撑爆压缩调用；文件要点已可从 content 内嵌文本获得）。
        let mut sum_msgs: Vec<MemoryMsg> = vec![MemoryMsg {
            role: "system".into(),
            content: Some(
                "Summarize the conversation history for an AI agent. Put a section titled '## PENDING (must do next)' \
FIRST, listing: unfinished work, options already rejected (and why), and the single next action. Only then cover \
the user's goal, verified facts (with file:line when known), files/actions taken, and decisions still in force. \
Never drop a pending item even if it appeared early in the history, and never drop the reason a path was abandoned \
(a re-derived wrong turn is worse than a longer summary). Omit pleasantries and restatement; length follows \
necessity, not a fixed word cap. Output only the summary."
                    .into(),
            ),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        }];
        sum_msgs.extend(older.iter().map(|m| MemoryMsg {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_calls: m.tool_calls.clone(),
            tool_call_id: m.tool_call_id.clone(),
            // R3：摘要输入剥离附件（图片 b64 对文本摘要模型无意义且易撑爆压缩调用；
            // 文件要点已可从 content 内嵌文本获得）。
            // P3 降开销：此逐条 owned 克隆是「摘要输入需要独立于 history 的 owned 消息」的
            // 必要一份——上一处 `older.to_vec()` 的整段克隆已删，故总克隆量减半。
            attachments: None,
        }));
        sum_msgs.push(MemoryMsg {
            role: "user".into(),
            content: Some("Summarize the above history now.".into()),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        });
        // 压缩摘要不是用户可见输出 → 不走流式旁路；瞬态失败同样重试（T3）
        // （压缩的重试不进主链 metrics——口径为用户可见回合的 LLM 调用）
        let mut compaction_retries = 0u32;
        let summary = match self
            .plan_with_retry(src, session_id, &mut sum_msgs, None, None, cfg, &mut compaction_retries)
            .await
        {
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
                CAP_MEMORY,
                json!({"op": "summarize", "session_id": session_id, "summary": summary, "keep_last": keep}),
                MEM_DEADLINE,
            )
            .await
        {
            Ok(v) if v.get("ok") == Some(&json!(true)) => {
                tracing::info!(target: ID, "context compacted: {} older messages summarized, kept {keep}", older.len());
                self.trace(src, session_id, json!({"type": "compaction", "summarized": older.len(), "kept": keep, "summary": summary})).await;
                // P3 降开销：压缩已经成功（memory.summarize 持久化完成），下游只有成功路径——
                // 此时 history 不再需要保留，可把 recent 段**按值消费**（into_iter），
                // 省掉原先对 recent 的逐条 clone。recent 段 = 下标 >= boundary 的部分，
                // boundary = history.len() - keep（keep 已 min 到 len）。
                //
                // P2：规范化 system（不假定在首位、只留一个、取最后一条非空）后置于压缩标记之前。
                // 与 memory 插件 summarize 的合成消息同构（sys 前置 + 标记 + recent）；recent 里的
                // 任何 system 一律剔除（只保留选定的唯一 sys）。空内容 system 视为无 system。
                let boundary = history.len() - keep;
                let sys_pos = history.iter().rposition(|m| {
                    m.role == "system"
                        && m.content.as_deref().map(|c| !c.trim().is_empty()).unwrap_or(false)
                });
                let mut compacted: Vec<MemoryMsg> = Vec::with_capacity(keep + 2);
                if let Some(i) = sys_pos {
                    compacted.push(history[i].clone());
                }
                compacted.push(Self::compaction_marker(&summary));
                compacted.extend(
                    history
                        .into_iter()
                        .enumerate()
                        .filter(|(i, m)| *i >= boundary && m.role != "system")
                        .map(|(_, m)| m),
                );
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
}
