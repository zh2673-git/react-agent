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
mod queue;   // Q 轮 Phase 3：服务端显式队列（全局有界状态，不动插件结构体）
mod board_ctx; // C 轮：任务看板上下文装载（@引用 + 在办任务摘要，只读 .rsi/board.json）
mod profile_ctx; // D 轮：习惯画像装载（.rsi/profile.json 的有界摘要，只读；写口是 scripts/profile.py）
mod memory_ctx; // G 轮：记忆 L2 时间轴（.rsi/memory/L3/index.json 的元数据，只读；写口是 scripts/memory_consolidate.py）
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

/// 进程级 env 是共享可变状态（`cargo test` 多测同进程并行跑）。
///
/// 实测过的伤害：S 轮新增的 `routing_hint_declares_three_tiers_and_tie_break` 与既有的
/// `task_board_section_requires_cli_and_scales_by_flag` 都要 `set_var("RSI_HOME")`，
/// 两个线程交错时后者读到前者布置的空目录 → 提示段为空 → 既有用例红。
/// 这类红有个恶劣特征：**换个跑法就绿**（单线程跑不复现），于是最容易被当成 flaky 忽略，
/// 而它每次在 CI 上的结论都是随机的。
///
/// v12：锁必须**跨模块**共用。`board_ctx::tests` 与本文件的测试都改
/// `RSI_HOME` / `WORKSPACE_ROOT`，各自持有自己的 `ENV_LOCK` 等于没加锁——实测：
/// 单包跑 `-p react-agent-agent-loop --lib` 全绿，`--workspace --lib` 时
/// `board_follows_rsi_home_not_workspace` 读到本文件用例留下的 `RSI_HOME` 而红。
/// 锁的粒度要覆盖它保护的状态，而不是「看起来相关的那几个用例」。
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
/// trace.append/trace.read/trace.meta/session.skills）统一寻址 `memory.session`——与本插件
/// manifest 硬依赖声明一致（声明什么就寻址什么，消除"用了未声明的 capability"的旧不一致）。
///
/// 后两个 op 是 P1 修复引入的**只读汇总**接口，专治「分页接口当总长度用」这一类 bug：
/// - `trace.meta` → `{ok, exists, size, mtime_ms, total_events, end_cursor}`：真实末尾
///   绝对游标（= trace 完整有效事件数）。`trace.read.next` 是**分页消费位置**，单页
///   ≤800 条 / 约 1MB，长 trace 上恒等于首页边界，绝不能当回合终点（见 chat.rs 三处）。
/// - `session.skills` → `{ok, skills, total_events}`：会话级已加载技能集合（扫完整
///   trace 去重），替代编排层逐页重放 `skill_loaded`（只读首页必漏长会话）。
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

/// 子会话镜像表（sub -> parent）里 parent==sid 的全部子会话 id，排序保证确定性。
fn collect_children(mirrors: &std::collections::HashMap<String, String>, parent: &str) -> Vec<String> {
    let mut v: Vec<String> = mirrors
        .iter()
        .filter(|(_, p)| p.as_str() == parent)
        .map(|(c, _)| c.clone())
        .collect();
    v.sort();
    v
}

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
    /// P4b 任务级累计花费账：外层键 = 根会话 id（`budget_root`），内层键 = 该任务下每条
    /// 调用链的会话 id（顶层即根会话 id，子代理为其 sub_session），值 = 该链已消耗 token。
    ///
    /// 为什么需要它（改前必读）：修复前 `budget_snap` 只把「父链当前剩余」传给子代理，
    /// 于是**同一父轮的多个子代理各自看到同一份剩余**——N 个子代理并发跑，实际可超支
    /// N 倍，token 预算闸在最贵的场景（扇出探索）里彻底失真。任务账把「整条链已花多少」
    /// 变成显式事实，派发时按「总额 − 全体已花」切分，超支被结构性封住。
    spent: Mutex<std::collections::HashMap<String, std::collections::HashMap<String, u64>>>,
}

impl AgentLoopPlugin {
    /// P1 Cancel propagation：置位取消标志，并联动其在办子会话（mirrors 表里
    /// parent==sid 的子会话；子会话阻塞在父的工具波次里，父只在轮次边界轮询自己的
    /// 标志，不联动就要等子会话跑完）。返回被联动的子会话 id，供 tools abort 逐个投递。
    fn cancel_marks_children(&self, sid: &str) -> Vec<String> {
        self.cancels.lock().unwrap().insert(sid.to_string());
        let children = collect_children(&self.mirrors.lock().unwrap(), sid);
        for c in &children {
            self.cancels.lock().unwrap().insert(c.clone());
        }
        children
    }

    /// P4b 记账：写入某条调用链在本任务内的累计花费。
    ///
    /// 语义是**绝对值覆盖**（调用方传该链的 `usage.total()`，不是增量）：同一
    /// (root, session) 只保留最新值，重复调用幂等；不同链互不覆盖，求和即任务总额。
    fn record_spent(&self, root: &str, session: &str, spent: u64) {
        let mut m = self.spent.lock().unwrap();
        m.entry(root.to_string()).or_default().insert(session.to_string(), spent);
    }

    /// P4b：本任务全体成员（父链 + 所有子代理）已花 token 总额。
    /// 派发子代理时用它算「总额 − 已花」，从而让兄弟子代理分食同一份剩余。
    fn task_spent(&self, root: &str) -> u64 {
        self.spent
            .lock()
            .unwrap()
            .get(root)
            .map(|m| m.values().copied().fold(0u64, |a, b| a.saturating_add(b)))
            .unwrap_or(0)
    }
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
        // Q 轮：槽位可配（缺省保持 8 / 32；非法值 fail-closed 回退缺省）。
        max_inflight: Some(std::env::var("AGENT_MAX_INFLIGHT").ok().and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n >= 2 && *n <= 128).unwrap_or(8)),
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
        spent: Mutex::new(std::collections::HashMap::new()),
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

    /// Q 轮 Phase 3：显式入队。当前会话空闲 → 立即 spawn pump；有在途 chat →
    /// 由 chat_body 出口（闸释放后）spawn；steer 且在运行中 → 进轮次边界注入缓冲。
    fn queue_enqueue(&self, env: &Envelope) -> Value {
        let sid = env.payload.get("session_id").and_then(Value::as_str).unwrap_or("");
        let text = env.payload.get("user_text").and_then(Value::as_str).unwrap_or("");
        if sid.is_empty() || text.trim().is_empty() {
            return json!({"ok": false, "error": {"code": "K400", "message": "queue/enqueue 需 session_id + user_text"}});
        }
        let target = env.payload.get("target").and_then(Value::as_str).unwrap_or("followup");
        if !matches!(target, "followup" | "steer") {
            return json!({"ok": false, "error": {"code": "K400", "message": "target 只能是 followup 或 steer"}});
        }
        let atts = env.payload.get("attachments").cloned();
        match queue::enqueue(sid, text.to_string(), atts, target) {
            Ok((id, position)) => {
                if let Some(host) = self.host.get() {
                    queue::try_spawn(host, sid);
                }
                json!({"ok": true, "queued": true, "id": id, "position": position, "target": target})
            }
            Err(e) => json!({"ok": false, "error": {"code": "K400", "message": e}}),
        }
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
            // Q 轮 Phase 3：显式队列 op 面（纯追加；裸并发 chat 仍走上面的 K409 闸）。
            // 队列项经 host.call_plugin 派发，与裸 chat 共用同一 per-session 闸，
            // 但由单消费者 pump 串行化——「显式队列」不推翻「隐式排队」旧裁决。
            "queue/enqueue" => Ok(self.queue_enqueue(&env)),
            "queue/status" => Ok(json!({ "ok": true, "session_id": env.payload.get("session_id").and_then(Value::as_str).unwrap_or(""), "status": queue::status(env.payload.get("session_id").and_then(Value::as_str).unwrap_or("")) })),
            "queue/remove" => {
                let sid = env.payload.get("session_id").and_then(Value::as_str).unwrap_or("");
                let id = env.payload.get("id").and_then(Value::as_str).unwrap_or("");
                Ok(json!({"ok": queue::remove(sid, id), "removed": id}))
            }
            "queue/interrupt" => {
                let sid = env.payload.get("session_id").and_then(Value::as_str).unwrap_or("");
                if sid.is_empty() {
                    Ok(json!({"ok": false, "error": {"code": "K400", "message": "interrupt 需 session_id"}}))
                } else {
                    let n = queue::clear(sid);
                    self.cancels.lock().unwrap().insert(sid.to_string());
                    Ok(json!({"ok": true, "session_id": sid, "cleared": n, "note": "取消信号已置位；队列已清空（前端可把待发项退回编辑器）"}))
                }
            }
            "cancel" => {
                // P2/T1：置位取消标志。Concurrent 语义保证本调用不被在途 chat 的锁阻塞。
                let sid = env.payload.get("session_id").and_then(Value::as_str).unwrap_or("");
                if sid.is_empty() {
                    Ok(json!({"ok": false, "error": {"code": "K400", "message": "cancel 需 session_id"}}))
                } else {
                    let children = self.cancel_marks_children(sid);
                    for c in &children {
                        tracing::info!(target: ID, session = %c, parent = %sid, "cancel propagated to sub-session");
                    }
                    tracing::info!(target: ID, session = %sid, children = children.len(), "cancel requested");
                    // T7/T8（v0.1.7 消费）：联动 tools `abort` op——终止该会话运行中的
                    // 工具子进程，使"点停止"对工具执行段也生效。System 优先级走专用
                    // 泳道（T8），满载 chat 下不再排队；即发即忘 + 2s 上限；失败仅
                    // 记录——轮次边界取消仍兜底。
                    if let Some(host) = self.host.get() {
                        // 父会话 + 每个被联动的子会话各投递一次（终止技能工具子进程）。
                        let mut targets: Vec<String> = vec![sid.to_string()];
                        targets.extend(children.iter().cloned());
                        for target in targets {
                            let host = host.clone();
                            let payload = json!({"op": "abort", "session_id": target});
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
                    }
                    Ok(json!({"ok": true, "session_id": sid, "cancelled_children": children, "note": "取消信号已置位；当前轮完成后中断"}))
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
        queue::destroy();   // Q 轮：init/destroy 成对，清空进程内队列状态
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::chat::build_user_msg;
    use super::tools_exec::ObsPlan;
    use super::*;

    use crate::ENV_LOCK; // v12：跨模块共用一把锁，见 crate 根注释

    // P1：取消联动只命中本父会话的在办子会话，不波及兄弟父会话的子会话。
    #[test]
    fn cancel_propagation_collects_only_own_children() {
        let mut mirrors = std::collections::HashMap::new();
        mirrors.insert("P#sub-1".to_string(), "P".to_string());
        mirrors.insert("P#sub-2".to_string(), "P".to_string());
        mirrors.insert("Q#sub-3".to_string(), "Q".to_string());
        assert_eq!(collect_children(&mirrors, "P"), vec!["P#sub-1".to_string(), "P#sub-2".to_string()]);
        assert_eq!(collect_children(&mirrors, "Q"), vec!["Q#sub-3".to_string()]);
        assert!(collect_children(&mirrors, "Z").is_empty());
    }

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
            spent: Mutex::new(std::collections::HashMap::new()),
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

    /// C 轮：@引用抽取器必须只认合法 ID 形状（`T-YYYYMMDD-NN`），且去重保序——
    /// 松匹配会把「@Tomorrow」误当引用，紧匹配会漏掉真实引用，两个方向都有代价。
    #[test]
    fn extract_refs_shape_and_order() {
        use super::board_ctx::extract_refs;
        let text = "看 @T-20261107-01 和 @I-20261107-02，再说 @T-20261107-01";
        assert_eq!(
            extract_refs(text),
            vec!["T-20261107-01".to_string(), "I-20261107-02".to_string()]
        );
        // 非引用：缺日期段 / 段长不符 / 前缀非 T|I / 无 @
        for bad in ["@T-2026-01", "@X-20261107-01", "@T-2026110-01", "@T-20261107-1", "T-20261107-01"] {
            assert!(extract_refs(bad).is_empty(), "不应命中: {bad}");
        }
        // 三位序号（>99 后的形状）
        assert_eq!(extract_refs("@T-20261107-100"), vec!["T-20261107-100".to_string()]);
        assert!(extract_refs("").is_empty());
    }

    /// C 轮：引用渲染的三种结局（命中 / 全缺失 / 无引用）。纯函数入参 = 不碰进程环境，
    /// 与既有 WORKSPACE_ROOT 相关测试零串扰（初版用 set_var 版本实测红过）。
    #[test]
    fn render_refs_three_outcomes() {
        use super::board_ctx::{render_open_tasks, render_refs};
        let board = json!({
            "version": 1,
            "tasks": [
                {
                    "id": "T-20261107-01", "title": "评价体系", "status": "doing", "priority": "P1",
                    "goal": "把自证换成证据",
                    "plan": [{"step": "契约", "status": "completed"},
                             {"step": "执行器", "status": "in_progress"}],
                    "acceptance": ["eval 能跑"],
                    "checks": [{"id": "c1", "kind": "cmd", "name": "pytest", "status": "pass"}],
                    "evaluation": {"verdict": "pass", "pass": 1, "fail": 0, "skip": 0},
                    "resourcing": {"model": "current", "effort": "large", "skills": ["project-dev"]},
                    "artifacts": ["docs/x.md"],
                    "progress": {"done": 1, "total": 2, "note": "契约已定"}
                },
                {
                    "id": "T-20261107-02", "title": "收尾项", "status": "done", "priority": "P2",
                    "progress": {"done": 2, "total": 2, "note": ""}
                }
            ],
            "inspirations": [
                {"id": "I-20261107-01", "text": "看板要有约束", "tags": ["ui"],
                 "status": "open", "tasks": []}
            ]
        });

        // ① 命中引用：任务与灵感都渲染，含计划/验收/检查结论/人财物/产物
        let hit = render_refs("看 @T-20261107-01 和 @I-20261107-01", &board);
        assert!(hit.contains("Board references"), "缺 header: {hit}");
        assert!(hit.contains("评价体系") && hit.contains("progress=1/2"), "任务正文缺失: {hit}");
        assert!(hit.contains("[x] 契约"), "计划状态标记缺失: {hit}");
        assert!(hit.contains("verdict=pass"), "评价结论缺失: {hit}");
        assert!(hit.contains("project-dev"), "人财物缺失: {hit}");
        assert!(hit.contains("docs/x.md"), "产物缺失: {hit}");
        assert!(hit.contains("看板要有约束"), "灵感正文缺失: {hit}");
        assert!(!hit.contains("missing"), "不该报缺失: {hit}");

        // ② 全缺失：不得注入空壳 header，改为显式提示陈旧引用
        let miss = render_refs("看 @T-20990101-01", &board);
        assert!(miss.contains("not on the board"), "陈旧引用应显式提示: {miss}");
        assert!(!miss.contains("Board references (auto-loaded"), "全缺失别装成已装载: {miss}");

        // ③ 无引用 → 空串（调用方决定是否退化）
        assert_eq!(render_refs("没有引用的一句话", &board), "");

        // ④ 在办摘要排除 done/cancelled
        let digest = render_open_tasks(&board);
        assert!(digest.contains("Open tasks on the board"), "缺摘要 header: {digest}");
        assert!(digest.contains("T-20261107-01"), "摘要应含在办任务: {digest}");
        assert!(!digest.contains("T-20261107-02"), "done 任务不该出现在在办摘要: {digest}");
    }

    /// C 轮：看板通道说明只在有 CLI 时出现，full 才讲评价纪律。
    #[test]
    fn board_ops_hint_scales_by_flag_and_cli() {
        use super::board_ctx::board_ops_hint;
        assert!(board_ops_hint(true, false).is_empty(), "无 CLI 不得注入");
        let brief = board_ops_hint(false, true);
        let full = board_ops_hint(true, true);
        assert!(brief.contains("auto-loaded"), "brief 必须说明 @引用已自动装载");
        assert!(!brief.contains("eval <ID>"), "brief 不得含评价纪律");
        assert!(full.contains("eval <ID>"), "full 必须给评价执行通道");
        assert!(full.contains("digest --skill"), "full 必须给同类归纳通道");
        assert!(full.len() > brief.len(), "full 必须比 brief 长");
    }

    /// I-20261114-05：技能注册表失败**不得连坐**看板通道段。
    ///
    /// 实测故障链（2026-11-14）：`skills/project-dev/SKILL.md` 尾部混入 GBK 字节 →
    /// assets 插件 `read_text(encoding="utf-8")` 抛 UnicodeDecodeError（**不是**
    /// OSError，旧代码没捕）→ `skills.list` 整条 K500 → `skills_appendix` 首行
    /// `return String::new()` → 看板纪律段 / @引用说明 / 分解 / 并行 / 三档 全部消失。
    /// 用户能观察到的唯一症状是「新窗口的会话不把活登记进看板」，报错一个字都看不到。
    ///
    /// 本用例守的是「降级边界」：技能段可以整段不注入，看板段必须在。
    /// 为什么不用 mock 去造 assets 失败：`skills_appendix` 需要 Envelope + kernel
    /// handle（集成态才有），单测里造不出来。故把不变量拆成可纯函数验证的两半：
    ///   ① `task_board_section` 与技能列表**无数据依赖**（技能为空也照样产出）；
    ///   ② 失败路径上不得再出现 `return String::new()` 提前返回（源码级断言，
    ///      与 tests 里既有的「锚点/契约」测法同款）。
    #[test]
    fn board_section_survives_skills_list_failure() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (old_ws, old_home) = (
            std::env::var_os("WORKSPACE_ROOT"),
            std::env::var_os("RSI_HOME"),
        );
        let dir = std::env::temp_dir().join(format!("ra-bctx-nolink-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scripts")).expect("mkdir");
        std::fs::write(dir.join("scripts").join("board.py"), "print('x')\n").expect("write");
        std::env::set_var("RSI_HOME", &dir);
        std::env::set_var("WORKSPACE_ROOT", dir.join("user-project"));

        // ① 看板段不依赖技能注册表：技能为空/失败时仍必须产出。
        assert!(!AgentLoopPlugin::task_board_section(false).is_empty());
        assert!(!AgentLoopPlugin::task_board_section(true).is_empty());
        assert!(super::board_ctx::board_ops_hint(false, super::board_ctx::has_board_cli())
            .contains("auto-loaded"));
        assert!(!super::board_ctx::decompose_hint().is_empty());
        assert!(!super::board_ctx::parallel_hint().is_empty());
        assert!(!super::board_ctx::routing_hint().is_empty());

        // ② 源码级：skills.list 失败分支只降级技能段，不得再提前返回整段。
        let src = include_str!("skills.rs");
        let fail_branch = src
            .split("Err(e) => {")
            .find(|chunk| chunk.contains("skills.list 失败"))
            .expect("失败分支必须存在且带可读日志");
        let upto = &fail_branch[..fail_branch.len().min(600)];
        assert!(
            !upto.contains("return String::new()"),
            "skills.list 失败时不得 return 整段空串（那会连看板段一起吞掉）: {upto}"
        );

        match old_ws {
            Some(v) => std::env::set_var("WORKSPACE_ROOT", v),
            None => std::env::remove_var("WORKSPACE_ROOT"),
        }
        match old_home {
            Some(v) => std::env::set_var("RSI_HOME", v),
            None => std::env::remove_var("RSI_HOME"),
        }
    }

    /// 任务看板触发词判定：交代成规模的事才注入完整规范，闲聊不触发。
    #[test]
    fn want_task_plan_detection() {
        // 命中：任务性动作词 / 显式看板语义 / @引用
        for t in ["帮我把左栏改成任务看板", "实现单会话形态", "优化一下前端", "@T-20261107-01 继续",
                  "@I-20261107-01 这个想法做掉", "refactor the web layer", "build a new module"] {
            assert!(AgentLoopPlugin::user_wants_task_plan(t), "应命中: {t}");
        }
        // 不命中：闲聊 / 过短
        for t in ["在吗", "谢谢", "这是什么", "hi", "   "] {
            assert!(!AgentLoopPlugin::user_wants_task_plan(t), "不应命中: {t}");
        }
    }

    /// 任务看板提示段：CLI 能力锚定**代码根**，与工作区无关；full 才给规范。
    ///
    /// v10 语义变更（本测试重写）：旧版断言「工作区没有 board.py → 不注入」，
    /// 那条规则正是 bug 本身——用户开「新窗口」把 WORKSPACE_ROOT 换成自己的项目后，
    /// 看板段就整段消失，于是子窗口的看板永远收不到任务。
    /// 现在的不变量是：**换工作区不影响能力**，只要代码根有 CLI 就注入。
    #[test]
    fn task_board_section_requires_cli_and_scales_by_flag() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (old_ws, old_home) = (
            std::env::var_os("WORKSPACE_ROOT"),
            std::env::var_os("RSI_HOME"),
        );
        let dir = std::env::temp_dir().join(format!("ra-board-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scripts")).expect("mkdir");
        std::fs::write(dir.join("scripts").join("board.py"), "print('x')\n").expect("write");
        // 代码根（带 CLI）与工作区（用户项目，无 CLI）分开摆——这就是「新窗口」形态。
        std::env::set_var("RSI_HOME", &dir);
        let proj = dir.join("user-project");
        std::fs::create_dir_all(&proj).expect("mkdir project");
        std::env::set_var("WORKSPACE_ROOT", &proj);

        let brief = AgentLoopPlugin::task_board_section(false);
        let full = AgentLoopPlugin::task_board_section(true);
        assert!(!brief.is_empty(), "代码根有 CLI 时，换工作区也必须注入（这是 bug 的回归点）");
        assert!(brief.contains("board.py"), "brief 必须点明 CLI 通道");
        assert!(brief.contains(&dir.to_string_lossy().replace('\\', "/")),
                "提示词必须给绝对路径（子窗口 cwd 里没有 scripts/board.py）: {brief}");
        // brief 不得含 full 档独有的「四个问题」教学（BS1 修判据）：
        // 原判据是 `!brief.contains("resourcing.model")`——用**字段名**当代理，
        // 于是 BS1 在 brief 里合法写出 schema（`resourcing.model` 是 schema 的一部分）
        // 就被误判成「brief 含了完整规范」。
        //
        // 代理判据换成**行为性**的：full 档那段教学有唯一开头标记
        // （"### How to register a task"），brief 里不得出现它。
        // 这才是判据本来的意思——「brief 是指针，full 是教程」。
        assert!(!brief.contains("### How to register a task"),
                "brief 不得含 full 档的完整写作教程（BS1：schema 可在 brief，但教程不行）");
        assert!(full.contains("### How to register a task"), "full 必须讲人财物");
        assert!(full.contains("resourcing.model"), "full 必须讲人财物");
        assert!(full.contains("acceptance"), "full 必须讲验收标准");
        assert!(full.len() > brief.len(), "full 应比 brief 长");

        // 代码根没有 CLI → 不注入（别的项目工作区不该凭空多出这段提示）
        let empty = dir.join("no-cli-root");
        std::fs::create_dir_all(&empty).expect("mkdir");
        std::env::set_var("RSI_HOME", &empty);
        assert!(AgentLoopPlugin::task_board_section(true).is_empty(), "无 CLI 时不得注入");

        match old_ws {
            Some(v) => std::env::set_var("WORKSPACE_ROOT", v),
            None => std::env::remove_var("WORKSPACE_ROOT"),
        }
        match old_home {
            Some(v) => std::env::set_var("RSI_HOME", v),
            None => std::env::remove_var("RSI_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BS1（I-20261116）：brief 档必须**自带 task-upsert 的最小 schema**。
    ///
    /// 故障（2026-11-16 实测，用户原话「新建窗口似乎又不能自动将会话中的任务做进看板了」）：
    /// 子窗口的 agent 连撞 4 次 `task-upsert` 的 BAD_ARG（status: 'in_progress' /
    /// id: 'T-blind-full' / acceptance 传字符串），第 4 次才对；make-video 子窗口
    /// 则一次都没登记成功（查了 --help 就放弃）。
    ///
    /// 根因不是通道（CLI 绝对路径可达、数据是一本账），而是**规格不在调用点同框**：
    /// brief 档只给命令名，schema 只在 full 档（靠关键词命中才注入）。于是
    /// 「用户的话没命中关键词」的那一轮，模型手里只有命令名——只能猜，猜错就重试，
    /// 重试到放弃时用户看到的就是「任务没进看板」。
    ///
    /// 不变量：**brief 档（每轮都在）必须含规格**，且规格里的两套状态枚举必须
    /// 与 CLI 的真值一致——枚举漂移会把「写对」变成「写错」，比不给规格更糟。
    #[test]
    fn brief_board_section_carries_task_upsert_schema() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (old_ws, old_home) = (
            std::env::var_os("WORKSPACE_ROOT"),
            std::env::var_os("RSI_HOME"),
        );
        let dir = std::env::temp_dir().join(format!("ra-board-schema-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scripts")).expect("mkdir");
        std::fs::write(dir.join("scripts").join("board.py"), "print('x')\n").expect("write");
        std::env::set_var("RSI_HOME", &dir);
        let proj = dir.join("user-project");
        std::fs::create_dir_all(&proj).expect("mkdir project");
        std::env::set_var("WORKSPACE_ROOT", &proj);

        let brief = AgentLoopPlugin::task_board_section(false);

        // ① brief 档必须带 schema 段（不是只有命令名）
        assert!(
            brief.contains("task-upsert JSON schema"),
            "brief 档必须带 task-upsert schema（这是子窗口撞墙的根因）: {brief}"
        );
        // ② 两套状态枚举都必须在，且**分清层级**——in_progress 只属 plan 步
        for token in [
            "planning|doing|verifying|done|blocked|cancelled",
            "pending|in_progress|completed|cancelled",
        ] {
            assert!(brief.contains(token), "brief 缺枚举 {token}: {brief}");
        }
        // ③ 三个高频拒因必须点名（status 层级 / id 形状 / acceptance 是数组）
        assert!(
            brief.contains("in_progress` is NOT a task"),
            "brief 必须点明 in_progress 不是 task 级状态: {brief}"
        );
        assert!(
            brief.contains("T-YYYYMMDD-NN"),
            "brief 必须点明 id 形状与「省略即自动分配」: {brief}"
        );
        assert!(
            brief.contains("acceptance`: an **ARRAY**"),
            "brief 必须点明 acceptance 必须是数组: {brief}"
        );

        // ④ full 档也必须带 task 级枚举（brief 是子集，full 不得反而没有）
        let full = AgentLoopPlugin::task_board_section(true);
        assert!(
            full.contains("planning|doing|verifying|done|blocked|cancelled"),
            "full 档也必须带 task 级枚举"
        );

        match old_ws {
            Some(v) => std::env::set_var("WORKSPACE_ROOT", v),
            None => std::env::remove_var("WORKSPACE_ROOT"),
        }
        match old_home {
            Some(v) => std::env::set_var("RSI_HOME", v),
            None => std::env::remove_var("RSI_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S 轮分流提示段：三档判据必须可判定（对象数），且**不确定时取便宜的一档**。
    ///
    /// 为什么这条值得被测试钉住（而不是只写在提示词里）：
    /// 分流段最容易退化成的两种样子都是静默的——① 三档删到只剩两档（退化成原来的
    /// 「像任务 / 不像任务」，也就是这条补丁想修的病本身）；② 判据被改成「行数/token」
    /// 这类模型无法自估的量，写了等于没写。两者在提示词里都「看起来还行」，只有
    /// 断言关键判据与决胜规则才能拦住。
    #[test]
    fn routing_hint_declares_three_tiers_and_tie_break() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (old_home,) = (std::env::var_os("RSI_HOME"),);
        let dir = std::env::temp_dir().join(format!("ra-routing-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scripts")).expect("mkdir");
        std::fs::write(dir.join("scripts").join("board.py"), "print('x')\n").expect("write");
        std::env::set_var("RSI_HOME", &dir);

        let h = super::board_ctx::routing_hint();
        assert!(!h.is_empty(), "代码根有 CLI 时必须有分流段");
        // 三档都在，且各自绑了「不做什么」——只有档名没有禁令的提示词不会改变行为
        for tier in ["**S — answer directly.**", "**M — small change.**", "**L — project-level.**"] {
            assert!(h.contains(tier), "缺档位: {tier}\n{h}");
        }
        // 判据必须是可观测对象数，不是行数/token
        assert!(h.contains("how many objects will change"), "判据必须落在可观测对象数上");
        assert!(!h.to_lowercase().contains("token count"), "不得用 token 数当判据");
        // 决胜规则：不确定时取更便宜的一档（这条决定分流是否真能省钱）
        assert!(h.contains("CHEAPER"), "必须明确『不确定时取更便宜的一档』");
        assert!(h.contains("escalate"), "必须给升级条件，否则会一路做薄");
        // L 档必须指向 project-dev，否则「项目级」没有去处
        assert!(h.contains("project-dev"), "L 档必须指定 project-dev");
        // 两条不变量：M/L 不跳验证、不为显忙建任务
        assert!(h.contains("never skipped"), "验证不得因降档而跳过");
        assert!(h.contains("look busy"), "不得为显忙而建任务");

        // 代码根没有 CLI → 不注入（与 decompose/parallel 同一门控）
        let empty = dir.join("no-cli-root");
        std::fs::create_dir_all(&empty).expect("mkdir");
        std::env::set_var("RSI_HOME", &empty);
        assert!(super::board_ctx::routing_hint().is_empty(), "无 CLI 时不得注入");

        match old_home {
            Some(v) => std::env::set_var("RSI_HOME", v),
            None => std::env::remove_var("RSI_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
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

    /// P0-C 核心断言：工作集尾部按 token 预算裁剪后，规模有界且保留 system 头。
    ///
    /// 实测背景：压缩后持久层留 30 条，每条工具结果可达 8K 字符 → 每轮原样发给
    /// 模型 ≈ 120K tokens；每请求 cache_read 平台值 170–200K，而当日 77.1% 的
    /// credits 来自 cache_read。裁剪只作用于工作集，持久层不受影响。
    #[test]
    fn trim_workset_bounds_tokens_and_keeps_system_head() {
        use crate::chat::trim_workset_by_tokens;
        let m = |role: &str, body: &str| MemoryMsg {
            role: role.into(),
            content: Some(body.into()),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        };
        // system 头 + 20 条 8K 字符的消息（远超预算）
        let big = "x".repeat(8000);
        let mut msgs = vec![m("system", "SYS")];
        for _ in 0..20 {
            msgs.push(m("tool", &big));
        }
        let out = trim_workset_by_tokens(msgs.clone(), 10_000);
        assert_eq!(out[0].content.as_deref(), Some("SYS"), "system 头必须保留");
        assert!(out.len() < msgs.len(), "超预算时必须裁剪：{} vs {}", out.len(), msgs.len());
        assert!(
            crate::context::estimate_messages(&out) <= 10_000 + 8_000,
            "裁剪后规模应回到预算附近：{:}",
            crate::context::estimate_messages(&out)
        );
    }

    /// 未超预算时不动（幂等）：裁剪不能改变正常规模的工作集。
    #[test]
    fn trim_workset_is_noop_when_within_budget() {
        use crate::chat::trim_workset_by_tokens;
        let m = |role: &str, body: &str| MemoryMsg {
            role: role.into(),
            content: Some(body.into()),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        };
        let msgs = vec![m("system", "SYS"), m("user", "hello"), m("assistant", "hi")];
        let out = trim_workset_by_tokens(msgs.clone(), 100_000);
        assert_eq!(out.len(), msgs.len());
        assert_eq!(out[0].content.as_deref(), Some("SYS"));
    }

    /// 前缘孤儿 tool 消息必须丢弃：其配对的 assistant(tool_calls) 已被裁掉，
    /// 孤儿回喂会被 provider 拒收。
    #[test]
    fn trim_workset_drops_orphan_tool_messages() {
        use crate::chat::trim_workset_by_tokens;
        let m = |role: &str, body: &str| MemoryMsg {
            role: role.into(),
            content: Some(body.into()),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        };
        let big = "y".repeat(8000);
        // [sys, tool(孤儿), tool(孤儿), user(小)] —— 预算只够留 user
        let msgs = vec![m("system", "SYS"), m("tool", &big), m("tool", &big), m("user", "ok")];
        let out = trim_workset_by_tokens(msgs, 20);
        assert_eq!(out.len(), 2, "只留 system + user：{:?}", out.iter().map(|x| x.role.clone()).collect::<Vec<_>>());
        assert_eq!(out[0].role, "system");
        assert_eq!(out[1].role, "user");
    }
}
