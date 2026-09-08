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
mod context;
mod contract;
mod skills;
mod subagent;
mod tools_exec;
mod trace;

pub use contract::{Attachment, ChatReq, LlmChatResp, MemoryMsg, StepRecord, ToolCall, ToolSpec};

use agent_kernel_sdk::*;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

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
        turn_starts: Mutex::new(std::collections::HashMap::new()),
        sid_seq: Mutex::new(std::collections::HashMap::new()),
        mirrors: Mutex::new(std::collections::HashMap::new()),
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

/// 工具结果回喂上限（字符）：0 = 禁用。
fn tool_result_limit() -> usize {
    std::env::var("TOOL_RESULT_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_TOOL_RESULT_CHARS)
}

impl AgentLoopPlugin {
    /// 生成下一轮流式 sid（sid 唯一性修复）：`{session}-r{N}`，N 为 per session 单调
    /// 递增序号，首个 sid 以 unix 毫秒为种子。跨对话回合不再碰撞（旧 bug：每回合
    /// rounds 从 1 重新计数，前端 doneSids 去重把后一回合的流式动画误判为已定稿帧）。
    pub(crate) fn next_sid(&self, session_id: &str) -> String {
        let mut m = self.sid_seq.lock().unwrap();
        let n = match m.get(session_id) {
            Some(v) => v + 1,
            None => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(1),
        };
        m.insert(session_id.to_string(), n);
        format!("{session_id}-r{n}")
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
    use super::chat::build_user_msg;
    use super::*;

    #[test]
    fn extract_sources_dedupes_and_validates() {
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
        let items = AgentLoopPlugin::extract_sources("web_search", &inner);
        assert_eq!(items.len(), 3, "{items:?}");
        assert_eq!(items[0], ("A".into(), "https://a.example/1".into()));
        assert_eq!(items[1], ("B".into(), "https://b.example/2".into()));
        assert_eq!(items[2], ("https://c.example/3".into(), "https://c.example/3".into()));

        // web_read：单 URL；其他工具：空
        assert_eq!(
            AgentLoopPlugin::extract_sources("web_read", &json!({"url": "https://r.example/x"})).len(),
            1
        );
        assert!(AgentLoopPlugin::extract_sources("bash", &json!({"output": "https://x.example"})).is_empty());
    }

    #[test]
    fn extract_sources_caps_at_ten() {
        let results: Vec<Value> = (0..30)
            .map(|i| json!({"title": format!("t{i}"), "url": format!("https://e.example/{i}")}))
            .collect();
        let items = AgentLoopPlugin::extract_sources("web_search", &json!({ "results": results }));
        assert_eq!(items.len(), 10, "溯源卡上限 10 条");
    }

    #[test]
    fn file_change_event_only_for_successful_file_tools() {
        let tc = |name: &str| ToolCall { id: "1".into(), name: name.into(), arguments: json!({}) };
        let ok = |path: &str| json!({"ok": true, "result": {"path": path}});
        // 成功 write/edit → 事件（path 归一正斜杠）
        let ev = AgentLoopPlugin::file_change_events_for(&tc("write_file"), &ok(r"D:\ws\outputs\a.md"))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(ev["path"], json!("D:/ws/outputs/a.md"), "反斜杠归一");
        assert_eq!(ev["op"], json!("write"));
        let ev = AgentLoopPlugin::file_change_events_for(&tc("edit_file"), &ok("x.md"))
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(ev["op"], json!("edit"));
        // 失败 / 非文件工具 / 缺 path → 不登记
        assert!(AgentLoopPlugin::file_change_events_for(&tc("write_file"), &json!({"ok": false})).is_empty());
        assert!(AgentLoopPlugin::file_change_events_for(&tc("bash"), &ok("x")).is_empty());
        assert!(AgentLoopPlugin::file_change_events_for(&tc("edit_file"), &json!({"ok": true, "result": {}})).is_empty());
    }

    #[test]
    fn next_sid_monotonic_and_shape_stable() {
        // 直接构造结构体（tests 模块在 crate root 子级，可访问私有字段）
        let p = AgentLoopPlugin {
            max_rounds: 8,
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
        let got = AgentLoopPlugin::scan_artifact_paths(text);
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

    /// TEXT_ATTACH_LIMIT 是进程级 env：并行测试同时改写会串扰，用互斥锁串行化。
    static TEXT_ATTACH_LIMIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn build_user_msg_routes_attachments() {
        // 隔离并行测试的 TEXT_ATTACH_LIMIT 串扰（截断行为由专项测试覆盖）
        let _guard = TEXT_ATTACH_LIMIT_LOCK.lock().unwrap();
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
        let _guard = TEXT_ATTACH_LIMIT_LOCK.lock().unwrap();
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
