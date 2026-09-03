//! 线契约（wire contract）：三个 guest 插件（Python/TS）与本 crate 共用的 JSON Schema 镜像。
//!
//! 单一 canonical schema 定义于此；Python/TS 侧按字段名严格镜像。
//! 业务错误一律走 payload 内的 `{"ok":false,"error":{...}}`；`KernelError` 仅承载
//! 传输/生命周期层失败。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 规范化工具调用（OpenAI 兼容 / Anthropic 两种形状已在 llm-adapter 内归一化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// object（JSON Schema 实参），不是字符串。
    #[serde(default)]
    pub arguments: Value,
}

/// 工具规格（JSON Schema 描述）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Value,
}

/// 会话消息（memory 与 llm-adapter 共用同一形状）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMsg {
    /// "system" | "user" | "assistant" | "tool"
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// role == "tool" 时必填（关联 assistant 的 tool_calls[].id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

// ---- agent-loop ------------------------------------------------------------

/// 逐轮工具调用观测（steps，只回传不持久化；事件日志 Phase 3 的对位预留）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub round: u32,
    pub tool: String,
    pub ms: u64,
}

/// `agent-loop` chat 请求。
#[derive(Debug, Deserialize)]
pub struct ChatReq {
    #[serde(default = "default_session")]
    pub session_id: String,
    pub user_text: String,
}

fn default_session() -> String {
    "default".into()
}

// ---- llm-adapter -----------------------------------------------------------

/// `llm-adapter` chat 响应。
#[derive(Debug, Clone, Deserialize)]
pub struct LlmChatResp {
    pub ok: bool,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub finish_reason: String,
    #[serde(default)]
    pub error: Option<Value>,
}
