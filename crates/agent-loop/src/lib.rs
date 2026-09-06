//! ReAct 编排插件（Rust，InProcess 域）。
//!
//! 时间契约：主执行流为**状态机循环**（感知→规划→行动→观察→回跳），收敛于最终答案
//! 或 `max_rounds` 强制收敛。循环状态全部存于局部变量与会话记忆（memory 插件），
//! 本体 `&self` 无跨调用可变态（A1）。委派深度随调用链传播（`ChatReq.depth`），
//! 非插件级共享状态——并发下各链互不挤占委派额度。
//! 取消（P2/T1）：`cancel` op 置位会话标志，循环在轮次边界（每轮开头/工具波次后）
//! 轮询命中即收敛为 K499；语义为 Concurrent——否则 cancel 会在 per-plugin 锁后排队、
//! 迟到到 chat 结束之后。
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

mod context;
mod contract;

pub use contract::{Attachment, ChatReq, LlmChatResp, MemoryMsg, StepRecord, ToolCall, ToolSpec};

use agent_kernel_sdk::*;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

/// 保留工具名：技能安装（R9）——assets skills.load 取声明 → tools.install 装载配套工具
/// （进池不启用），trace `skill_installed` 事件供前端内联卡与一键启用。不下发 tools。
pub const RESERVED_SKILL_INSTALL: &str = "skill_install";

/// 各转发步的相对截止（A2：Envelope.deadline 为相对时长）。
const MEM_DEADLINE: Duration = Duration::from_secs(5);
const TOOLS_DEADLINE: Duration = Duration::from_secs(60);
const LLM_DEADLINE: Duration = Duration::from_secs(120);
const ASSETS_DEADLINE: Duration = Duration::from_secs(5);

/// 工具结果回喂上限（字符数，PLAN R2）：防止单条大结果撑爆上下文与 memory。
/// 0 = 禁用截断。可用 `TOOL_RESULT_LIMIT` 覆盖。
const DEFAULT_TOOL_RESULT_CHARS: usize = 8000;

/// 文本附件内嵌上限（字符，R3）：文本文件不进结构化 attachments（仅图片多模态），
/// 而是拼入 user content——一次内嵌、后续轮次随历史自然参与上下文。
/// 单文件超限截断并显式标注（模型必须能感知内容不完整）。可用 `TEXT_ATTACH_LIMIT` 覆盖。
const DEFAULT_TEXT_ATTACH_CHARS: usize = 24000;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a capable agent working in a workspace. \
Answer the user's request. When tools are provided and useful, call them (one batch per round); \
after receiving tool results, continue reasoning until you can produce the final answer in plain text. \
Presemble precise tool arguments: read before writing files, and prefer edit_file over rewriting whole files.";

pub struct AgentLoopPlugin {
    max_rounds: usize,
    manifest: Manifest,
    host: OnceLock<Arc<dyn HostApi>>,
    /// 子会话计数（sub_session id 唯一性；并发下各链父会话 id 不同，全局递增即可）。
    sub_counter: AtomicU64,
    /// 取消令牌（P2/T1）：被请求取消的 session id 集合。`cancel` op 置位，循环在
    /// 轮次边界 take（命中即清）；chat 结束时兜底清理，防残留标志误杀同 session 下轮对话。
    cancels: Mutex<HashSet<String>>,
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
        // Concurrent（P2/T1）：本体 &self 无跨调用可变态（A1），并发安全由构造保证；
        // Serial 会让 cancel dispatch 在 per-plugin 锁后排队到 chat 结束之后（取消永远迟到）。
        semantics: Semantics::Concurrent,
        priority: 1,
        // 8 = 并发 chat 槽位 + cancel 通道余量（cancel 不应被满载 chat 挤掉）。
        max_inflight: Some(8),
        fuel_limit: None,
        host_timeout_ms: None,
        epoch_interval_ms: None,
        subscriptions: vec![],
    };
    Arc::new(AgentLoopPlugin {
        max_rounds,
        manifest,
        host: OnceLock::new(),
        sub_counter: AtomicU64::new(0),
        cancels: Mutex::new(HashSet::new()),
    })
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

/// 工具结果回喂上限（字符）：0 = 禁用。
fn tool_result_limit() -> usize {
    std::env::var("TOOL_RESULT_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_TOOL_RESULT_CHARS)
}

/// 单次 chat 总时长预算（T4）：`CHAT_BUDGET_SECS`（秒，支持小数便于测试；0=禁用，缺省 300）。
/// 这是轮次边界的护栏——轮内超支由单步 deadline（5s/60s/120s）封顶，不追求精确。
fn budget_secs() -> Option<Duration> {
    let v: f64 = std::env::var("CHAT_BUDGET_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(300.0);
    if v <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(v).ok()
}

/// 单次 chat 总 token 预算（T4）：`CHAT_TOKEN_BUDGET`（input+output 累计；0=禁用）。
fn token_budget() -> Option<u64> {
    match std::env::var("CHAT_TOKEN_BUDGET").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(n) if n > 0 => Some(n),
        _ => None,
    }
}

/// LLM 瞬态失败重试次数（T3）：`LLM_RETRY_ATTEMPTS`（缺省 2，上限 6）。
fn retry_attempts() -> u32 {
    std::env::var("LLM_RETRY_ATTEMPTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2).min(6)
}

/// 重试退避基数毫秒（T3）：`LLM_RETRY_BASE_MS`（缺省 500；0 用于测试立即重试）。
fn retry_base_ms() -> u64 {
    std::env::var("LLM_RETRY_BASE_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(500)
}

/// 瞬态判定（T3）：限流/超时/网关类错误值得重试；参数/鉴权类重试无益。
/// llm-adapter 的 provider 异常统一为 code=LLM_ERROR + message="{ExcType}: {exc}"，
/// 故按 message 关键词匹配（429/rate limit/timeout/5xx/连接类）；K400 是确定性失败。
fn is_transient_llm_error(err: &Value) -> bool {
    let code = err.get("code").and_then(Value::as_str).unwrap_or("");
    if code == "K400" {
        return false;
    }
    let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
    let hay = format!("{code} {msg}").to_ascii_lowercase();
    [
        "429", "rate limit", "ratelimit", "timeout", "timed out", "502", "503", "504", "overloaded", "connection",
        "temporarily",
    ]
    .iter()
    .any(|k| hay.contains(k))
}

/// 内核层瞬态判定（T3）：deadline 超时值得重试；panic/取消/路由失败重试无益。
fn is_transient_kernel_err(e: &KernelError) -> bool {
    matches!(e, KernelError::DeadlineExceeded(_))
}

/// 单次 chat 的剩余预算（T4）。顶层 chat 取 env 缺省；子代理继承父链衰减后的剩余。
#[derive(Debug, Clone, Copy, Default)]
struct ChatBudget {
    deadline: Option<Instant>,
    tokens_left: Option<u64>,
}

/// 按字符截断（UTF-8 安全）。超限时追加省略标记——模型必须能感知结果被裁剪，
/// 而非把残缺内容当作完整事实。
fn truncate_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if max == 0 || total <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("\n…[truncated, {total} chars total, showing first {max}]"));
    out
}

/// 文本附件内嵌上限（字符）：0 = 禁用截断。
fn text_attach_limit() -> usize {
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
fn build_user_msg(user_text: &str, attachments: Option<&[Attachment]>) -> MemoryMsg {
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
fn base64_decode_utf8(b64: &str) -> String {
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
fn apply_history_limit(mut msgs: Vec<MemoryMsg>) -> Vec<MemoryMsg> {
    if let Ok(lim) = std::env::var("HISTORY_LIMIT") {
        if let Ok(n) = lim.trim().parse::<usize>() {
            if n > 0 && msgs.len() > n {
                msgs = msgs.split_off(msgs.len() - n);
            }
        }
    }
    msgs
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

    /// 计费口径（T4 预算）：input+output（cache_read 是折扣而非独立产出，不计入）。
    fn total(&self) -> u64 {
        self.input + self.output
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
    /// 生效轮次上限（E1 收编）：构造值兜底，`MAX_ROUNDS` env 每轮可热改
    /// （web 设置面板保存即下轮对话生效，无需重启）。
    fn max_rounds(&self) -> usize {
        std::env::var("MAX_ROUNDS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(self.max_rounds)
    }

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
    /// R9 扩展：技能打包与安装引导——SKILL.md + tools.json（语言无关声明）+ 执行体后
    /// 调 skill_install；装载后工具处于待启用态，由用户在界面确认启用；启用后
    /// load_skill 即在会话内生效。
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
             no reload call needed. Keep skills small and focused; invalid frontmatter is silently skipped.\n\n\
             ### Skill packaging with companion tools\n\
             A skill may ship its own tools in a language-agnostic way: write a `tools.json` into the \
             skill directory (a JSON array; each item {\"name\",\"description\",\"parameters\",\
             \"exec\":{\"cmd\":[...],\"cwd\"?}}), plus the executor programs in any language \
             (Python/Node/Rust binary/script — declare the exact command in exec.cmd). Executor protocol: \
             read one JSON {\"args\":{...}} from stdin, reply one JSON {\"ok\":true,\"result\":...} or \
             {\"ok\":false,\"error\":{\"code\",\"message\"}} on stdout. After writing the files, call the \
             reserved tool `skill_install` with {\"name\":\"<skill>\"}: it loads the tools into the pool \
             but does NOT enable them (the user approves each tool in the settings UI); after enabling, \
             call load_skill to activate the tools in the session. Declare tools only when the skill \
             truly needs them.\n"
                .replace("<skills-root>", skills_root)
                + "\n",
        )
    }

    /// R9：会话已加载技能集——从 trace 重放推导（skill_loaded 事件），**不新增循环可变态**
    /// （守 A1）。同会话新对话经重放恢复技能作用域；子代理新会话不继承（独立 trace）。
    async fn trace_loaded_skills(&self, env: &Envelope, session_id: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        if let Ok(v) = self
            .call(env, ID_MEMORY, json!({"op": "trace.read", "session_id": session_id, "after": 0}), MEM_DEADLINE)
            .await
        {
            if let Some(events) = v.get("events").and_then(Value::as_array) {
                for e in events {
                    if e.get("type").and_then(Value::as_str) == Some("skill_loaded") {
                        if let Some(s) = e.get("skill").and_then(Value::as_str) {
                            if !s.is_empty() {
                                out.insert(s.to_string());
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// R9：会话级技能工具清单——**已加载技能**的**已启用**工具（装配进 LLM 工具清单）。
    /// 未装载/未启用对模型不可见（防误用 + 清单不被领域工具污染）。失败降级为空，
    /// 主流程不因技能工具失败中断。
    async fn session_skill_tools(&self, env: &Envelope, skills: &HashSet<String>) -> Vec<ToolSpec> {
        if skills.is_empty() {
            return vec![];
        }
        let payload = json!({"op": "skill_tools", "skills": skills});
        match self.call(env, ID_TOOLS, payload, TOOLS_DEADLINE).await {
            Ok(v) => serde_json::from_value(v.get("tools").cloned().unwrap_or(Value::Null)).unwrap_or_default(),
            Err(e) => {
                tracing::warn!(target: ID, "tools.skill_tools failed, proceeding without skill tools: {e}");
                vec![]
            }
        }
    }

    /// R9：技能安装编排（保留名 skill_install 路由终点）——
    /// ① assets skills.load：取 SKILL.md 全文 + tools 声明（tools_manifest）；
    /// ② 有声明 → tools.install：fail-closed 装载进池（装载 ≠ 启用，不进任何会话清单）；
    /// ③ trace `skill_installed`（无声明技能同样发出，tools_* 为空 → 前端统一呈现「已注册」）；
    /// ④ 观察回写（含失败明细，部分成功不算整体失败——技能注册本身总是成立）。
    async fn skill_install(&self, env: &Envelope, session_id: &str, name: &str) -> Value {
        if name.trim().is_empty() {
            return json!({"ok": false, "error": {"code": "K400", "field": "name",
                "message": "skill_install 需非空参数 {\"name\": str}（技能名）"}});
        }
        // ① 注册表取声明（unknown skill → 错误 payload 原样回喂）
        let loaded = match self
            .call(env, ID_ASSETS, json!({"op": "skills.load", "name": name}), ASSETS_DEADLINE)
            .await
        {
            Ok(v) if v.get("ok") == Some(&json!(true)) => v,
            Ok(v) => return v,
            Err(e) => return json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
        };
        let mut tools_loaded: Vec<String> = vec![];
        let mut tools_pending: Vec<String> = vec![];
        let mut issues: Vec<Value> = vec![];
        if let Some(m) = loaded.get("tools_manifest") {
            // 声明存在但文件缺失（assets 回传 missing）→ 明细回写，模型可感知修复
            if let Some(missing) = m.get("missing").and_then(Value::as_array) {
                for f in missing {
                    issues.push(json!({"where": "tools.json", "error": format!("声明文件不存在: {f}")}));
                }
            }
            if let Some(path) = m.get("path").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                // ② fail-closed 装载（装载 ≠ 启用）
                match self
                    .call(env, ID_TOOLS, json!({"op": "install", "path": path, "skill": name}), TOOLS_DEADLINE)
                    .await
                {
                    Ok(v) if v.get("ok") == Some(&json!(true)) => {
                        tools_loaded = v
                            .get("loaded")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default();
                        tools_pending = v
                            .get("pending")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default();
                        for s in v.get("skipped").and_then(Value::as_array).cloned().unwrap_or_default() {
                            issues.push(json!({"where": "tools.install", "error": s}));
                        }
                    }
                    Ok(v) => issues.push(json!({"where": "tools.install", "error": v.get("error").cloned().unwrap_or(v)})),
                    Err(e) => issues.push(json!({"where": "tools.install", "error": e.to_string()})),
                }
            }
        }
        // ③ 事件日志（SSE 实时可达 → 前端内联卡 + 一键启用）
        self.trace(
            env,
            session_id,
            json!({"type": "skill_installed", "skill": name, "tools_loaded": tools_loaded, "tools_pending": tools_pending}),
        )
        .await;
        // ④ 观察回写
        let mut out = json!({
            "ok": true,
            "skill": name,
            "registered": true,
            "tools_loaded": tools_loaded,
            "tools_pending": tools_pending,
            "note": "技能已注册；待启用工具需用户在界面确认（装载≠启用），启用后 load_skill 生效于会话",
        });
        if !issues.is_empty() {
            out["issues"] = json!(issues);
        }
        out
    }

    /// 感知：拉取会话**全量**历史（不做窗口裁剪）。
    /// 窗口与压缩的顺序见 `apply_history_limit`（PLAN R3：先压缩判断，后窗口裁剪）。
    async fn perceive(&self, src: &Envelope, session_id: &str) -> Result<Vec<MemoryMsg>, KernelError> {
        let v = self
            .call(src, ID_MEMORY, json!({"op": "get", "session_id": session_id}), MEM_DEADLINE)
            .await?;
        let msgs: Vec<MemoryMsg> =
            serde_json::from_value(v.get("messages").cloned().unwrap_or(Value::Null)).unwrap_or_default();
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
            attachments: None,
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
    /// 历史超过 COMPACT_TRIGGER（默认 40；0=禁用）**或**估算 token 超发送预算
    /// （P7/R6 token 闸，LLM_CONTEXT_TOKENS>0 时启用）时，把除最近 COMPACT_KEEP（默认 10）条
    /// 之外的旧史交 LLM 摘要，经 memory `summarize` op 持久化（含孤儿 tool 消息防撕裂），
    /// 并就地替换本轮工作集。任何失败（llm/memory）→ 降级为不压缩（warn），主流程不受影响。
    async fn maybe_compact(&self, src: &Envelope, session_id: &str, history: Vec<MemoryMsg>) -> Vec<MemoryMsg> {
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
    async fn plan_with_retry(
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

    /// 行动前奏：登记 tool_call 观测（trace + tracing 日志）。
    /// 并行波次开始前由 chat_body 按**声明顺序**统一发出（trace 顺序稳定）。
    async fn act_begin(&self, src: &Envelope, session_id: &str, round: u32, tc: &ToolCall) {
        tracing::info!(target: "react_progress", round, tool = %tc.name, "▶ round {round}: {}", tc.name);
        self.trace(src, session_id, json!({"type": "tool_call", "round": round, "name": tc.name, "id": tc.id, "args": tc.arguments}))
            .await;
    }

    /// 行动执行：保留名 `task` 路由子代理（07 §2.2），`load_skill` 路由 assets（07 §2.2），
    /// 其余走 tools.exec。失败合成 ok:false 结果回喂（不中断循环）。
    /// 不含事件发射——并行执行时事件顺序由 chat_body 统一编排（声明顺序，稳定）。
    /// 返回 (工具结果, 耗时ms)。`depth` 为当前链上的委派深度（0=顶层，随链传播）；
    /// `budget_snap` 为传给子代理的剩余预算快照 (剩余ms, 剩余token)（T4，随链衰减）。
    async fn act_exec(
        &self,
        src: &Envelope,
        session_id: &str,
        depth: u32,
        tc: &ToolCall,
        budget_snap: Option<(Option<u64>, Option<u64>)>,
    ) -> (Value, u64) {
        let started = Instant::now();
        let result = if tc.name == RESERVED_TASK {
            // 子代理委派（Phase 3-3）：全新会话复用 agent.chat 全链路，仅回传最终答案
            let task_text = tc.arguments.get("task").and_then(Value::as_str).unwrap_or("");
            if task_text.trim().is_empty() {
                json!({"ok": false, "error": {"code": "K400", "field": "task", "message": "task 工具需非空参数 {\"task\": str}（子任务自包含描述）"}})
            } else {
                self.run_subagent(src, session_id, task_text, depth, budget_snap).await
            }
        } else if tc.name == RESERVED_SKILL_INSTALL {
            // R9 技能安装编排（不进 tools 分发）
            let name = tc.arguments.get("name").and_then(Value::as_str).unwrap_or("");
            self.skill_install(src, session_id, name).await
        } else if tc.name == RESERVED_LOAD_SKILL {
            // assets 路由；成功时发 skill_loaded 事件（会话技能集重放推导依据，R9）
            let name = tc.arguments.get("name").and_then(Value::as_str).unwrap_or("");
            let v = match self.call(src, ID_ASSETS, json!({"op": "skills.load", "name": name}), ASSETS_DEADLINE).await {
                Ok(v) => v,
                Err(e) => json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
            };
            if v.get("ok") == Some(&json!(true)) {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    self.trace(src, session_id, json!({"type": "skill_loaded", "skill": trimmed})).await;
                }
            }
            v
        } else {
            match self
                .call(src, ID_TOOLS, json!({"op": "call", "name": tc.name, "args": tc.arguments}), TOOLS_DEADLINE)
                .await
            {
                Ok(v) => v,
                Err(e) => json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
            }
        };
        (result, started.elapsed().as_millis() as u64)
    }

    /// 行动收尾：tool_result 观测。`ms` 为该工具自身耗时（并行下与墙钟无关）；
    /// `memory_truncated` 表明回喂进 memory 的内容是否被截断（PLAN R2 后全文不再入库）。
    async fn act_end(&self, src: &Envelope, session_id: &str, round: u32, tc: &ToolCall, result: &Value, ms: u64) {
        tracing::info!(target: "react_progress", round, tool = %tc.name, ms, "✓ round {round}: {} ({}ms)", tc.name, ms);
        // 事件日志：结果截断（防大输出撑爆审计文件），memory 侧另有 8000 字符预算
        let result_str = result.to_string();
        let full_chars = result_str.chars().count();
        let limit = tool_result_limit();
        let truncated: String = result_str.chars().take(2000).collect();
        self.trace(
            src,
            session_id,
            json!({
                "type": "tool_result", "round": round, "name": tc.name, "id": tc.id, "ms": ms,
                "ok": result.get("ok") == Some(&json!(true)),
                "result_truncated": truncated,
                "memory_truncated": limit > 0 && full_chars > limit,
            }),
        )
        .await;
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
    /// 委派深度随链传播：`depth >= 1`（已在子代理内）→ 字段级拒绝再嵌套；
    /// 子会话事件日志独立（session_id 关联可追溯）。
    /// 预算随链衰减（T4）：`budget_snap` = (剩余ms, 剩余token) 写入子 chat 请求；
    /// 预算未启用（两者皆 None）→ 不携带，子链退化为各自的 env 缺省（同为禁用）。
    async fn run_subagent(
        &self,
        src: &Envelope,
        parent_session: &str,
        task_text: &str,
        depth: u32,
        budget_snap: Option<(Option<u64>, Option<u64>)>,
    ) -> Value {
        if depth >= 1 {
            return json!({"ok": false, "error": {"code": "K400", "message": "task 不支持嵌套委派（子代理内不可再调用 task）"}});
        }
        let n = self.sub_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let sub_session = format!("{parent_session}#sub-{n}");
        self.trace(src, parent_session, json!({"type": "subagent", "sub_session": sub_session, "task": task_text}))
            .await;
        let mut payload = json!({"op": "chat", "session_id": sub_session, "user_text": task_text, "depth": depth + 1});
        if let Some((ms_left, toks_left)) = budget_snap {
            if let Some(ms) = ms_left {
                payload["budget_ms_left"] = json!(ms);
            }
            if let Some(toks) = toks_left {
                payload["tokens_left"] = json!(toks);
            }
        }
        let env = Envelope::new(PluginId::new(ID), payload);
        // 递归委派：Box::pin 打断未来大小的无限递归（嵌套上限由随链 depth 硬性收敛）
        let resp = Box::pin(self.chat_body(&env)).await;
        if resp.get("ok") == Some(&json!(true)) {
            json!({
                "ok": true,
                "answer": resp.get("answer").cloned().unwrap_or(Value::Null),
                "sub_session": sub_session,
            })
        } else {
            // 错误消息瘦身（PLAN E2）：只取 error.message，不再把整个响应 JSON 塞进错误
            let emsg = resp
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            json!({"ok": false, "error": {"code": "SUBAGENT_FAILED", "message": format!("子代理失败: {emsg}")}})
        }
    }

    /// 取消检查（P2/T1）：轮次边界轮询。take 语义（命中即清）。
    /// 返回 Some(error payload) = 已取消，调用方立即收敛返回。
    async fn check_cancel(&self, env: &Envelope, session_id: &str) -> Option<Value> {
        if !self.cancels.lock().unwrap().remove(session_id) {
            return None;
        }
        tracing::info!(target: ID, session = %session_id, "cancelled at round boundary");
        self.trace(env, session_id, json!({"type": "error", "where": "cancel", "message": "已被用户取消"})).await;
        Some(json!({"ok": false, "error": {"code": "K499", "message": "已被用户取消"}}))
    }

    /// 轮次边界统一停车检查（P2 取消 + T4 预算）：取消优先（用户意愿最高），
    /// 其次时长、再次 token。命中即返回错误 payload 立即收敛。
    /// token 口径 input+output（usage.total()）；超支边界 = 当前轮已发生的消耗，
    /// 轮内超支由单步 deadline 封顶（护栏语义，不追求精确）。
    async fn check_stop(&self, env: &Envelope, session_id: &str, budget: &ChatBudget, usage: &UsageAcc) -> Option<Value> {
        if let Some(v) = self.check_cancel(env, session_id).await {
            return Some(v);
        }
        if let Some(dl) = budget.deadline {
            if Instant::now() >= dl {
                tracing::info!(target: ID, session = %session_id, "budget exhausted: wall clock");
                self.trace(env, session_id, json!({"type": "error", "where": "budget", "message": "chat 预算耗尽：时长超限"})).await;
                return Some(json!({"ok": false, "error": {"code": "K508", "message": "chat 预算耗尽：时长超限"}}));
            }
        }
        if let Some(left) = budget.tokens_left {
            if usage.total() >= left {
                tracing::info!(target: ID, session = %session_id, used = usage.total(), budget = left, "budget exhausted: tokens");
                self.trace(env, session_id, json!({"type": "error", "where": "budget", "message": "chat 预算耗尽：token 超限"})).await;
                return Some(json!({"ok": false, "error": {"code": "K508", "message": "chat 预算耗尽：token 超限"}}));
            }
        }
        None
    }

    /// ReAct 主循环。委派深度由 `ChatReq.depth` 随链携带（0=顶层），
    /// 不使用插件级共享计数——并发下各链深度互不挤占。
    async fn chat_body(&self, env: &Envelope) -> Value {
        let Ok(req) = serde_json::from_value::<ChatReq>(env.payload.clone()) else {
            return json!({"ok": false, "error": {"code": "K400", "message": "chat 请求需 {session_id, user_text}"}});
        };
        // 开局清残留取消标志（chat 结束时也会清理——双保险，防误杀同 session 下轮对话）
        self.cancels.lock().unwrap().remove(&req.session_id);
        let out = self.chat_run(env, &req).await;
        self.cancels.lock().unwrap().remove(&req.session_id);
        out
    }

    async fn chat_run(&self, env: &Envelope, req: &ChatReq) -> Value {
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
            let sid = format!("{}-r{}", req.session_id, rounds);
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
                return self.finish(env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms).await;
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
                let execs = group
                    .iter()
                    .map(|tc| self.act_exec(env, &req.session_id, req.depth, tc, budget_snap));
                let done = futures::future::join_all(execs).await;
                let mut newly: HashSet<String> = HashSet::new();
                for (tc, (result, ms)) in group.iter().zip(done) {
                    // R9：load_skill 成功 → 新技能的已启用工具并入后续轮次清单
                    //（skill_loaded 事件已在 act_exec 内发出；HashSet 去重防重复并入）
                    if tc.name == RESERVED_LOAD_SKILL && result.get("ok") == Some(&json!(true)) {
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
        let sid = format!("{}-r{}", req.session_id, rounds);
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
            return self.finish(env, &req.session_id, resp, rounds, steps, sid, usage, llm_ms).await;
        }
        self.trace(env, &req.session_id, json!({"type": "error", "where": "max_rounds", "message": format!("agent loop exhausted max_rounds={max_rounds}")})).await;
        json!({"ok": false, "error": {"code": "K502", "message": format!("agent loop exhausted max_rounds={max_rounds}")}})
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
            "chat" => Ok(self.chat_body(&env).await),
            "cancel" => {
                // P2/T1：置位取消标志。Concurrent 语义保证本调用不被在途 chat 的锁阻塞。
                let sid = env.payload.get("session_id").and_then(Value::as_str).unwrap_or("");
                if sid.is_empty() {
                    Ok(json!({"ok": false, "error": {"code": "K400", "message": "cancel 需 session_id"}}))
                } else {
                    self.cancels.lock().unwrap().insert(sid.to_string());
                    tracing::info!(target: ID, session = %sid, "cancel requested");
                    Ok(json!({"ok": true, "session_id": sid, "note": "取消信号已置位；当前轮完成后中断"}))
                }
            }
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

    #[test]
    fn build_user_msg_routes_attachments() {
        // 隔离并行测试的 TEXT_ATTACH_LIMIT 串扰（截断行为由专项测试覆盖）
        let saved_limit = std::env::var("TEXT_ATTACH_LIMIT").ok();
        std::env::set_var("TEXT_ATTACH_LIMIT", "24000");

        // R3：无附件 → 纯文本消息，attachments 不出现
        let plain = build_user_msg("hi", None);
        assert_eq!(plain.content.as_deref(), Some("hi"));
        assert!(plain.attachments.is_none());

        // 图片 → 结构化 attachments（不内嵌 content）；文本 → 内嵌 content；其他二进制 → 仅注明
        let atts = vec![
            Attachment { name: "a.png".into(), mime: "image/png".into(), data_b64: "QUJD".into() },
            Attachment { name: "b.txt".into(), mime: "text/plain".into(), data_b64: "aGVsbG8=".into() }, // "hello"
            Attachment { name: "c.bin".into(), mime: "application/octet-stream".into(), data_b64: "AAA=".into() },
        ];
        let m = build_user_msg("看图", Some(&atts));
        let content = m.content.expect("content");
        assert!(content.starts_with("看图"));
        assert!(content.contains("[附件: b.txt]"), "文本附件内嵌 content");
        assert!(content.contains("hello"));
        assert!(content.contains("[附件: c.bin]"), "二进制附件注明类型不静默丢弃");
        assert!(!content.contains("a.png"), "图片附件不内嵌 content");

        let imgs = m.attachments.expect("images");
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].name, "a.png");
        assert_eq!(imgs[0].data_b64, "QUJD");

        match saved_limit {
            Some(v) => std::env::set_var("TEXT_ATTACH_LIMIT", v),
            None => std::env::remove_var("TEXT_ATTACH_LIMIT"),
        }
    }

    #[test]
    fn build_user_msg_truncates_oversized_text_attachment() {
        // R3：文本附件超限 → 截断并显式标注（模型必须感知内容不完整）
        let saved = std::env::var("TEXT_ATTACH_LIMIT").ok();
        std::env::set_var("TEXT_ATTACH_LIMIT", "4");
        let atts = [Attachment {
            name: "big.txt".into(),
            mime: "text/plain".into(),
            // "abcdefghij"（10 字符，上限 4）
            data_b64: "YWJjZGVmZ2hpag==".into(),
        }];
        let m = build_user_msg("q", Some(&atts));
        let c = m.content.expect("content");
        assert!(c.contains("truncated"), "截断必须标注: {c}");
        assert!(c.contains("[附件: big.txt]"));
        assert!(m.attachments.is_none(), "文本附件不进结构化字段");
        match saved {
            Some(v) => std::env::set_var("TEXT_ATTACH_LIMIT", v),
            None => std::env::remove_var("TEXT_ATTACH_LIMIT"),
        }
    }
}
