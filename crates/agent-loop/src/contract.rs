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
    /// R3 多模态附件（仅图片；文本文件已在构造时拼入 content）：纯追加字段，
    /// 旧 memory 数据 / 未升级 provider 读不到该键时行为不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

/// R3 附件（用户上传）：`data_b64` 为不含 `data:` 前缀的裸 base64。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub data_b64: String,
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
    /// 委派深度，随调用链传播（非插件级共享状态）：0=顶层会话；1=子代理。
    /// 子代理链内（depth >= 1）拒绝再调 task，嵌套上限由此收敛；
    /// 缺省 0 向后兼容既有调用方（host/e2e 无需携带）。
    #[serde(default)]
    pub depth: u32,
    /// 剩余预算（T4，随链继承）：单次 chat 墙钟剩余毫秒。顶层请求缺省
    /// （读 env 缺省值）；子代理由父链传入**衰减后**的剩余，防「子代理继承
    /// 同预算无衰减」——每层委派都在消耗同一份总预算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_ms_left: Option<u64>,
    /// 剩余 token 预算（T4）：input+output 累计口径。继承语义同上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_left: Option<u64>,
    /// R3 用户附件（可选）：host 已做形状/条数/体量校验，此处仅透传。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
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
    // ── 追加可选字段（缺省 = provider 未提供，旧插件/未升级 provider 不受影响）──
    /// 思考内容（DeepSeek reasoning_content 等）；无思考模型为 None。
    #[serde(default)]
    pub reasoning: Option<String>,
    /// 归一化用量：{input_tokens, output_tokens, cache_read_tokens, reasoning_tokens}。
    #[serde(default)]
    pub usage: Option<Value>,
    /// 该次 LLM 请求墙钟耗时（毫秒）。
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
}
