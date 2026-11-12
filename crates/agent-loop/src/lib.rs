//! ReAct 编排插件（Rust，InProcess 域）。
//!
//! 时间契约：主执行流为**状态机循环**（感知→规划→行动→观察→回跳），收敛于最终答案
//! 或 `max_rounds` 强制收敛。循环状态全部存于局部变量与会话记忆（memory 插件），
//! 本体 `&self` 无跨调用可变态（A1）。委派深度随调用链传播（`ChatReq.depth`），
//! 非插件级共享状态——并发下各链互不挤占委派额度。
//! 取消（P2/T1）：`cancel` op 置位会话标志，循环在轮次边界（每轮开头/工具波次后）
//! 与工具波次间（R1 补强：单轮多波工具时不等全轮跑完）轮询命中即收敛为 K499；语义为 Concurrent——否则 cancel 会在 per-plugin 锁后排队、
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
//!
//! 模块布局（R12，按关注点拆分；互引用走 `super::`，子模块入口 `pub(super)`）：
//! `chat`（主循环/LLM 规划）、`tools_exec`（工具波次/产物与来源登记）、
//! `subagent`（子代理委派）、`trace`（事件日志/流式旁路）、`compaction`（上下文压缩）、
//! `budget`（预算与取消）、`skills`（技能装配/提示词组装）、`context`（上下文策略）、
//! `contract`（Wire 类型）。本文件留守：插件结构、装配（new/impl Plugin）、
//! 跨插件调用封装与跨模块共享的常量/小工具。

mod budget;
mod chat;
mod compaction;
mod config;
mod context;
mod contract;
mod secrets;
mod skills;
mod subagent;
mod tools_defer; // G 轮：按需装载工具（deferred：schema 不进清单，load_tools 报名后下发）
mod turn;        // G 轮：回合对象（turn_id + trace 游标区间 + paused 暂停态 + 补播视图）
mod tools_exec;
mod trace;

pub use contract::{Attachment, ChatReq, LlmChatResp, MemoryMsg, StepRecord, ToolCall, ToolObs, ToolObsChanges, ToolSpec};

use agent_kernel_sdk::*;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const ID: &str = "agent-loop";
pub const ID_MEMORY: &str = "memory";
pub const ID_LLM: &str = "llm-adapter";
pub const ID_TOOLS: &str = "tools";
pub const ID_ASSETS: &str = "assets";

/// capability 寻址（K2，内核 v0.1.4）：跨插件调用按能力寻址，提供者可被同名/异名
/// 实现替换而本 crate 零改动。memory 侧所有 session 域操作（get/append/summarize/
/// trace.append/trace.read）统一寻址 `memory.session`——与本插件 manifest 硬依赖
/// 声明一致（声明什么就寻址什么，消除"用了未声明的 capability"的旧不一致）。
pub const CAP_MEMORY: &str = "memory.session";
pub const CAP_LLM: &str = "llm.chat";
pub const CAP_TOOLS: &str = "tools.exec";
pub const CAP_ASSETS: &str = "assets.registry";

/// 保留工具名：路由 assets（读正文 + 装配套工具，R9b），不下发 tools（见 03 §3）。
pub const RESERVED_LOAD_SKILL: &str = "load_skill";

/// 保留工具名：子代理委派（Phase 3-3）——复用 agent.chat 全链路（新 session_id），不下发 tools。
pub const RESERVED_TASK: &str = "task";

/// 保留工具名：技能安装（R9）——assets skills.load 取声明 → tools.install 装载配套工具
/// （preset 装载即启用并视同已加载；用户技能进池待界面确认），trace `skill_installed`
/// 事件供前端内联卡与一键启用。不下发 tools。
pub const RESERVED_SKILL_INSTALL: &str = "skill_install";

/// 保留工具名：按需装载报名（G 轮）——`ToolSpec.deferred` 的工具不进下发清单，
/// 模型调本工具报过名之后，完整 schema 自下一轮起下发（见 tools_defer.rs）。不下发 tools。
pub const RESERVED_LOAD_TOOLS: &str = "load_tools";

/// 保留工具名：向用户提问并**暂停回合**（G 轮）——与「把问题写进最终答复然后结束」
/// 的区别是回合不结束：用户回答后按 tool_call_id 接回同一条 tool 结果，工作集不重建。
pub const RESERVED_ASK_USER: &str = "ask_user";

/// RSI 策略钩子（Phase 1 接缝，**默认关闭**）：由 rsi-agent 的配置显式开启。
/// - `contract_first`：非平凡任务的第一个工具调用前要求先写验收清单（可落 todo）；
/// - `verify_before_final`：最终答复前要求逐条核对验收清单。
/// 关闭时循环行为与启用前完全一致（不引入任何新分支）；提醒只发一次，避免死循环。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoopPolicy {
    pub contract_first: bool,
    pub verify_before_final: bool,
}

impl LoopPolicy {
    /// 从 env 解析（config.json agent 段可写 `rsi_contract_first` / `rsi_verify_before_final`）。
    pub fn from_env() -> Self {
        Self {
            contract_first: env_flag("RSI_CONTRACT_FIRST"),
            verify_before_final: env_flag("RSI_VERIFY_BEFORE_FINAL"),
        }
    }
}

fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
        .unwrap_or(false)
}

/// 验收清单痕迹判定（提示词层面）：接受中英文关键词；缺省无痕迹即视为「未核对」。
pub fn has_acceptance_marker(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    ["验收", "acceptance", "checklist", "核对清单"]
        .iter()
        .any(|k| t.contains(k))
}

const CONTRACT_REMINDER: &str = "[policy:contract_first] 先写验收清单再行动：请在不调用工具的情况下，用 todo 或文本列出本次任务的验收标准（逐条、可核对），然后再开始调用工具。";
const VERIFY_REMINDER: &str = "[policy:verify_before_final] 最终答复前请逐条核对验收清单，并在答复中给出每条标准的满足情况（满足/不满足/原因）。";

/// 各转发步的相对截止（A2：Envelope.deadline 为相对时长）。tools 闸可配（O3）：
/// env `TOOLS_DEADLINE_SECS` 覆盖缺省 60s（<30s 回退），重构建/全仓扫描类命令按需放宽。
/// LLM_DEADLINE=600s：深度思考模型长推理是常态（110s 时代被误伤）——单步上限放宽到
/// 10 分钟覆盖全部商用推理模型流式思考；必须与 llm_adapter PROVIDER_DEADLINE=590s
/// 配对（provider 先收敛 10s 报可重试 timeout，避免 gRPC abort 不中断 guest 线程导致
/// 的涓流占坑，见 base.py 头注）；整轮预算 CHAT_BUDGET_SECS 缺省 900s 需 ≥ 单步闸，
/// 否则长思考 + 工具轮会在轮边界被 K508 掐断。
const MEM_DEADLINE: Duration = Duration::from_secs(5);
// O3：tools 闸可配——重构建/重索引/全仓扫描类命令 60s 常不够（活体 trace 实录 K502 ×3），
// env `TOOLS_DEADLINE_SECS` 覆盖（<30s 视为误配回退 60，语义同 llm_step_deadline）。
fn tools_deadline() -> Duration {
    std::env::var("TOOLS_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s >= 30)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(60))
}
const LLM_DEADLINE: Duration = Duration::from_secs(600);
const ASSETS_DEADLINE: Duration = Duration::from_secs(5);

/// LLM 单步 deadline 单一事实源（LOOP-P2/A4）：agent-loop 总闸与 llm_adapter 的
/// PROVIDER_DEADLINE（= 总闸 - 10s 余量）同读 env `LLM_STEP_DEADLINE_SECS`（缺省 600），
/// 配对关系由代码保证，不再依赖两侧注释互指人工同步（改其一漏其余 → 涓流占坑/K502
/// 雪崩复发的历史不再可能）。env < 30s 视为误配回退缺省；CHAT_BUDGET_SECS（整轮预算）
/// 仍独立配置，需 ≥ 本闸（文档约束不变）。
fn llm_step_deadline() -> Duration {
    std::env::var("LLM_STEP_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s >= 30)
        .map(Duration::from_secs)
        .unwrap_or(LLM_DEADLINE)
}

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
Presemble precise tool arguments: read before writing files, and prefer edit_file over rewriting whole files. \
Efficiency discipline (these bound the tool budget, not the task): \
(a) BATCH independent calls into one round — if two calls do not depend on each other's result, \
issue them together instead of spending a round each. \
(b) PREFER dedicated tools over shell: grep/symbols_search to find content or definitions, read_file to read, \
list_dir to list; use bash only when a real shell is required (tests, git, build). \
(c) CONVERGE, do not thrash: if two attempts at the same goal yield no new information, change strategy \
instead of repeating the call with cosmetic variations.";

pub struct AgentLoopPlugin {
    max_rounds: usize,
    /// RSI 策略钩子（默认关闭；见 `LoopPolicy`）。
    policy: LoopPolicy,
    manifest: Manifest,
    host: OnceLock<Arc<dyn HostApi>>,
    /// 子会话计数（sub_session id 唯一性；并发下各链父会话 id 不同，全局递增即可）。
    sub_counter: AtomicU64,
    /// 取消令牌（P2/T1）：被请求取消的 session id 集合。`cancel` op 置位，循环在
    /// 轮次边界 take（命中即清）；chat 结束时兜底清理，防残留标志误杀同 session 下轮对话。
    cancels: Mutex<HashSet<String>>,
    /// 回合开始时间（per session）：兜底产物扫描的「新鲜度」基准——只有本轮生成
    /// （mtime ≥ 回合开始）的文件才登记为 artifact，ls/git 输出里的历史文件不再刷卡。
    turn_starts: Mutex<std::collections::HashMap<String, std::time::SystemTime>>,
    /// 流式 sid 单调序（sid 唯一性修复）：per session 递增，首个 sid 以 unix 毫秒为
    /// 基准种子——跨对话回合（rounds 重置）不再生成相同 sid；跨 host 重启也因毫秒
    /// 种子不同而不碰撞。形状保持 `{session}-r{digits}`：llm-adapter 的
    /// `_SID_RE`（`^(.*)-r\d+$`）无需改动即可反解会话 id（abort/cancel 对位）。
    sid_seq: Mutex<std::collections::HashMap<String, u64>>,
    /// 子代理事件镜像（R11）：sub_session → parent_session。run_subagent 委派期间
    /// 注册，trace() 据此把子会话事件同步写一份到父 trace（打 `sub` 标签），前端
    /// 过程框内的「子代理」框据此实时/重放渲染——否则子代理只有一个静态图标，
    /// 过程不可见（用户感知为卡住）。
    mirrors: Mutex<std::collections::HashMap<String, String>>,
    /// 已发 sid 的全局下限（A4 卫生）：sid_seq 会话表项在 chat 结束时逐出后，
    /// 新 seed 取 `max(当前毫秒, floor+1)`——严格大于历史上任何已发 sid，跨回合
    /// 不碰撞语义在逐出后依然成立。
    sid_floor: AtomicU64,
    /// 同会话并发闸（LOOP-P2/A1）：两个并发 chat 用同一 session_id 会互清取消标志、
    /// 互踩 turn_starts 与流式 sid（低概率、高混淆度）。值是 per-session 的 tokio Mutex，
    /// chat_body 入口 try_lock——拿不到即 409 拒绝，不排队不阻塞。
    session_chats: Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// memory.append 连续失败计数（LOOP-P2/A2）：持久层与内存工作集开始分叉的信号，
    /// 成功即清零；达到阈值由关键写入点熔断本轮，防错位静默放大。
    append_fails: AtomicU64,
}

/// 构造插件实例（`Arc<dyn Plugin>`）：策略钩子按 env 解析（缺省关闭）。
pub fn new(max_rounds: usize) -> PluginInstance {
    new_with_policy(max_rounds, LoopPolicy::from_env())
}

/// 构造插件实例并显式注入策略钩子（发行形态用；`LoopPolicy::default()` = 行为不变）。
pub fn new_with_policy(max_rounds: usize, policy: LoopPolicy) -> PluginInstance {
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
        policy,
        manifest,
        host: OnceLock::new(),
        sub_counter: AtomicU64::new(0),
        cancels: Mutex::new(HashSet::new()),
        turn_starts: Mutex::new(std::collections::HashMap::new()),
        sid_seq: Mutex::new(std::collections::HashMap::new()),
        mirrors: Mutex::new(std::collections::HashMap::new()),
        sid_floor: AtomicU64::new(0),
        session_chats: Mutex::new(std::collections::HashMap::new()),
        append_fails: AtomicU64::new(0),
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

impl AgentLoopPlugin {
    /// 生成下一轮流式 sid（sid 唯一性修复）：`{session}-r{N}`，N 为 per session 单调
    /// 递增序号，首个 sid 以 unix 毫秒为种子。跨对话回合不再碰撞（旧 bug：每回合
    /// rounds 从 1 重新计数，前端 doneSids 去重把后一回合的流式动画误判为已定稿帧）。
    pub(crate) fn next_sid(&self, session_id: &str) -> String {
        let n = {
            let mut m = self.sid_seq.lock().unwrap();
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(1);
            let n = match m.get(session_id) {
                Some(v) => v + 1,
                // 表项被逐出（A4）后：seed 取 max(当前毫秒, 全局下限+1)——
                // 严格大于历史上任何已发 sid，跨回合不碰撞语义不变。
                None => seed.max(self.sid_floor.load(Ordering::SeqCst) + 1),
            };
            m.insert(session_id.to_string(), n);
            n
        };
        self.sid_floor.fetch_max(n, Ordering::SeqCst);
        format!("{session_id}-r{n}")
    }

    /// 跨插件调用便捷封装（K2 寻址契约 + S1 贯穿，v0.1.7 消费）：按 capability 寻址，
    /// 复用内核 dispatch 全链（B5 快照 / B2 panic 隔离 / B4 背压 / deadline 竞争全部继承）。
    ///
    /// trace/priority：显式延续调用链的 trace_id 与 priority——内核 `call_capability_as`
    /// 使寻址路径与 call_plugin 同权；此前的"每次新 trace"缺口随内核 v0.1.7 闭合。
    /// 子代理链（run_subagent）仍走 `call_plugin`（trace 连续 + 整体 deadline 语义不变）。
    async fn call(
        &self,
        src: &Envelope,
        capability: &str,
        payload: Value,
        deadline: Duration,
    ) -> Result<Value, KernelError> {
        let host = self
            .host
            .get()
            .ok_or_else(|| KernelError::Internal("agent-loop: host not initialized".into()))?;
        host.call_capability_as(capability, src.trace_id, src.priority, payload, deadline)
            .await
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
                    // T7/T8（v0.1.7 消费）：联动 tools `abort` op——终止该会话运行中的
                    // 工具子进程，使"点停止"对工具执行段也生效。System 优先级走专用
                    // 泳道（T8），满载 chat 下不再排队；即发即忘 + 2s 上限；失败仅
                    // 记录——轮次边界取消仍兜底。
                    if let Some(host) = self.host.get() {
                        let host = host.clone();
                        let payload = json!({"op": "abort", "session_id": sid});
                        tokio::spawn(async move {
                            if let Err(e) = host
                                .call_capability_as(
                                    CAP_TOOLS,
                                    TraceId::new(),
                                    Priority::System,
                                    payload,
                                    std::time::Duration::from_secs(2),
                                )
                                .await
                            {
                                tracing::debug!(target: ID, "tools abort dispatch failed: {e}");
                            }
                        });
                    }
                    Ok(json!({"ok": true, "session_id": sid, "note": "取消信号已置位；当前轮完成后中断"}))
                }
            }
            // G 轮：回合补播。
            //
            // 为什么做成 op 而不是 host 路由：host 在核心禁写区（改它要走补丁且牵动
            // 前端契约），而**补播本质是「读 turn 文件 + 算游标」**——纯读、无副作用，
            // 放在插件里就是一行 dispatch；等前端真要接时再在 host 加薄路由即可。
            // 返回游标区间而**不是事件本体**：事件由调用方经 `trace.read(after=…)`
            // 分页拉（既有能力），于是补播不引入第二个事件通道，也就不可能与 trace 不一致。
            "turn/replay" => {
                let sid = env.payload.get("session_id").and_then(Value::as_str).unwrap_or("");
                let tid = env.payload.get("turn_id").and_then(Value::as_str).unwrap_or("");
                let after = env.payload.get("after_seq").and_then(Value::as_u64).unwrap_or(0);
                let Some(store) = turn::TurnStore::open() else {
                    return Ok(json!({"ok": false, "error": {"code": "K500",
                        "message": "回合目录不可用（RSI_TURNS_DIR / RSI_HOME / WORKSPACE_ROOT 均未解析出）"}}));
                };
                // turn_id 给了就精确定位；没给取该会话最近一条未终结的（前端刷新后
                // 通常只知道 session_id，这是补播最常见的入口）。
                let found = if tid.is_empty() {
                    store.list_session(sid).into_iter().find(|t| {
                        !turn::is_terminal(t.get("status").and_then(Value::as_str).unwrap_or(""))
                    })
                } else {
                    store.load(tid)
                };
                match found {
                    Some(t) if sid.is_empty() || t.get("session_id").and_then(Value::as_str) == Some(sid) => {
                        Ok(turn::replay_view(&t, after))
                    }
                    Some(_) => Ok(json!({"ok": false, "error": {"code": "K403",
                        "message": "该回合不属于此会话"}})),
                    None => Ok(json!({"ok": false, "error": {"code": "K404",
                        "message": "找不到可补播的回合（可能已收敛且超出保留上限）"}})),
                }
            }
            // G 轮：列出一段会话的回合（前端「历史回合」列表与排障用）。
            "turn/list" => {
                let sid = env.payload.get("session_id").and_then(Value::as_str).unwrap_or("");
                let Some(store) = turn::TurnStore::open() else {
                    return Ok(json!({"ok": false, "error": {"code": "K500", "message": "回合目录不可用"}}));
                };
                let items: Vec<Value> = store
                    .list_session(sid)
                    .into_iter()
                    .map(|t| json!({
                        "turn_id": t.get("turn_id"),
                        "status": t.get("status"),
                        "started_at": t.get("started_at"),
                        "ended_at": t.get("ended_at"),
                        "start_cursor": t.get("start_cursor"),
                        "end_cursor": t.get("end_cursor"),
                        "has_pending": t.get("pending").map(|p| !p.is_null()).unwrap_or(false),
                    }))
                    .collect();
                Ok(json!({"ok": true, "session_id": sid, "count": items.len(), "turns": items}))
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
    use super::chat::build_user_msg;
    use super::tools_exec::ObsPlan;
    use super::*;

    // O3：env 互相污染防不住并发——三测合并串行跑（语义：缺省→覆盖→误配回退）
    #[test]
    fn tools_deadline_env_semantics() {
        let saved = std::env::var("TOOLS_DEADLINE_SECS").ok();
        std::env::remove_var("TOOLS_DEADLINE_SECS");
        assert_eq!(tools_deadline(), Duration::from_secs(60)); // 缺省
        std::env::set_var("TOOLS_DEADLINE_SECS", "120");
        assert_eq!(tools_deadline(), Duration::from_secs(120)); // 覆盖生效
        std::env::set_var("TOOLS_DEADLINE_SECS", "5");
        assert_eq!(tools_deadline(), Duration::from_secs(60)); // 低于下限回退
        match saved {
            Some(v) => std::env::set_var("TOOLS_DEADLINE_SECS", v),
            None => std::env::remove_var("TOOLS_DEADLINE_SECS"),
        }
    }

    #[test]
    fn extract_sources_dedupes_and_validates() {
        // R16：计划经遗留注册表解析（obs 未声明回落路径）+ 元数据直构路径各验一遍
        let legacy = |name: &str| ObsPlan::resolve(name, None);
        let inner = json!({
            "query": "q", "engine": "bing",
            "results": [
                {"title": "A", "url": "https://a.example/1", "snippet": "s"},
                {"title": "B", "url": "https://b.example/2", "snippet": "s"},
                {"title": "A2", "url": "https://a.example/1", "snippet": "s"},  // 重复 URL 去重
                {"title": "X", "url": "javascript:alert(1)", "snippet": "s"},   // 非 http 白名单外
                {"title": "", "url": "  https://c.example/3  ", "snippet": "s"} // 空标题用 URL 充当
            ]
        });
        let items = AgentLoopPlugin::extract_sources(&inner, &legacy("web_search"));
        assert_eq!(items.len(), 3, "{items:?}");
        assert_eq!(items[0], ("A".into(), "https://a.example/1".into()));
        assert_eq!(items[1], ("B".into(), "https://b.example/2".into()));
        assert_eq!(items[2], ("https://c.example/3".into(), "https://c.example/3".into()));

        // web_read：单 URL；其他工具：空（遗留路径）
        assert_eq!(
            AgentLoopPlugin::extract_sources(&json!({"url": "https://r.example/x"}), &legacy("web_read")).len(),
            1
        );
        assert!(AgentLoopPlugin::extract_sources(&json!({"output": "https://x.example"}), &legacy("bash")).is_empty());

        // 元数据路径：改名工具带胸牌同样提取（R16 核心行为）
        let obs = ToolObs { sources: Some("result_list".into()), ..Default::default() };
        let items = AgentLoopPlugin::extract_sources(&inner, &ObsPlan::resolve("search_web", Some(&obs)));
        assert_eq!(items.len(), 3, "胸牌声明 result_list 的改名工具应同形提取");
    }

    #[test]
    fn extract_sources_caps_at_ten() {
        let results: Vec<Value> = (0..30)
            .map(|i| json!({"title": format!("t{i}"), "url": format!("https://e.example/{i}")}))
            .collect();
        let items =
            AgentLoopPlugin::extract_sources(&json!({ "results": results }), &ObsPlan::resolve("web_search", None));
        assert_eq!(items.len(), 10, "溯源卡上限 10 条");
    }

    #[test]
    fn file_change_event_only_for_successful_file_tools() {
        let ok = |path: &str| json!({"ok": true, "result": {"path": path}});
        let legacy = |name: &str| ObsPlan::resolve(name, None);
        // 成功 write/edit → 事件（path 归一正斜杠；遗留注册表 op 映射不变）
        let ev = AgentLoopPlugin::file_change_events_for(&ok(r"D:\ws\outputs\a.md"), &legacy("write_file"))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(ev["path"], json!("D:/ws/outputs/a.md"), "反斜杠归一");
        assert_eq!(ev["op"], json!("write"));
        let ev = AgentLoopPlugin::file_change_events_for(&ok("x.md"), &legacy("edit_file"))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(ev["op"], json!("edit"));
        // 失败 / 非文件工具 / 缺 path → 不登记
        assert!(AgentLoopPlugin::file_change_events_for(&json!({"ok": false}), &legacy("write_file")).is_empty());
        assert!(AgentLoopPlugin::file_change_events_for(&ok("x"), &legacy("bash")).is_empty());
        assert!(AgentLoopPlugin::file_change_events_for(&json!({"ok": true, "result": {}}), &legacy("edit_file")).is_empty());
        // 元数据路径：改名工具带胸牌产出同形事件（op 取声明值）
        let obs = ToolObs {
            artifacts: Some("structured".into()),
            changes: Some(ToolObsChanges { op: "note".into(), from: None }),
            ..Default::default()
        };
        let ev = AgentLoopPlugin::file_change_events_for(&ok("outputs/n.md"), &ObsPlan::resolve("create_note", Some(&obs)))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(ev["op"], json!("note"), "胸牌声明的 op 原样生效");
        assert_eq!(ev["path"], json!("outputs/n.md"));
        // 未知取值降级为未声明（向后兼容：新枚举值不炸旧编排层）——未知 from 按
        // 缺省 single_path 行事（行为级断言：结果 path 照常成单事件）
        let obs_bad = ToolObs { changes: Some(ToolObsChanges { op: "x".into(), from: Some("future_shape".into()) }), ..Default::default() };
        let ev = AgentLoopPlugin::file_change_events_for(&ok("y.md"), &ObsPlan::resolve("future_tool", Some(&obs_bad)))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(ev["path"], json!("y.md"), "未知 from 值按缺省 single_path 处理");
    }

    #[test]
    fn next_sid_monotonic_and_shape_stable() {
        // 直接构造结构体（tests 模块在 crate root 子级，可访问私有字段）
        let p = AgentLoopPlugin {
            max_rounds: 8,
            policy: LoopPolicy::default(),
            manifest: Manifest {
                name: PluginId::new(ID),
                kind: PluginKind::Orchestrator,
                version: Version::new(0, 1, 0),
                api_version: ApiVersion::new(1, 0),
                capabilities: vec![Capability::new("agent.chat")],
                dependencies: vec![],
                domain: Domain::InProcess,
                semantics: Semantics::Concurrent,
                priority: 1,
                max_inflight: Some(8),
                fuel_limit: None,
                host_timeout_ms: None,
                epoch_interval_ms: None,
                subscriptions: vec![],
            },
            host: OnceLock::new(),
            sub_counter: AtomicU64::new(0),
            cancels: Mutex::new(HashSet::new()),
            turn_starts: Mutex::new(std::collections::HashMap::new()),
            sid_seq: Mutex::new(std::collections::HashMap::new()),
            mirrors: Mutex::new(std::collections::HashMap::new()),
            sid_floor: AtomicU64::new(0),
            session_chats: Mutex::new(std::collections::HashMap::new()),
            append_fails: AtomicU64::new(0),
        };
        let s1 = p.next_sid("s-x");
        let s2 = p.next_sid("s-x");
        let s3 = p.next_sid("s-x#sub-1");
        let digits_tail = |s: &str| s.rsplit("-r").next().is_some_and(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()));
        // 形状保持 {session}-r{digits}：llm-adapter `_SID_RE` 反解会话 id 无需改动
        assert!(digits_tail(&s1) && digits_tail(&s2) && digits_tail(&s3), "{s1} {s2} {s3}");
        assert_ne!(s1, s2, "同一会话相邻 sid 必须不同（跨对话回合唯一）");
        assert!(s3.starts_with("s-x#sub-1-r"), "子会话独立计数: {s3}");
    }

    #[test]
    fn scan_artifact_paths_narrow_gate() {
        // 文本兜底双闸：bash 输出里 rg/ls/git 列出的整仓 .md/源码不刷卡
        let text = "README.md crates/agent-loop/PLAN.md docs/02-架构设计.md src/main.rs \
                    outputs/report.md outputs/关于开学.docx 泥石流灾害预警防范的通知.docx report.xlsx";
        let got = AgentLoopPlugin::scan_artifact_paths(text, "outputs");
        // 产物目录内放行（md 也收）+ 二进制成品放行（任意位置）
        assert!(got.contains(&"outputs/report.md".to_string()), "{got:?}");
        assert!(got.iter().any(|s| s.ends_with("关于开学.docx")), "{got:?}");
        assert!(got.contains(&"泥石流灾害预警防范的通知.docx".to_string()), "{got:?}");
        assert!(got.contains(&"report.xlsx".to_string()), "{got:?}");
        // 整仓文档 / 源码不上卡
        assert!(!got.iter().any(|s| s.contains("README")), "{got:?}");
        assert!(!got.iter().any(|s| s.contains("PLAN.md")), "{got:?}");
        assert!(!got.iter().any(|s| s.contains("docs/")), "{got:?}");
        assert!(!got.iter().any(|s| s.ends_with(".rs")), "{got:?}");
    }

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
    fn effective_result_limit_scales_by_tool() {
        // X2 回归锚：web_read 实测 50% 的网页结果曾落在 1× 被腰斩 → 现与 read_file 同档 4×
        use crate::config::AgentLoopConfig;
        use crate::tools_exec::effective_result_limit;
        let saved = std::env::var("TOOL_RESULT_LIMIT").ok();
        std::env::set_var("TOOL_RESULT_LIMIT", "8000");
        let cfg = AgentLoopConfig::from_env(10);
        assert_eq!(effective_result_limit("read_file", &cfg), 32_000);
        assert_eq!(effective_result_limit("web_read", &cfg), 32_000);
        assert_eq!(effective_result_limit("grep", &cfg), 24_000);
        assert_eq!(effective_result_limit("symbols_search", &cfg), 24_000);
        assert_eq!(effective_result_limit("bash", &cfg), 8_000);
        // 0 = 禁用语义不变
        std::env::set_var("TOOL_RESULT_LIMIT", "0");
        let off = AgentLoopConfig::from_env(10);
        assert_eq!(effective_result_limit("web_read", &off), 0);
        match saved {
            Some(v) => std::env::set_var("TOOL_RESULT_LIMIT", v),
            None => std::env::remove_var("TOOL_RESULT_LIMIT"),
        }
    }

    #[test]
    fn self_extension_section_gated_by_workspace_reachability() {
        let saved = std::env::var("WORKSPACE_ROOT").ok();
        std::env::set_var("WORKSPACE_ROOT", r"C:\ws");

        // root 为空 → 不授权
        assert!(AgentLoopPlugin::self_extension_section("", false).is_none());
        // skills 根在 workspace 外 → 不授权
        assert!(AgentLoopPlugin::self_extension_section(r"D:\elsewhere\skills", false).is_none());
        // skills 根在 workspace 内 → 授权段含路径与 write_file 指引
        let section = AgentLoopPlugin::self_extension_section(r"C:\ws\skills", false)
            .expect("in-workspace");
        assert!(section.contains(r"C:\ws\skills"));
        assert!(section.contains("write_file"));
        assert!(section.contains("Skill self-extension"));
        // B2 按需注入：默认 brief 不含完整打包协议，full=true 才含
        assert!(!section.contains("### Skill packaging with companion tools"), "brief 不得含长教程");
        let full = AgentLoopPlugin::self_extension_section(r"C:\ws\skills", true).expect("full");
        assert!(full.contains("### Skill packaging with companion tools"));
        assert!(full.contains("tools.json") && full.contains("skill_install"));
        assert!(full.len() > section.len(), "full 必须比 brief 长");

        match saved {
            Some(v) => std::env::set_var("WORKSPACE_ROOT", v),
            None => std::env::remove_var("WORKSPACE_ROOT"),
        }
    }

    /// B2 触发词判定：明确要求造技能才注入长教程，普通请求不触发。
    #[test]
    fn want_skill_build_detection() {
        // 命中：名词 + 动作
        assert!(AgentLoopPlugin::user_wants_skill_build("帮我做一个技能"));
        assert!(AgentLoopPlugin::user_wants_skill_build("把这个工作沉淀成技能"));
        assert!(AgentLoopPlugin::user_wants_skill_build("create a new skill for linting"));
        assert!(AgentLoopPlugin::user_wants_skill_build("package this as a skill"));
        // 不命中：无名词，或只有名词无动作
        assert!(!AgentLoopPlugin::user_wants_skill_build("跑一下测试"));
        assert!(!AgentLoopPlugin::user_wants_skill_build("读 specs/call-efficiency/tasks.md 继续"));
        assert!(!AgentLoopPlugin::user_wants_skill_build("有哪些技能可用？"));
    }

    /// B1 效率约束：内置提示词必须含批量/专用工具/收敛三条，且不删原有纪律。
    #[test]
    fn default_prompt_has_efficiency_discipline() {
        assert!(DEFAULT_SYSTEM_PROMPT.contains("BATCH"), "缺少批量约束");
        assert!(DEFAULT_SYSTEM_PROMPT.contains("PREFER dedicated tools"), "缺少专用工具优先");
        assert!(DEFAULT_SYSTEM_PROMPT.contains("CONVERGE"), "缺少收敛约束");
        // 原有质量纪律不得被效率约束挤掉
        assert!(DEFAULT_SYSTEM_PROMPT.contains("read before writing files"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("edit_file"));
        // 效率约束必须声明「不缩减任务范围」，防模型以少做换少调
        assert!(DEFAULT_SYSTEM_PROMPT.contains("not the task"));
    }

    #[test]
    fn build_user_msg_routes_attachments() {
        // R3：无附件 → 纯文本消息，attachments 不出现
        let plain = build_user_msg("hi", None, 24000);
        assert_eq!(plain.content.as_deref(), Some("hi"));
        assert!(plain.attachments.is_none());

        // 图片 → 结构化 attachments（不内嵌 content）；文本 → 内嵌 content；其他二进制 → 仅注明
        let atts = vec![
            Attachment { name: "a.png".into(), mime: "image/png".into(), data_b64: "QUJD".into() },
            Attachment { name: "b.txt".into(), mime: "text/plain".into(), data_b64: "aGVsbG8=".into() }, // "hello"
            Attachment { name: "c.bin".into(), mime: "application/octet-stream".into(), data_b64: "AAA=".into() },
        ];
        let m = build_user_msg("看图", Some(&atts), 24000);
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
    }

    #[test]
    fn build_user_msg_truncates_oversized_text_attachment() {
        // R3：文本附件超限 → 截断并显式标注（模型必须感知内容不完整）。
        // E3 后上限为注入参数（不再读 env），直接传 4 验证截断行为。
        let atts = [Attachment {
            name: "big.txt".into(),
            mime: "text/plain".into(),
            // "abcdefghij"（10 字符，上限 4）
            data_b64: "YWJjZGVmZ2hpag==".into(),
        }];
        let m = build_user_msg("q", Some(&atts), 4);
        let c = m.content.expect("content");
        assert!(c.contains("truncated"), "截断必须标注: {c}");
        assert!(c.contains("[附件: big.txt]"));
        assert!(m.attachments.is_none(), "文本附件不进结构化字段");
    }

    #[test]
    fn loop_policy_defaults_off_and_marker_detection() {
        // Phase 1 接缝：默认策略必须关闭（行为零变化）；显式构造可开启。
        assert_eq!(LoopPolicy::default(), LoopPolicy { contract_first: false, verify_before_final: false });
        let on = LoopPolicy { contract_first: true, verify_before_final: true };
        assert_ne!(on, LoopPolicy::default());

        // 验收痕迹：中英文关键词命中；普通答复视为未核对。
        assert!(has_acceptance_marker("验收标准：1. ... 2. ..."));
        assert!(has_acceptance_marker("Acceptance criteria checked"));
        assert!(has_acceptance_marker("todo checklist"));
        assert!(!has_acceptance_marker("已完成，结果如下"));
    }
}
