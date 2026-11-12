//! 回合（turn）持久化（G 轮）：让「一次问-答」成为**可寻址、可补播、可暂停**的对象。
//!
//! ## 缺的是什么（改之前必读）
//!
//! 改造前，一次 chat 只是一次阻塞的 HTTP 调用：`POST /api/chat` 打到收敛才返回。
//! 于是三件事没有契约：
//!
//! 1. **断线即重跑**——浏览器刷新/网络抖动后，前端只能整轮重发；模型把已经做过的
//!    工具调用再做一遍（实测一次深度调研白烧十几万 token）。
//! 2. **暂停无处承接**——模型想问「要 A 还是 B」，只能把问题写进最终答复然后结束
//!    回合；用户回答=新回合，上下文要重新装配。
//! 3. **无法从中间看**——trace 有全量事件，但没有「这一轮从第几条开始」的锚点，
//!    离线补播只能全量重放。
//!
//! ## 做了什么（最小形态，三条都在）
//!
//! 一个 turn 就是一条落在 `.rsi/turns/<turn_id>.json` 的记录：
//!
//! | 字段 | 用途 |
//! |---|---|
//! | `turn_id` | 寻址键。`POST /api/chat` 返回值里带上，前端据此补播/回复 |
//! | `session_id` | 归属会话（补播时据此找 trace 文件） |
//! | `start_cursor` | **本轮起点在 trace 里的下标**——补播只读这一轮 |
//! | `end_cursor` | 已产出的最后一条（跑动中递增） |
//! | `status` | `running` / `paused` / `done` / `failed` / `cancelled` |
//! | `pending` | 暂停时挂着的问题 + 那条 tool_call 的 id（回复要按 id 接回去） |
//!
//! ## 三条边界（决定「不做」的部分）
//!
//! 1. **不复制事件**。turn 只存**游标**，事件仍在 trace 里（一份真相）。否则同一批
//!    事件存两份，回滚截断 trace 时 turn 里的副本就成了幽灵。
//! 2. **seq 是 trace 的绝对下标**，不是每回合从 0 开始。理由：补播端点
//!    `?after_seq=N` 直接把 N 交给 `trace.read(after=N)`，中间不做任何换算——
//!    换算层是「客户端和服务端各算一次」这类 bug 的产地。
//! 3. **turn 不是账本**。它是**短生命周期**的进程间交接物（`running` 的那几分钟），
//!    完成后保留只为补播；保留上限 `MAX_TURNS_PER_SESSION`，超出按 `ended_at` 淘汰最旧。
//!
//! ## 可测性
//!
//! 文件 I/O 全在 `TurnStore`；状态机（`can_reply` / `apply_reply`）是纯函数，单测覆盖
//! 全部非法转移。目录由 `RSI_TURNS_DIR` 覆盖（测试用临时目录，不碰真账）。

use super::*;
use serde_json::{json, Value};
use std::path::PathBuf;

/// 每会话保留的 turn 数上限（超出按 `ended_at` 淘汰最旧）。
/// 为什么是 24：补播只需要「最近几轮」；留太多等于把 trace 的下标反查表变成本地账本。
const MAX_TURNS_PER_SESSION: usize = 24;

pub(super) fn turns_root() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("RSI_TURNS_DIR") {
        let v = v.trim();
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    // 与看板/画像/记忆同源：`RSI_HOME` → `WORKSPACE_ROOT` → `cwd`（四层账不分散）。
    for key in ["RSI_HOME", "WORKSPACE_ROOT"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(PathBuf::from(v).join(".rsi").join("turns"));
            }
        }
    }
    std::env::current_dir().ok().map(|p| p.join(".rsi").join("turns"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 合法 turn_id：字母数字开头，其余字母数字/`-`/`_`，≤64。
///
/// 为什么要校验：turn_id 来自前端 URL/JSON，直接拼进文件名就是路径穿越面
/// （`../../config.json`）。这里**白名单**而不是黑名单——黑名单永远漏一个。
pub(super) fn valid_turn_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    let mut it = id.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 新建 turn 的 id：`t-<毫秒时间戳>-<会话短哈希>`。
///
/// 为什么不用随机数：id 可读性对排障有实际价值（`ls .rsi/turns` 就能看出先后），
/// 而碰撞风险由毫秒 + 会话哈希已经足够低；万一碰撞，`save` 的覆盖写成「同 id 更新」，
/// 语义仍是幂等而非丢数据。
pub(super) fn new_turn_id(session_id: &str) -> String {
    let mut h: u64 = 1469598103934665603; // FNV-1a offset basis
    for b in session_id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    format!("t-{}-{:06x}", now_ms(), h & 0xff_ffff)
}

/// turn 状态机：**合法的转移表**（非法转移一律拒绝并给可读原因）。
///
/// ```text
/// running ──pause──▶ paused ──reply──▶ running ──▶ done | failed | cancelled
///    └──────────────done | failed | cancelled──────▲
/// ```
///
/// 为什么不允许「done → reply」：回合已收敛，再回复就是新回合（新 turn_id）。
/// 允许它会让「这一轮到底跑了几次」变得不可回答。
pub(super) fn can_reply(status: &str) -> bool {
    status == "paused"
}

/// turn 是否已终结（不再接受任何状态变更）。
pub(super) fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled")
}

/// `ask_user` 保留工具的声明（随 tools 传给模型，使其可见可调）。
///
/// 描述里刻意写清「暂停而不是结束」：模型对「提问」的默认理解是把问题写进最终答复
/// 然后结束（那样用户回答=新回合，上下文要重建）。不写清它就不会用这个工具。
pub(super) fn ask_user_spec() -> ToolSpec {
    ToolSpec {
        name: RESERVED_ASK_USER.into(),
        obs: None,
        deferred: None,
        description: "Ask the user a question and PAUSE this turn (it does not end the turn). \
Use it when the next step genuinely depends on the user's decision — a real fork you cannot \
resolve from the workspace, not a politeness check. After the user answers, this same turn \
resumes with your existing progress intact (no work is redone). Prefer making a defensible \
assumption and stating it over asking."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "The question to ask; include the options you see"},
                "options": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional candidate answers the user can pick from"
                }
            },
            "required": ["question"]
        }),
    }
}

/// 回合记录的磁盘形状（`serde_json::Value` 直接读写，不做强类型 struct）。
///
/// 为什么用 Value 而不是 struct：这张表会被**新客户端/新字段**持续追加
/// （`paused` 是这一轮新加的），强类型 struct 会让每次加字段都变成一次
/// 「旧 turn 文件反序列化失败 → 补播全废」。Value + 逐字段取值对旧文件天然兼容。
pub(super) struct TurnStore {
    root: PathBuf,
}

impl TurnStore {
    pub(super) fn open() -> Option<Self> {
        turns_root().map(|root| Self { root })
    }

    fn path(&self, turn_id: &str) -> Option<PathBuf> {
        valid_turn_id(turn_id).then(|| self.root.join(format!("{turn_id}.json")))
    }

    /// 读一条 turn（不存在/坏 JSON/形状不对 → None，调用方按「无此回合」处理）。
    pub(super) fn load(&self, turn_id: &str) -> Option<Value> {
        let p = self.path(turn_id)?;
        let text = std::fs::read_to_string(p).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        v.is_object().then_some(v)
    }

    /// 写一条 turn（原子替换：中途崩溃不留半份 → 补播不会读到残缺 JSON）。
    pub(super) fn save(&self, turn: &Value) -> bool {
        let Some(turn_id) = turn.get("turn_id").and_then(Value::as_str) else {
            return false;
        };
        let Some(p) = self.path(turn_id) else {
            return false;
        };
        if std::fs::create_dir_all(&self.root).is_err() {
            return false;
        }
        let tmp = self.root.join(format!("{turn_id}.json.tmp"));
        let text = serde_json::to_string_pretty(turn).unwrap_or_default();
        if std::fs::write(&tmp, text).is_err() {
            return false;
        }
        std::fs::rename(&tmp, &p).is_ok()
    }

    /// 列出某会话的 turn（按 `started_at` 倒序），并顺带清理超限的最旧项。
    pub(super) fn list_session(&self, session_id: &str) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            if v.get("session_id").and_then(Value::as_str) == Some(session_id) {
                out.push(v);
            }
        }
        out.sort_by_key(|v| std::cmp::Reverse(v.get("started_at").and_then(Value::as_u64).unwrap_or(0)));
        // 淘汰超限的最旧项（只删终结态——`running`/`paused` 删了等于让前端失去补播目标）
        let mut live = 0usize;
        let mut doomed: Vec<String> = Vec::new();
        for v in out.iter() {
            live += 1;
            if live > MAX_TURNS_PER_SESSION {
                let st = v.get("status").and_then(Value::as_str).unwrap_or("");
                if is_terminal(st) {
                    if let Some(id) = v.get("turn_id").and_then(Value::as_str) {
                        doomed.push(id.to_string());
                    }
                }
            }
        }
        for id in doomed {
            if let Some(p) = self.path(&id) {
                let _ = std::fs::remove_file(p);
            }
            out.retain(|v| v.get("turn_id").and_then(Value::as_str) != Some(id.as_str()));
        }
        out
    }
}

/// 构造一条新 turn 记录（`running`）。
///
/// `start_cursor` 是本轮起点在 trace 里的绝对下标——补播只读 `[start_cursor, end_cursor)`，
/// 于是「这一轮做了什么」不需要全量重放，也不需要第二个事件存储。
pub(super) fn new_turn(turn_id: &str, session_id: &str, start_cursor: u64) -> Value {
    json!({
        "turn_id": turn_id,
        "session_id": session_id,
        "status": "running",
        "start_cursor": start_cursor,
        "end_cursor": start_cursor,
        "started_at": now_ms(),
        "ended_at": null,
        "pending": null,
        "rounds": 0,
    })
}

/// 标记暂停：写入**问题**与那条 `ask_user` 调用的 id。
///
/// 为什么必须存 `tool_call_id`：回复要作为**那一条 tool_call 的结果**接回去，
/// 否则模型看到的是「一个没有对应调用的孤立 tool 消息」，严格协议下会直接报错。
pub(super) fn mark_paused(turn: &mut Value, question: &str, tool_call_id: &str, end_cursor: u64) -> bool {
    if turn.get("status").and_then(Value::as_str) != Some("running") {
        return false;
    }
    turn["status"] = json!("paused");
    turn["pending"] = json!({
        "question": question,
        "tool_call_id": tool_call_id,
        "asked_at": now_ms(),
    });
    turn["end_cursor"] = json!(end_cursor);
    true
}

/// 标记终结（`done` / `failed` / `cancelled`）。
///
/// 只从非终结态迁移；重复终结保持第一次的结论（幂等）——否则「跑完又被 cancel」
/// 会把一条本来成功的回合改写成 cancelled，补播时状态与事件对不上。
pub(super) fn mark_terminal(turn: &mut Value, status: &str, end_cursor: u64) {
    if is_terminal(turn.get("status").and_then(Value::as_str).unwrap_or("")) {
        return;
    }
    if !is_terminal(status) {
        return;
    }
    turn["status"] = json!(status);
    turn["end_cursor"] = json!(end_cursor);
    turn["ended_at"] = json!(now_ms());
    turn["pending"] = Value::Null;
}

/// 承接回复：`paused` → `running`，并把答案记在 `pending.answer` 上（审计用）。
/// 返回被接回的那条 `tool_call_id`（调用方据此构造 tool 消息）。
pub(super) fn apply_reply(turn: &mut Value, answer: &str) -> Option<String> {
    if !can_reply(turn.get("status").and_then(Value::as_str).unwrap_or("")) {
        return None;
    }
    let call_id = turn
        .get("pending")
        .and_then(|p| p.get("tool_call_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if call_id.is_empty() {
        return None;
    }
    // 保留原问题：模型下一轮要看到「我问了什么、用户答了什么」。原先这里整体覆写
    // pending 把 question 丢了 —— 工作集里只剩一句回答，模型不知道自己在答哪一问
    // （端到端用例实测：问题文本消失，模型只能重新猜上下文）。
    let question = turn
        .get("pending")
        .and_then(|p| p.get("question"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let asked_at = turn.get("pending").and_then(|p| p.get("asked_at")).cloned();
    turn["status"] = json!("running");
    turn["pending"] = json!({
        "question": question,
        "asked_at": asked_at,
        "answered_at": now_ms(),
        "answer": answer,
        "tool_call_id": call_id,
    });
    Some(call_id)
}

/// 补播视图：把 turn 记录转成「前端要什么」。
///
/// 关键决定：返回的是**游标区间**不是事件本体。事件由调用方经 `trace.read(after=…)`
/// 分页拉（已有能力），于是补播不引入第二个事件通道，也就不可能与 trace 不一致。
pub(super) fn replay_view(turn: &Value, after_seq: u64) -> Value {
    let start = turn.get("start_cursor").and_then(Value::as_u64).unwrap_or(0);
    let end = turn.get("end_cursor").and_then(Value::as_u64).unwrap_or(start);
    // after_seq 是 turn 内相对序号（0 = 本轮开头），换成 trace 绝对下标。
    // 夹在 [start, end] 内：客户端乱传（负数/超前）不产生越界读，退化为从头/无新增。
    let from = start.saturating_add(after_seq).min(end);
    json!({
        "ok": true,
        "turn_id": turn.get("turn_id").cloned().unwrap_or(Value::Null),
        "session_id": turn.get("session_id").cloned().unwrap_or(Value::Null),
        "status": turn.get("status").cloned().unwrap_or(Value::Null),
        "pending": turn.get("pending").cloned().unwrap_or(Value::Null),
        "from_cursor": from,
        "end_cursor": end,
        "has_more": from < end,
        "replayed_seq": from.saturating_sub(start),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Value {
        new_turn("t-1", "s1", 100)
    }

    #[test]
    fn turn_id_whitelist_blocks_traversal() {
        assert!(valid_turn_id("t-1234-abcd"));
        assert!(valid_turn_id("T_a"));
        assert!(!valid_turn_id(""));
        assert!(!valid_turn_id("../config"));
        assert!(!valid_turn_id("a/b"));
        assert!(!valid_turn_id("-lead"));
        assert!(!valid_turn_id(&"a".repeat(65)));
    }

    #[test]
    fn new_turn_starts_running_at_start_cursor() {
        let v = t();
        assert_eq!(v["status"], json!("running"));
        assert_eq!(v["start_cursor"], json!(100));
        assert_eq!(v["end_cursor"], json!(100));
        assert_eq!(v["pending"], Value::Null);
    }

    /// 暂停必须记下 tool_call_id——回复要按 id 接回那一条调用，
    /// 否则模型看到孤立 tool 消息（严格协议下直接报错）。
    #[test]
    fn pause_records_question_and_call_id() {
        let mut v = t();
        assert!(mark_paused(&mut v, "要 A 还是 B？", "call_x", 120));
        assert_eq!(v["status"], json!("paused"));
        assert_eq!(v["pending"]["question"], json!("要 A 还是 B？"));
        assert_eq!(v["pending"]["tool_call_id"], json!("call_x"));
        assert_eq!(v["end_cursor"], json!(120));
    }

    /// 只有 running 能暂停：已终结的回合暂停等于让补播看到「跑完了还挂着问题」。
    #[test]
    fn pause_rejected_when_not_running() {
        let mut v = t();
        mark_terminal(&mut v, "done", 130);
        assert!(!mark_paused(&mut v, "q", "c", 140));
        assert_eq!(v["status"], json!("done"));
    }

    #[test]
    fn reply_only_from_paused_and_returns_call_id() {
        let mut v = t();
        assert_eq!(apply_reply(&mut v, "A"), None, "running 态不该接回复");
        mark_paused(&mut v, "q", "call_y", 110);
        let id = apply_reply(&mut v, "选 A");
        assert_eq!(id.as_deref(), Some("call_y"));
        assert_eq!(v["status"], json!("running"));
        assert_eq!(v["pending"]["answer"], json!("选 A"));
        assert_eq!(apply_reply(&mut v, "再来一次"), None, "已接过的回复不该再接");
    }

    /// 回复必须保留原问题：模型下一轮要看到「我问了什么、用户答了什么」。
    /// 原先整体覆写 pending 把 question 丢了，工作集里只剩一句回答。
    #[test]
    fn reply_preserves_original_question() {
        let mut v = t();
        mark_paused(&mut v, "要 A 还是 B？", "call_z", 110);
        apply_reply(&mut v, "选 B");
        assert_eq!(v["pending"]["question"], json!("要 A 还是 B？"), "原问题不该丢");
        assert_eq!(v["pending"]["answer"], json!("选 B"));
    }

    /// 终结幂等：重复终结保持第一次结论——否则成功的回合会被后来的 cancel 改写。
    #[test]
    fn terminal_transition_is_idempotent() {
        let mut v = t();
        mark_terminal(&mut v, "done", 200);
        assert_eq!(v["status"], json!("done"));
        mark_terminal(&mut v, "cancelled", 210);
        assert_eq!(v["status"], json!("done"), "第一次结论不该被后来的改写");
        assert_eq!(v["end_cursor"], json!(200));
        mark_terminal(&mut v, "running", 220);
        assert_eq!(v["status"], json!("done"), "非终结态不算转移");
    }

    #[test]
    fn reply_view_maps_relative_seq_to_trace_cursor() {
        let mut v = t();
        mark_paused(&mut v, "q", "c1", 140);
        // after_seq=5（本轮第 5 条之后）→ trace 下标 100+5=105
        let rv = replay_view(&v, 5);
        assert_eq!(rv["from_cursor"], json!(105));
        assert_eq!(rv["end_cursor"], json!(140));
        assert_eq!(rv["has_more"], json!(true));
        assert_eq!(rv["replayed_seq"], json!(5));
        // 超前/越界夹回 end：不产生越界读
        let over = replay_view(&v, 999);
        assert_eq!(over["from_cursor"], json!(140));
        assert_eq!(over["has_more"], json!(false));
    }

    #[test]
    fn store_roundtrip_and_path_safety() {
        let dir = std::env::temp_dir().join(format!("rsi-turns-test-{}", std::process::id()));
        std::env::set_var("RSI_TURNS_DIR", &dir);
        let store = TurnStore::open().expect("turns dir");
        let mut v = new_turn("t-rt-1", "sess-a", 7);
        assert!(store.save(&v));
        let back = store.load("t-rt-1").expect("load");
        assert_eq!(back["session_id"], json!("sess-a"));
        assert_eq!(back["start_cursor"], json!(7));
        // 非法 id 不落盘也不读盘（路径穿越面）
        assert!(!store.save(&json!({"turn_id": "../evil"})));
        assert!(store.load("../evil").is_none());
        // 终结态才能被淘汰：造 30 条 done，只留上限内
        for i in 0..30 {
            let mut x = new_turn(&format!("t-bulk-{i}"), "sess-b", i as u64);
            mark_terminal(&mut x, "done", i as u64);
            x["started_at"] = json!(i);
            assert!(store.save(&x));
        }
        let listed = store.list_session("sess-b");
        assert_eq!(listed.len(), MAX_TURNS_PER_SESSION, "超限应淘汰最旧终结态");
        assert_eq!(listed[0]["turn_id"], json!("t-bulk-29"), "按 started_at 倒序");
        std::env::remove_var("RSI_TURNS_DIR");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = &mut v;
    }

    /// 未终结的 turn 不淘汰——删了等于让前端失去补播目标。
    #[test]
    fn running_turns_are_not_evicted() {
        let dir = std::env::temp_dir().join(format!("rsi-turns-test2-{}", std::process::id()));
        std::env::set_var("RSI_TURNS_DIR", &dir);
        let store = TurnStore::open().expect("turns dir");
        // 一条 running + 一堆 done
        let run = new_turn("t-live", "sess-c", 0);
        store.save(&run);
        for i in 0..MAX_TURNS_PER_SESSION + 5 {
            let mut x = new_turn(&format!("t-d-{i}"), "sess-c", i as u64);
            mark_terminal(&mut x, "done", i as u64);
            x["started_at"] = json!(i + 1);
            store.save(&x);
        }
        let listed = store.list_session("sess-c");
        assert!(
            listed.iter().any(|v| v["turn_id"] == json!("t-live")),
            "running 的 turn 不能被淘汰: {:?}",
            listed.iter().map(|v| v["turn_id"].clone()).collect::<Vec<_>>()
        );
        std::env::remove_var("RSI_TURNS_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
