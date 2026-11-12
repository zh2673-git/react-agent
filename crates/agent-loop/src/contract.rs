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
    /// R16 观测面声明（工具胸牌）：工具自带「我的结果如何被编排层观测」。
    /// 缺省（未声明）→ 编排层回落遗留注册表（write_file/edit_file/bash/web_search/
    /// web_read 五个内置名）；显式声明（含空 {}）覆盖注册表。llm providers 按字段名
    /// 取值（name/description/parameters），本字段对 LLM 线路不可见。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obs: Option<ToolObs>,
    /// 按需装载（G 轮）：`Some(true)` = 该工具**不进下发的工具清单**，只在系统提示词里
    /// 留一行名字摘要；模型调保留工具 `load_tools` 报过名之后才随清单下发。
    ///
    /// 为什么要有这个字段：工具 schema 随**每一轮**请求重发。MCP 每接一台 server 就是
    /// 十几件工具的完整 JSON Schema，而单轮真正用得上的通常只有 1-2 件——其余是纯
    /// token 税，且长会话里这笔开销与任务难度无关。
    ///
    /// 判据边界：只影响**下发**，不影响**调用**。模型凭记忆直接调一个 deferred 工具
    /// 照样成功（tools.call 不看该标记）——「猜中即可用」，不惩罚直觉。
    /// 缺省（未声明）→ 照常下发，既有工具行为零变化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,
}

/// R16 观测面声明（胸牌 schema）：声明工具结果中可被结构化观测的三类产物。
/// 未知取值按未声明处理（向后兼容：新增枚举值不破坏旧编排层）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolObs {
    /// 产物登记（前端可点击文件卡）：
    /// - `"structured"`：`result.path`/`result.bytes` 结构化承载（write_file/edit_file 形态）
    /// - `"output_text"`：`result.output` 自由文本启发式扫描（bash 形态）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<String>,
    /// 变更登记（前端「📝 文件变更」chip + 回滚 diff 数据源）：
    /// `op` 为 file_change 事件的 op 值；`from` 为变更来源形状：
    /// - 缺省 / `"single_path"`：结果即一次变更（`result.path` + `result.undo` 透传）
    /// - `"change_list"`：`result.changes[]` 逐条展开（每条自带 path/undo）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<ToolObsChanges>,
    /// 来源登记（前端溯源卡）：
    /// - `"result_list"`：`result.results[]{title,url}`（web_search 形态）
    /// - `"single_url"`：`result.url` 单链接（web_read 形态）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<String>,
}

/// 胸牌 changes 子结构（R16）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolObsChanges {
    /// file_change 事件的 op 值（前端变更 chip 标签；如 "write"/"edit"/"bash"）。
    #[serde(default)]
    pub op: String,
    /// 变更来源形状（缺省 single_path；"change_list" = result.changes[] 展开）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
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
    /// G 轮：回合寻址键（可选）。给了就复用该 turn（暂停后回复走这里），
    /// 没给就新开一条。缺省 None 向后兼容所有既有调用方。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// G 轮：对 `ask_user` 暂停的回答。给了它就按 `tool_call_id` 接回同一回合，
    /// 不重新装配工作集（这正是「暂停而不是结束」的收益）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_reply: Option<ToolReply>,
}

fn default_session() -> String {
    "default".into()
}

/// G 轮：对 `ask_user` 暂停的回答。
///
/// 为什么要 `tool_call_id` 而不是只给文本：回复必须作为**那一条 tool_call 的结果**
/// 接回去，否则模型看到的是「一个没有对应调用的孤立 tool 消息」——严格函数调用协议
/// 下直接报错，宽松协议下模型会重新规划、把已有进度丢掉。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolReply {
    pub tool_call_id: String,
    pub answer: String,
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
