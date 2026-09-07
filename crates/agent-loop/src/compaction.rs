//! 上下文压缩（R12 拆分自 lib.rs）。
//!
//! 职责：`maybe_compact`（压缩双闸：条数 or 估算 token → LLM 摘要 → memory `summarize`
//! 持久化 → 就地替换本轮工作集）与 `compaction_marker`（压缩标记消息，与 memory 插件
//! summarize 的合成消息保持同构）。

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
    pub(super) async fn maybe_compact(&self, src: &Envelope, session_id: &str, history: Vec<MemoryMsg>) -> Vec<MemoryMsg> {
        fn env_num(key: &str, default: usize) -> usize {
            std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
        }
        let trigger = env_num("COMPACT_TRIGGER", 40);
        let keep = env_num("COMPACT_KEEP", 10).min(history.len());
        // 双闸（PLAN P7/R6）：条数闸 **或** token 闸任一命中即压缩——
        // 单条大结果在条数闸（40 条）之前就能撑爆 LLM 窗口。
        let count_gate = trigger > 0 && history.len() > trigger;
        let budget = context::ctx_budget();
        let token_gate = budget > 0 && context::estimate_messages(&history) > budget;
        if !count_gate && !token_gate {
            return history;
        }
        let split = history.len() - keep;
        let (older, recent) = history.split_at(split);
        let older = older.to_vec();

        // LLM 摘要旧史（不带工具）。R3：摘要输入剥离附件（图片 b64 对文本摘要模型
        // 无意义且易撑爆压缩调用；文件要点已可从 content 内嵌文本获得）。
        let mut sum_msgs: Vec<MemoryMsg> = vec![MemoryMsg {
            role: "system".into(),
            content: Some(
                "Summarize the conversation history for an AI agent. Capture: the user's goal, decisions made, \
facts learned, files/actions taken, and pending work. Be concise (<= 300 words). Output only the summary."
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
        let summary = match self.plan_with_retry(src, session_id, &mut sum_msgs, None, None).await {
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
}
