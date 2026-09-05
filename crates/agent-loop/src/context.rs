//! 上下文策略模块（PLAN P7/R5-R6，发送前）。
//!
//! 架构裁决：上下文 = 发给 LLM 的工作集，归属权在**组装者**（编排层）；memory 只拥有
//! 持久层全量历史。裁剪顺序（全量拉取 → 压缩 → 窗口 → 发送）是 ReAct 时间流的一部分，
//! 必须单一 ownership——不做独立插件（guest 不可互调 + 热路径过 gRPC 得不偿失）。
//!
//! 职责（纯函数策略，无独立生命周期/失败域）：
//! - `estimate_tokens`：字符 → token 保守估算（CJK ≈ 1 token/字、其余 ≈ 4 字符/token，
//!   宁高估早触发，不追求 provider 精确口径）。
//! - 压缩双闸：条数闸 **或** token 闸（估算工作集超发送预算）任一命中即压缩
//!   （`maybe_compact` 消费；单条大结果在条数闸之前就能撑爆窗口）。
//! - 发送前逐级收紧 `tighten_for_context`：窗口条数减半 → tool_result 限额减半 →
//!   仍超限则返回 CONTEXT_OVERFLOW 错误 payload（不发请求）。
//! - 降级收紧 `degrade`（P8 兜底，响应后）：provider 侧超限（估算闸漏网）时
//!   窗口/限额减半重试一次，由 `plan_with_retry` 消费。
//!
//! 预算口径：`CTX_BUDGET = LLM_CONTEXT_TOKENS × 0.7`（预留输出与工具 schema 开销）；
//! `LLM_CONTEXT_TOKENS=0`（缺省）= token 闸禁用，向后兼容（只按条数管理）。
//! 适用范围（L7）：仅本地窗口型 provider（ollama 等显存受限、需以窗口换速度的部署形态）；
//! 云端 API 不消费 num_ctx 且窗口由服务端管理，本闸一并禁用——避免为本地调小的窗口值
//! 误压云端历史。非名单 provider 一律视同 0（禁用）。

use crate::contract::MemoryMsg;
use serde_json::{json, Value};

/// 本地窗口型 provider 名单：会消费 `options.num_ctx` 的部署形态（显存受限，常需调小窗口换速度）。
/// 扩展方式：新本地部署后端（llama.cpp server / vLLM / LM Studio 等）接入后在此加名即可。
const LOCAL_WINDOW_PROVIDERS: [&str; 1] = ["ollama"];

/// LLM 上下文窗口（token）：`LLM_CONTEXT_TOKENS`；仅本地窗口型 provider 生效，
/// 其余 provider 返回 0（token 闸禁用 + 不下发 num_ctx）。0/缺省 = 禁用。
pub fn context_window_tokens() -> usize {
    let provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
    if !LOCAL_WINDOW_PROVIDERS.contains(&provider.as_str()) {
        return 0;
    }
    std::env::var("LLM_CONTEXT_TOKENS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// 发送预算：窗口 × 0.7（预留模型输出与工具 schema 开销）；窗口 0 → 预算 0（闸禁用）。
pub fn ctx_budget() -> usize {
    let w = context_window_tokens();
    if w == 0 {
        0
    } else {
        (w as f64 * 0.7) as usize
    }
}

/// 宽字符判定（保守口径：宽字符按 1 token/字计，宁高估）。
/// 覆盖 CJK 统一表意/扩展、兼容表意、全角形式、Hangul、Emoji。
fn is_wide(ch: char) -> bool {
    let c = ch as u32;
    matches!(c,
        0x1100..=0x115F       // Hangul Jamo
        | 0x2E80..=0xA4CF     // CJK 部首 ~ Yi
        | 0xAC00..=0xD7A3     // Hangul 音节
        | 0xF900..=0xFAFF     // CJK 兼容表意
        | 0xFE30..=0xFE4F     // CJK 兼容形式
        | 0xFF00..=0xFF60     // 全角形式
        | 0x1F300..=0x1FAFF   // Emoji（近似 1 token/枚）
        | 0x20000..=0x3FFFD   // CJK 扩展 B+
    )
}

/// token 保守估算：CJK/宽字符 ≈ 1 token/字，其余 ≈ 4 字符/token（宁高估早触发）。
pub fn estimate_tokens(s: &str) -> usize {
    let (mut wide, mut narrow) = (0usize, 0usize);
    for ch in s.chars() {
        if is_wide(ch) {
            wide += 1;
        } else {
            narrow += 1;
        }
    }
    wide + (narrow + 3) / 4
}

/// 单条消息估算：content + tool_calls（name + JSON 实参）+ 每条 4 token 结构余量。
fn estimate_msg(m: &MemoryMsg) -> usize {
    let mut n = estimate_tokens(m.content.as_deref().unwrap_or("")) + 4;
    if let Some(tcs) = &m.tool_calls {
        for tc in tcs {
            n += estimate_tokens(&tc.name) + estimate_tokens(&tc.arguments.to_string());
        }
    }
    n
}

/// 工作集（发给 LLM 的消息列表）token 估算。
pub fn estimate_messages(msgs: &[MemoryMsg]) -> usize {
    msgs.iter().map(estimate_msg).sum()
}

/// 窗口条数减半：保留 system 头 + 尾半；丢弃新窗口前缘的孤儿 tool 消息
/// （其配对的 assistant.tool_calls 已被裁掉，孤儿回喂会被 provider 拒收）。
/// 假定 `msgs[0]` 为 system（调用方 chat_run 保证）。
fn halve_window(mut msgs: Vec<MemoryMsg>) -> Vec<MemoryMsg> {
    if msgs.len() <= 1 {
        return msgs;
    }
    let keep = ((msgs.len() - 1) / 2).max(1);
    let cut = msgs.len() - keep;
    msgs.drain(1..cut);
    let first_real = msgs.iter().enumerate().skip(1).find(|(_, m)| m.role != "tool").map(|(i, _)| i);
    match first_real {
        Some(i) if i > 1 => {
            msgs.drain(1..i);
        }
        None => {
            msgs.truncate(1); // 病态：尾半全是 tool 消息 → 只留 system 头
        }
        _ => {}
    }
    msgs
}

/// tool 消息内容按 `limit` 再截断（减半闸第二级；limit=0 即 tool_result_limit 禁用，不截）。
fn retrim_tool_results(msgs: &mut [MemoryMsg], limit: usize) {
    if limit == 0 {
        return;
    }
    for m in msgs.iter_mut() {
        if m.role == "tool" {
            if let Some(c) = m.content.as_deref() {
                if c.chars().count() > limit {
                    m.content = Some(crate::truncate_chars(c, limit));
                }
            }
        }
    }
}

/// 发送前逐级收紧（P7/R5，预防在发送前）：token 闸启用且工作集估算超预算时——
/// ① 窗口条数减半；② tool_result 限额减半；仍超 → Err（CONTEXT_OVERFLOW payload，不发请求）。
/// 裁剪只影响本轮发给 LLM 的工作集，memory 全量历史不受影响（下轮重新拉取）。
pub fn tighten_for_context(mut messages: Vec<MemoryMsg>) -> Result<Vec<MemoryMsg>, Value> {
    let budget = ctx_budget();
    if budget == 0 {
        return Ok(messages);
    }
    let est = estimate_messages(&messages);
    if est <= budget {
        return Ok(messages);
    }
    messages = halve_window(messages);
    retrim_tool_results(&mut messages, crate::tool_result_limit() / 2);
    let est2 = estimate_messages(&messages);
    if est2 <= budget {
        tracing::info!(target: crate::ID, "context tightened: {est} -> {est2} est tokens (budget {budget})");
        return Ok(messages);
    }
    Err(json!({
        "ok": false,
        "error": {
            "code": "CONTEXT_OVERFLOW",
            "message": format!(
                "上下文估算 {est2} tokens（原始 {est}）超出发送预算 {budget}，逐级收紧（窗口/工具结果限额减半）后仍超限；请开启新会话或调大 LLM_CONTEXT_TOKENS"
            ),
            "ctx": {"limit": context_window_tokens(), "budget": budget, "estimated": est2},
        }
    }))
}

/// 降级收紧（P8/R7，兜底在响应后）：provider 侧 CONTEXT_OVERFLOW（确定性但可行动）——
/// 窗口条数减半 + tool_result 限额减半后重试一次。不做预算判定（估算已漏网，provider 是终审）。
pub fn degrade(mut msgs: Vec<MemoryMsg>) -> Vec<MemoryMsg> {
    msgs = halve_window(msgs);
    retrim_tool_results(&mut msgs, crate::tool_result_limit() / 2);
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_ascii_cjk_and_mixed() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1); // 4 字符/token
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("abcde"), 2); // 除法向上取整（保守高估）
        assert_eq!(estimate_tokens("中文字"), 3); // CJK 1 token/字
        assert_eq!(estimate_tokens("中文abcd"), 3); // 2 + 1
        // 全角标点按宽字符计
        assert_eq!(estimate_tokens("，。"), 2);
    }

    #[test]
    fn estimate_messages_counts_content_and_tool_calls() {
        let msgs = vec![
            MemoryMsg {
                role: "system".into(),
                content: Some("SYS".into()),
                tool_calls: None,
                tool_call_id: None,
                attachments: None,
            },
            MemoryMsg {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![crate::ToolCall {
                    id: "c1".into(),
                    name: "web_search".into(), // 10 ASCII → 3 tokens
                    arguments: json!({"query": "测试"}), // {"query":"测试"} ≈ 9 narrow + 2 wide → 3+2=5
                }]),
                tool_call_id: None,
                attachments: None,
            },
        ];
        // system: 1 + 4 = 5；assistant: 0 + 4 + 3 + 5+... 实参 JSON 串含结构字符
        let est = estimate_messages(&msgs);
        assert!(est >= 15, "应含 content + tool_calls + 结构余量: {est}");
    }

    #[test]
    fn halve_window_keeps_system_and_drops_orphan_tools() {
        let m = |role: &str, c: &str| MemoryMsg {
            role: role.into(),
            content: Some(c.into()),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        };
        // [sys, u1, a1, t1, t2, user] → 保留尾半 2 条 + sys；t1/t2 的配对 assistant（a1）
        // 已被裁掉 → 前缘孤儿 tool 全部丢弃
        let out = halve_window(vec![m("system", "SYS"), m("user", "u1"), m("assistant", "a1"), m("tool", "t1"), m("tool", "t2"), m("user", "u2")]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "system");
        assert_eq!(out[1].content.as_deref(), Some("u2"));
        // 尾半全是 tool 的病态 → 只留 system
        let out = halve_window(vec![m("system", "SYS"), m("user", "u1"), m("tool", "t1")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "system");
    }

    // L7 回归：窗口值仅本地窗口型 provider 生效（env 全局，测试串行化避免多线程互踩）
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn window_only_applies_to_local_window_providers() {
        let _g = env_lock().lock().unwrap();
        let (prov, tok) =
            (std::env::var("LLM_PROVIDER").ok(), std::env::var("LLM_CONTEXT_TOKENS").ok());

        std::env::set_var("LLM_PROVIDER", "openai");
        std::env::set_var("LLM_CONTEXT_TOKENS", "8192");
        assert_eq!(context_window_tokens(), 0, "云端 provider 不消费窗口：token 闸禁用");
        assert_eq!(ctx_budget(), 0);

        std::env::set_var("LLM_PROVIDER", "ollama");
        assert_eq!(context_window_tokens(), 8192, "ollama 读窗口值");
        assert_eq!(ctx_budget(), 5734); // 8192 × 0.7

        std::env::set_var("LLM_CONTEXT_TOKENS", "0");
        assert_eq!(context_window_tokens(), 0, "0=禁用（向后兼容）");

        std::env::remove_var("LLM_CONTEXT_TOKENS");
        assert_eq!(context_window_tokens(), 0, "缺省=禁用");

        match prov {
            Some(v) => std::env::set_var("LLM_PROVIDER", v),
            None => std::env::remove_var("LLM_PROVIDER"),
        }
        match tok {
            Some(v) => std::env::set_var("LLM_CONTEXT_TOKENS", v),
            None => std::env::remove_var("LLM_CONTEXT_TOKENS"),
        }
    }
}
