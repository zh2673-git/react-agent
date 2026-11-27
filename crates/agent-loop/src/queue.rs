//! Q 轮 Phase 3：服务端显式队列（跨标签页一致、可观测、可取消）。
//!
//! 设计取舍（与方案 §2.4 / 00-PLAN P2 旧裁决一致）：
//! - **裸并发** `POST /api/chat` 同 session 仍由 `session_chats` 闸返回 K409，不排队；
//! - 只有显式 `queue/enqueue` 入口的指令才被串行化——「显式队列 vs 隐式排队」不冲突；
//! - 状态放进程内全局 `Mutex<HashMap>`（不放插件结构体：它有两处构造点 + 单测直构，
//!   加字段会破坏既有测试）。`concurrent:false` 的 memory 语义不变。
//! - 有界：每会话 ≤20 项、单条 ≤64KB、pump 重试有上限；destroy 清空（时空闭环）。
//! - steer 是独立的软注入缓冲：轮次边界由 `chat_run` 取走注入当前工作集，
//!   不新开 chat、不写 `user` trace 事件（不污染回滚的 user 计数与事件 type 全集）。
use agent_kernel_sdk::{Envelope, HostApi, PluginId};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const MAX_ITEMS_PER_SESSION: usize = 20;
const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct QueueItem {
    pub id: String,
    pub text: String,
    pub attachments: Option<Value>,
    pub target: String,
}

struct SessionQueue {
    active: bool, // 有 chat 正在跑（含裸 /api/chat）
    pump: bool,   // 有 pump 任务在串行派发
    items: VecDeque<QueueItem>,
    steers: Vec<String>,
}
impl Default for SessionQueue {
    fn default() -> Self {
        Self { active: false, pump: false, items: VecDeque::new(), steers: Vec::new() }
    }
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, SessionQueue>> {
    static Q: OnceLock<Mutex<HashMap<String, SessionQueue>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
fn new_id() -> String {
    static N: OnceLock<Mutex<u64>> = OnceLock::new();
    let c = N.get_or_init(|| Mutex::new(0));
    let mut n = c.lock().unwrap_or_else(|e| e.into_inner());
    *n += 1;
    format!("q{}", *n)
}

/// chat_body 进入/退出：标记会话是否有在途 chat（enqueue 据此决定 steer 走向）。
pub fn mark_active(sid: &str, v: bool) {
    let mut m = lock();
    if v {
        m.entry(sid.to_string()).or_default().active = true;
    } else if let Some(s) = m.get_mut(sid) {
        s.active = false;
        if !s.pump && s.items.is_empty() && s.steers.is_empty() {
            m.remove(sid);
        }
    }
}

/// 入队。返回 (id, position)；steer 且当前有在途 chat → 进软注入缓冲（position=0）。
pub fn enqueue(sid: &str, text: String, attachments: Option<Value>, target: &str) -> Result<(String, usize), String> {
    if text.len() > MAX_TEXT_BYTES {
        return Err("message 超过 64KB 上限".into());
    }
    let id = new_id();
    let mut m = lock();
    let s = m.entry(sid.to_string()).or_default();
    if target == "steer" && s.active {
        if s.steers.len() >= MAX_ITEMS_PER_SESSION {
            return Err("队列已满（20 条上限）".into());
        }
        s.steers.push(text);
        return Ok((id, 0));
    }
    if s.items.len() >= MAX_ITEMS_PER_SESSION {
        return Err("队列已满（20 条上限）".into());
    }
    s.items.push_back(QueueItem { id: id.clone(), text, attachments, target: "followup".into() });
    Ok((id, s.items.len()))
}

/// chat 出口兜底：本 chat 未赶上的轮次边界（模型已收敛）→ 残留 steer 转回
/// followup 队首，由 pump 作为新 chat 执行，绝不静默丢失。
pub fn leftover_steers_to_followup(sid: &str) {
    let mut m = lock();
    let Some(s) = m.get_mut(sid) else { return };
    let texts = std::mem::take(&mut s.steers);
    for t in texts.into_iter().rev() {
        if s.items.len() < MAX_ITEMS_PER_SESSION {
            s.items.push_front(QueueItem { id: new_id(), text: t, attachments: None, target: "followup".into() });
        }
    }
}

/// chat_run 轮次边界：取走全部待注入 steer 文本。
pub fn take_steers(sid: &str) -> Vec<String> {
    lock().get_mut(sid).map(|s| std::mem::take(&mut s.steers)).unwrap_or_default()
}

/// 原子：无 pump、无 active chat 且队列非空 → 置 pump 并返回 true（调用方 spawn）。
pub fn should_spawn_pump(sid: &str) -> bool {
    let mut m = lock();
    let s = m.entry(sid.to_string()).or_default();
    if !s.pump && !s.active && !s.items.is_empty() {
        s.pump = true;
        true
    } else {
        false
    }
}

/// pump 取下一项；队列空时原子清 pump 并返回 None（enqueue 不与之交错，绝不丢项）。
pub fn next_item(sid: &str) -> Option<QueueItem> {
    let mut m = lock();
    let s = m.get_mut(sid)?;
    match s.items.pop_front() {
        Some(it) => Some(it),
        None => {
            s.pump = false;
            if !s.active && s.steers.is_empty() {
                m.remove(sid);
            }
            None
        }
    }
}

/// 重试耗尽把队首放回（防极端竞态下丢项）。
pub fn push_front(sid: &str, it: QueueItem) {
    let mut m = lock();
    let s = m.entry(sid.to_string()).or_default();
    if s.items.len() < MAX_ITEMS_PER_SESSION {
        s.items.push_front(it);
    }
    s.pump = false;
}

pub fn status(sid: &str) -> Value {
    let m = lock();
    let s = m.get(sid);
    let pending: Vec<Value> = s
        .map(|s| s.items.iter().map(|i| json!({"id": i.id, "text": i.text, "target": i.target})).collect())
        .unwrap_or_default();
    json!({
        "ok": true,
        "running": s.map(|s| s.active || s.pump).unwrap_or(false),
        "pending": pending,
        "steer_pending": s.map(|s| s.steers.len()).unwrap_or(0),
        "mode": "explicit",
    })
}

pub fn remove(sid: &str, id: &str) -> bool {
    let mut m = lock();
    let Some(s) = m.get_mut(sid) else { return false };
    let n = s.items.len();
    s.items.retain(|i| i.id != id);
    s.items.len() != n
}

/// interrupt：清空 followup 队列与 steer 缓冲，返回被清项数。取消信号由调用方置位。
pub fn clear(sid: &str) -> usize {
    let mut m = lock();
    let Some(s) = m.get_mut(sid) else { return 0 };
    let n = s.items.len();
    s.items.clear();
    s.steers.clear();
    n
}

/// 队列项 → chat payload（pump 经 host.call_plugin 派发，复用 per-session K409 闸）。
pub fn item_payload(sid: &str, it: &QueueItem) -> Value {
    let mut v = json!({"op": "chat", "session_id": sid, "user_text": it.text});
    if let Some(a) = &it.attachments {
        v["attachments"] = a.clone();
    }
    v
}

/// 入队/会话空闲的共用入口：满足调度条件就 spawn 单消费者。
pub fn try_spawn(host: &Arc<dyn HostApi>, sid: &str) {
    if should_spawn_pump(sid) {
        spawn_pump(host.clone(), sid.to_string());
    }
}

/// 单消费者：逐条派发 chat；偶发 K409（裸 chat 竞态）有限重试，耗尽放回队首。
pub fn spawn_pump(host: Arc<dyn HostApi>, sid: String) {
    tokio::spawn(async move {
        loop {
            let Some(it) = next_item(&sid) else { break };
            let env = Envelope::new(PluginId::new(crate::ID), item_payload(&sid, &it));
            let mut tries = 0u32;
            loop {
                let hit409 = match host.call_plugin(env.clone()).await {
                    Ok(v) => v.get("error").and_then(|e| e.get("code")).and_then(|c| c.as_str()) == Some("K409"),
                    Err(_) => false,
                };
                if !hit409 {
                    break;
                }
                tries += 1;
                if tries >= 120 {
                    push_front(&sid, it);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
}

/// init/destroy 成对：进程结束时清空全局队列状态。
pub fn destroy() {
    lock().clear();
}
