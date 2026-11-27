//! 任务看板 / 灵感看板（`GET|POST /api/board`）。
//!
//! 为什么在 host 而不是新插件：看板是**视图数据**不是业务能力——它没有自己的执行流，
//! 只有「读一份 JSON、按校验改几个字段、原子写回」。放进内核会污染运行时语义（内核不
//! 感知任务），放进 guest 插件又要跨进程只为读一个文件。host 是组合根，读 UI 数据的
//! 位置天然在这里（与 `/api/config`、`/api/instances` 同层）。
//!
//! 契约与 `scripts/board.py` 逐字同源（`docs/self-evolution/01-单会话与任务看板.md` §2）：
//! host 侧**只写灵感**（用户视角的入口），任务的增删改一律由 agent 经 `board.py` 完成
//! ——用户构想是「用户不需要管背后的项目分类」。两侧都用「读-改-写 + 按 id 合并 +
//! tmp/rename 原子替换」。**注意原子替换只保证不掉半截文件，不保证两次写不互相覆盖**：
//! 两者都无锁时，A/B 各自 load 到同一份旧快照再整体写回，后写者会覆盖前者的新条目
//! （文件完整、少一条，最难查的那种）。故两侧写盘前都取同一把跨进程文件锁
//! `board.lock`（Python `BoardLock` / Rust `BoardLock`，见下方实现）。
//!
//! 数据落点：`WORKSPACE_ROOT/.rsi/board.json`（`RSI_BOARD_PATH` 可覆盖，工作区相对或绝对）。
//! 坏 JSON **不覆盖**：按空板返回并带 `corrupt:true`，让用户看见而不是被悄悄清零。

use super::super::{bad_request, json_resp};
use crate::config;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use tokio::net::TcpStream;

// ── 契约常量（与 scripts/board.py 同源，改一侧必须改另一侧）─────────────────
pub(super) const TASK_STATUSES: &[&str] = &["planning", "doing", "verifying", "done", "blocked", "cancelled"];
pub(super) const INSP_STATUSES: &[&str] = &["open", "adopted", "archived"];
const MAX_TASKS: usize = 200;
const MAX_INSP: usize = 500;
const MAX_INSP_TEXT: usize = 2000;
const MAX_TAGS: usize = 10;
const MAX_TAG: usize = 32;
const MAX_TITLE: usize = 120;

/// 布局白名单（与 `scripts/board.py::validate_layout` 逐字同源）：kind 决定渲染器，
/// 顺序也由 kind 钉死（tasks → metrics → inspirations）——「插件化但有约束」的落点。
const LAYOUT_KINDS: &[&str] = &["tasks", "inspirations", "metrics"];
// v13：`workspace` 也是分组维度（与 scripts/board.py::GROUP_BYS 逐字同源）。
// 用户诉求是「全部里一眼看出哪条属于哪个工作区」；分组比逐行挂徽章更省眼，
// 且折叠组头时仍能给出「这个状态组里有哪几个工作区的活」。
const GROUP_BYS: &[&str] = &["status", "priority", "skill", "workspace"];
const FILTERS: &[&str] = &["all", "open", "done"];
const METRIC_IDS: &[&str] = &["by_status", "by_skill", "verdict", "acceptance"];
const MAX_SECTIONS: usize = 6;

fn default_layout() -> Value {
    json!({"sections": [
        {"id": "tasks", "kind": "tasks", "title": "任务看板", "group_by": "status",
         "filter": "all", "limit": 30},
        {"id": "insp", "kind": "inspirations", "title": "灵感"}
    ]})
}

/// 规整章节数组：丢非法项而不整体拒绝（坏配置不该让看板白屏），但**顺序按 kind 重排**，
/// 保证手改 layout 也改变不了「任务在上、灵感在下」这个可预期结构。
fn normalize_layout(raw: Option<&Value>) -> Value {
    let sections = raw.and_then(|v| v.get("sections")).and_then(Value::as_array);
    let mut out: Vec<Value> = Vec::new();
    if let Some(items) = sections {
        for it in items.iter().take(MAX_SECTIONS) {
            let Some(obj) = it.as_object() else { continue };
            let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
            let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() || id.chars().count() > 24 || !LAYOUT_KINDS.contains(&kind) {
                continue;
            }
            if out.iter().any(|s| s.get("id").and_then(Value::as_str) == Some(id)) {
                continue; // id 重复只留第一个（后到的丢弃而非整体拒绝）
            }
            let title = obj.get("title").and_then(Value::as_str).unwrap_or(id);
            let mut sec = json!({"id": id, "kind": kind, "title": title});
            if kind == "tasks" {
                let gb = obj.get("group_by").and_then(Value::as_str).unwrap_or("status");
                let fl = obj.get("filter").and_then(Value::as_str).unwrap_or("all");
                let lim = obj.get("limit").and_then(Value::as_u64).unwrap_or(30).clamp(1, 200);
                sec["group_by"] = json!(if GROUP_BYS.contains(&gb) { gb } else { "status" });
                sec["filter"] = json!(if FILTERS.contains(&fl) { fl } else { "all" });
                sec["limit"] = json!(lim);
            } else if kind == "metrics" {
                let mut keep: Vec<Value> = Vec::new();
                if let Some(Value::Array(ms)) = obj.get("metrics") {
                    for m in ms {
                        if let Some(name) = m.as_str() {
                            if METRIC_IDS.contains(&name) && !keep.iter().any(|k| k == &json!(name)) {
                                keep.push(json!(name));
                            }
                        }
                    }
                }
                if keep.is_empty() {
                    keep = METRIC_IDS.iter().map(|m| json!(m)).collect();
                }
                sec["metrics"] = Value::Array(keep);
            }
            out.push(sec);
        }
    }
    if out.is_empty() {
        return default_layout();
    }
    let rank = |k: &str| match k {
        "tasks" => 0,
        "metrics" => 1,
        _ => 2,
    };
    out.sort_by_key(|s| rank(s.get("kind").and_then(Value::as_str).unwrap_or("")));
    json!({"sections": out})
}

/// 看板数据根：`RSI_HOME`（代码根）优先，退化到工作区。
///
/// 为什么改（v11，I-20261108-14「新窗口看板没变化」）：v10 只修了「能力在哪」，
/// 数据仍绑 `WORKSPACE_ROOT`，于是每个窗口各有一块板——主窗口 22 条、子窗口 0 条，
/// 两边的板永不相见。看板是 agent 与用户之间的**账**，账只有一本；工作区只是
/// 「这本账当前记到哪个项目」。
///
/// 与 `scripts/board.py::data_root` 判定顺序必须逐字一致，否则 CLI 与 UI
/// 会写两份文件——那是最难查的一种不一致。
pub(super) fn data_root() -> PathBuf {
    for key in ["RSI_HOME", "WORKSPACE_ROOT"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    std::env::current_dir().unwrap_or_default()
}

pub(super) fn board_path() -> PathBuf {
    let raw = std::env::var("RSI_BOARD_PATH").unwrap_or_default();
    let raw = raw.trim();
    let p = if raw.is_empty() {
        PathBuf::from(".rsi").join("board.json")
    } else {
        PathBuf::from(raw)
    };
    if p.is_absolute() {
        p
    } else {
        data_root().join(p)
    }
}

fn now_iso() -> String {
    // 无 chrono 依赖（不新增 crate）：用 SystemTime 秒数拼一个可排序的固定宽度时间戳。
    // 前端只做展示与排序，不需要人类可读的 RFC3339——但为了与 board.py 的
    // `%Y-%m-%dT%H:%M:%SZ` 同源可比，这里换算为 UTC 分解后拼装。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d, hh, mm, ss) = civil_from_unix(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn today_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d, _, _, _) = civil_from_unix(secs);
    format!("{y:04}{m:02}{d:02}")
}

/// Unix 秒 → (Y, M, D, h, m, s)。Howard Hinnant 的 civil_from_days 算法（无外部依赖）。
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    ((y), m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

// ── G4a：跨进程写锁（与 `scripts/board.py` 的 `BoardLock` 是同一把文件锁）──────
//
// 为什么必须有：`board.py`（agent 侧）与 `POST /api/board`（UI 侧）都是
// 「load → 改 → save」。两路并行时 A 读到旧快照、B 写入新条目、A 再整体覆盖 →
// B 的条目凭空消失。
//
// 这里曾写着一句错的技术理由：「tmp/rename 原子替换，因此并发写不会互相吞条目」。
// 原子替换保证的是**不会写出半截文件**，不是**两次写不会互相覆盖**：
//   A: load(v1) → 加条目x → save(v1+x)
//   B: load(v1) → 加条目y → save(v1+y)   ← x 被整体覆盖，文件是完整的 v1+y
// 完整但少一条，正是最难查的那种「我明明加了，看板上没有」。
//
// 选型与 Python 侧一致（两条约束是硬的）：
// 1. **文件锁而非内存锁**：写者是两个不同进程（Python CLI / Rust host），
//    进程内 `Mutex` 拦不住对方。
// 2. **`create_new`（O_CREAT|O_EXCL）而非 flock**：Windows 上无 `flock`，
//    `LockFileEx` 语义按字节区间、跨语言实现易漂移。独占创建空文件跨平台一致，
//    代价是崩溃可能留残锁——故按 mtime 年龄回收（`LOCK_STALE`）。
// 3. 拿不到锁**明确失败**，不静默降级：宁可让用户看到「正在被写入，请重试」，
//    也不让两路写入互相覆盖。
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(15);
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

fn lock_path() -> PathBuf {
    board_path().with_file_name("board.lock")
}

/// 跨进程独占锁守卫：`Drop` 时删锁文件（含早退与 panic 展开路径）。
pub(super) struct BoardLock {
    path: PathBuf,
}

impl BoardLock {
    /// 取锁。成功返回守卫；超时返回 `Err(锁文件路径)`（调用方据此回 409）。
    pub(super) async fn acquire() -> Result<Self, String> {
        let path = lock_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // 残锁回收：持锁进程崩了，锁文件会留下。按 mtime 年龄判定；
                    // 读不到 mtime 视为超龄（否则一个坏文件会永久堵死看板）。
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .map(|age| age > LOCK_STALE)
                        .unwrap_or(true);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(path.display().to_string());
                    }
                    // 异步睡眠而非 thread::sleep：不阻塞 runtime 线程。
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
    }
}

impl Drop for BoardLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn empty_board() -> Value {

    json!({"version": 1, "updated_at": now_iso(), "tasks": [], "inspirations": [], "archived_tasks": []})
}

/// 读板：缺文件 → 空板；坏 JSON / 非对象 → 空板 + `corrupt:true`（原文件不动）。
pub(super) fn load_board() -> Value {
    let p = board_path();
    let Ok(text) = std::fs::read_to_string(&p) else {
        return empty_board();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(obj)) => {
            let mut board = empty_board();
            for key in ["tasks", "inspirations", "archived_tasks"] {
                if let Some(Value::Array(items)) = obj.get(key) {
                    let kept: Vec<Value> = items.iter().filter(|x| x.is_object()).cloned().collect();
                    board[key] = Value::Array(kept);
                }
            }
            if let Some(Value::String(u)) = obj.get("updated_at") {
                board["updated_at"] = json!(u);
            }
            // C 轮：layout 是看板的**表现层配置**，与任务/灵感同文件——读时必须原样带出，
            // 否则每次 GET 都退回兜底布局，`layout --file` 写的配置永远看不见。
            if let Some(lay) = obj.get("layout") {
                board["layout"] = lay.clone();
            }
            board        }
        _ => {
            let mut board = empty_board();
            board["corrupt"] = json!(true);
            board
        }
    }
}

/// 原子写回：tmp 同目录 + rename（NTFS 上是替换语义，崩溃不留半截 JSON）。
pub(super) fn save_board(board: &mut Value) -> std::io::Result<PathBuf> {
    let p = board_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    board["version"] = json!(1);
    board["updated_at"] = json!(now_iso());
    if let Some(obj) = board.as_object_mut() {
        obj.remove("corrupt");
    }
    let text = serde_json::to_string_pretty(board).unwrap_or_else(|_| "{}".into());
    let tmp = p.with_file_name(match p.file_name().and_then(|n| n.to_str()) {
        Some(n) => format!("{n}.tmp"),
        None => "board.json.tmp".into(),
    });
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

fn next_id(items: &[Value], prefix: &str) -> String {
    let stamp = today_stamp();
    let used: Vec<&str> = items.iter().filter_map(|i| i.get("id").and_then(Value::as_str)).collect();
    for n in 1..100 {
        let cand = format!("{prefix}-{stamp}-{n:02}");
        if !used.contains(&cand.as_str()) {
            return cand;
        }
    }
    format!("{prefix}-{stamp}-99")
}

fn err(code: &str, message: impl Into<String>, field: Option<&str>) -> Value {
    let mut e = json!({"code": code, "message": message.into()});
    if let Some(f) = field {
        e["field"] = json!(f);
    }
    json!({"ok": false, "error": e})
}

fn take_str(v: &Value, field: &str, limit: usize) -> Result<String, Value> {
    let Some(s) = v.get(field).and_then(Value::as_str) else {
        return Err(err("K400", format!("{field} 缺失或非字符串"), Some(field)));
    };
    let s = s.trim();
    if s.is_empty() {
        return Err(err("K400", format!("{field} 不能为空"), Some(field)));
    }
    if s.chars().count() > limit {
        return Err(err("K400", format!("{field} 超过 {limit} 字符"), Some(field)));
    }
    Ok(s.to_string())
}

fn parse_tags(v: &Value) -> Result<Vec<String>, Value> {
    let Some(Value::Array(arr)) = v.get("tags") else {
        return Ok(Vec::new());
    };
    if arr.len() > MAX_TAGS {
        return Err(err("K400", format!("tags 最多 {MAX_TAGS} 个"), Some("tags")));
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, t) in arr.iter().enumerate() {
        let Some(s) = t.as_str() else {
            return Err(err("K400", format!("tags[{i}] 必须是字符串"), Some("tags")));
        };
        let s = s.trim();
        if s.is_empty() || s.chars().count() > MAX_TAG {
            return Err(err("K400", format!("tags[{i}] 长度须为 1..={MAX_TAG}"), Some("tags")));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

fn workspace_tag() -> String {
    let ws = config::workspace_root();
    let s = ws.trim();
    if s.is_empty() {
        return String::new();
    }
    // 取目录名而不是完整路径：完整路径在 UI 里是噪声，换机器还会失真。
    // 与 `scripts/board.py::workspace_tag` 同一口径（都取最后一段），
    // 否则同一台机器上 UI 记的标签与 CLI 记的标签对不上，过滤就永远半空。
    Path::new(s)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .take(64)
        .collect()
}

/// GET /api/board：返回整块看板 + 路径 + 汇总计数（前端一次拿全，不做多次往返）。
pub(crate) async fn get_board(stream: &mut TcpStream) -> anyhow::Result<()> {
    let board = load_board();
    let tasks = board.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
    // C 轮：layout 规整一次后同时用于「回显给前端」与「兜底」——前端因此永远拿到
    // 一份**合法且顺序已定**的章节表，不必自己再判空/排序（判空逻辑只在一处）。
    let layout = normalize_layout(board.get("layout"));
    let mut by_verdict: Map<String, Value> = Map::new();
    for v in ["none", "pending", "pass", "fail"] {
        let n = tasks
            .iter()
            .filter(|t| {
                t.get("evaluation")
                    .and_then(|e| e.get("verdict"))
                    .and_then(Value::as_str)
                    .unwrap_or("none")
                    == v
            })
            .count();
        by_verdict.insert(v.to_string(), json!(n));
    }    let insps = board.get("inspirations").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut by_status: Map<String, Value> = Map::new();
    for s in TASK_STATUSES {
        let n = tasks.iter().filter(|t| t.get("status").and_then(Value::as_str) == Some(s)).count();
        by_status.insert((*s).to_string(), json!(n));
    }
    let mut insp_by: Map<String, Value> = Map::new();
    for s in INSP_STATUSES {
        let n = insps.iter().filter(|i| i.get("status").and_then(Value::as_str) == Some(s)).count();
        insp_by.insert((*s).to_string(), json!(n));
    }
    // v12（B 方案）：按归属工作区切片 + 回显当前工作区。
    // 前端据此提供「本项目 / 全部」两档视图：同一份数据，两种投影，账仍只有一本。
    // 空标签（旧条目）单独归到 `""` 下，前端在两个视图里都显示它——宁可多显示，
    // 也不让老条目凭空消失（那会被理解成丢账）。
    let current_ws = workspace_tag();
    let mut ws_of_task: Map<String, Value> = Map::new();
    let mut ws_of_insp: Map<String, Value> = Map::new();
    for t in &tasks {
        let k = t.get("workspace").and_then(Value::as_str).unwrap_or("").to_string();
        let cur = ws_of_task.get(&k).and_then(Value::as_u64).unwrap_or(0);
        ws_of_task.insert(k, json!(cur + 1));
    }
    for i in &insps {
        let k = i.get("workspace").and_then(Value::as_str).unwrap_or("").to_string();
        let cur = ws_of_insp.get(&k).and_then(Value::as_u64).unwrap_or(0);
        ws_of_insp.insert(k, json!(cur + 1));
    }
    let mut board = board;
    board["layout"] = layout;
    json_resp(
        stream,
        200,
        json!({
            "ok": true,
            "path": board_path().display().to_string(),
            // 回显的是**规整后**的 layout：前端拿到的永远合法，不必自己再判空/排序。
            "board": board,
            // 当前窗口所属工作区（目录名）；前端「本项目」档用它过滤。
            "workspace": current_ws,
            "by_workspace": {"tasks": Value::Object(ws_of_task), "inspirations": Value::Object(ws_of_insp)},
            "summary": {
                "tasks": {"total": tasks.len(), "by_status": Value::Object(by_status),
                          "by_verdict": Value::Object(by_verdict)},
                "inspirations": {"total": insps.len(), "by_status": Value::Object(insp_by)},
            },
            "limits": {"tasks": MAX_TASKS, "inspirations": MAX_INSP, "sections": MAX_SECTIONS},
        }),
    )
    .await
}

/// POST /api/board：UI 侧支持的三个 op。
///
/// `inspiration.add` / `inspiration.set` / `refresh`——**故意不提供 task 写 op**：
/// 任务由 agent 经 `scripts/board.py` 建与推进；用户在 UI 只能记灵感、归档灵感，
/// 这正是「用户不管背后的项目分类，agent 自己分」的界面落点。
pub(crate) async fn post_board(stream: &mut TcpStream, body: &[u8]) -> anyhow::Result<()> {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return json_resp(stream, 400, err("K400", format!("body 非法 JSON: {e}"), None)).await;
        }
    };
    let Some(op) = req.get("op").and_then(Value::as_str) else {
        return json_resp(stream, 400, bad_request("缺 op", Some("op"))).await;
    };
    // G4a：读→改→写整段持锁。锁必须**在 load 之前**拿——先 load 后取锁等于
    // 拿旧快照去覆盖别人的新写入，锁就白加了。
    let _lock = match BoardLock::acquire().await {
        Ok(g) => g,
        Err(why) => {
            // 拿不到锁明确失败，不静默降级为无锁写（那正是丢更新的来源）。
            return json_resp(
                stream,
                409,
                err(
                    "K409",
                    format!("看板正在被另一个写入方修改（{why}），请稍后重试"),
                    Some("board"),
                ),
            )
            .await;
        }
    };
    let mut board = load_board();
    if board.get("corrupt").and_then(Value::as_bool) == Some(true) {
        // 坏板拒绝写入：否则会把用户手改的正确内容按空板覆盖掉。
        return json_resp(
            stream,
            409,
            err(
                "K409",
                format!("看板文件无法解析，拒绝写入以免覆盖：{}（请先修正或删除该文件）", board_path().display()),
                None,
            ),
        )
        .await;
    }
    match op {
        "inspiration.add" => {
            let text = match take_str(&req, "text", MAX_INSP_TEXT) {
                Ok(t) => t,
                Err(e) => return json_resp(stream, 400, e).await,
            };
            let tags = match parse_tags(&req) {
                Ok(t) => t,
                Err(e) => return json_resp(stream, 400, e).await,
            };
            let items = board["inspirations"].as_array().cloned().unwrap_or_default();
            if items.len() >= MAX_INSP {
                return json_resp(
                    stream,
                    400,
                    err("LIMIT", format!("灵感数已达上限 {MAX_INSP}，请先归档"), Some("inspirations")),
                )
                .await;
            }
            let id = next_id(&items, "I");
            let ts = now_iso();
            // v12：灵感也记归属工作区（与任务同口径）。用户在哪个窗口记的灵感，
            // 「本项目」视图里就该看得见——否则灵感侧永远只能在「全部」里翻。
            let item = json!({
                "id": id, "text": text, "tags": tags, "status": "open", "tasks": [],
                "workspace": workspace_tag(), "created_at": ts, "updated_at": ts,
            });
            let mut merged = vec![item.clone()];
            merged.extend(items);
            board["inspirations"] = Value::Array(merged);
            match save_board(&mut board) {
                Ok(p) => json_resp(stream, 200, json!({"ok": true, "id": id, "path": p.display().to_string()})).await,
                Err(e) => json_resp(stream, 500, err("K500", format!("写盘失败: {e}"), None)).await,
            }
        }
        "inspiration.set" => {
            let id = match take_str(&req, "id", 40) {
                Ok(v) => v,
                Err(e) => return json_resp(stream, 400, e).await,
            };
            let status = match take_str(&req, "status", 16) {
                Ok(s) if INSP_STATUSES.contains(&s.as_str()) => s,
                Ok(s) => {
                    return json_resp(
                        stream,
                        400,
                        err("K400", format!("status 必须是 {INSP_STATUSES:?} 之一，收到 {s:?}"), Some("status")),
                    )
                    .await
                }
                Err(e) => return json_resp(stream, 400, e).await,
            };
            let mut items = board["inspirations"].as_array().cloned().unwrap_or_default();
            let Some(slot) = items.iter_mut().find(|i| i.get("id").and_then(Value::as_str) == Some(id.as_str())) else {
                return json_resp(stream, 404, err("K404", format!("未找到灵感 {id}"), Some("id"))).await;
            };
            slot["status"] = json!(status);
            slot["updated_at"] = json!(now_iso());
            board["inspirations"] = Value::Array(items);
            match save_board(&mut board) {
                Ok(_) => json_resp(stream, 200, json!({"ok": true, "id": id, "status": status})).await,
                Err(e) => json_resp(stream, 500, err("K500", format!("写盘失败: {e}"), None)).await,
            }
        }
        // C 轮：布局写入。UI/agent 都能改章节组合，但**只能从白名单里挑**。
        // 逐项预校验（kind/id/重复/metrics），**报出具体下标**并整体拒绝——不做「规整后
        // 条数比对」：normalize_layout 在无合法项时会回退兜底布局，条数失配会变成误导文案。
        "layout.set" => {
            let Some(Value::Object(obj)) = req.get("layout") else {
                return json_resp(stream, 400, err("K400", "缺 layout 对象", Some("layout"))).await;
            };
            let Some(Value::Array(raw_secs)) = obj.get("sections") else {
                return json_resp(stream, 400, err("K400", "layout.sections 必须是数组", Some("sections"))).await;
            };
            if raw_secs.is_empty() {
                return json_resp(stream, 400, err("K400", "layout.sections 不能为空", Some("sections"))).await;
            }
            if raw_secs.len() > MAX_SECTIONS {
                return json_resp(
                    stream,
                    400,
                    err("K400", format!("layout.sections 最多 {MAX_SECTIONS} 个"), Some("sections")),
                )
                .await;
            }
            let mut seen: Vec<&str> = Vec::new();
            for (i, sec) in raw_secs.iter().enumerate() {
                let Some(obj_i) = sec.as_object() else {
                    return json_resp(stream, 400, err("K400", format!("sections[{i}] 必须是对象"), Some("sections"))).await;
                };
                let id = obj_i.get("id").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() || id.chars().count() > 24 {
                    return json_resp(
                        stream,
                        400,
                        err("K400", format!("sections[{i}].id 须为 1..=24 字符"), Some("sections")),
                    )
                    .await;
                }
                if seen.contains(&id) {
                    return json_resp(
                        stream,
                        400,
                        err("K400", format!("sections[{i}].id 重复：{id}"), Some("sections")),
                    )
                    .await;
                }
                seen.push(id);
                let kind = obj_i.get("kind").and_then(Value::as_str).unwrap_or("");
                if !LAYOUT_KINDS.contains(&kind) {
                    return json_resp(
                        stream,
                        400,
                        err(
                            "K400",
                            format!("sections[{i}].kind 必须是 {LAYOUT_KINDS:?} 之一，收到 {kind:?}"),
                            Some("sections"),
                        ),
                    )
                    .await;
                }
                if kind == "metrics" {
                    if let Some(Value::Array(ms)) = obj_i.get("metrics") {
                        for m in ms {
                            let name = m.as_str().unwrap_or("");
                            if !METRIC_IDS.contains(&name) {
                                return json_resp(
                                    stream,
                                    400,
                                    err(
                                        "K400",
                                        format!("sections[{i}].metrics 只能取 {METRIC_IDS:?}，收到 {name:?}"),
                                        Some("metrics"),
                                    ),
                                )
                                .await;
                            }
                        }
                    }
                }
                if kind == "tasks" {
                    if let Some(lim) = obj_i.get("limit").and_then(Value::as_u64) {
                        if !(1..=200).contains(&lim) {
                            return json_resp(
                                stream,
                                400,
                                err("K400", format!("sections[{i}].limit 须为 1..=200"), Some("sections")),
                            )
                            .await;
                        }
                    }
                }
            }
            let normalized = normalize_layout(Some(&Value::Object(obj.clone())));
            board["layout"] = normalized.clone();
            match save_board(&mut board) {
                Ok(_) => json_resp(stream, 200, json!({"ok": true, "layout": normalized})).await,
                Err(e) => json_resp(stream, 500, err("K500", format!("写盘失败: {e}"), None)).await,
            }
        }
        // D 轮：改灵感正文/标签。B 轮只给了 add/set——用户实测「记错了只能归档再重打一遍」。
        // G1（I-20261108-04 第 1 条）：本 op 是**用户主权**的实现——host `/api/board` 的
        // 唯一调用方是 UI，agent 侧无对应 CLI（`insp-edit`/`insp-rm`/`insp-add` 已移除）。
        // 契约：text ≤2000、tags ≤10×32，原子写回。
        "inspiration.edit" => {
            let id = match take_str(&req, "id", 40) {
                Ok(v) => v,
                Err(e) => return json_resp(stream, 400, e).await,
            };
            let mut items = board["inspirations"].as_array().cloned().unwrap_or_default();
            let Some(slot) = items.iter_mut().find(|i| i.get("id").and_then(Value::as_str) == Some(id.as_str())) else {
                return json_resp(stream, 404, err("K404", format!("未找到灵感 {id}"), Some("id"))).await;
            };
            // 两字段都可选，但**至少要改一个**：全空的 edit 是误调用，静默成功会掩盖前端 bug。
            let has_text = req.get("text").is_some();
            let has_tags = req.get("tags").is_some();
            if !has_text && !has_tags {
                return json_resp(stream, 400, err("K400", "text 与 tags 至少给一个", Some("text"))).await;
            }
            if has_text {
                match take_str(&req, "text", MAX_INSP_TEXT) {
                    Ok(t) => slot["text"] = json!(t),
                    Err(e) => return json_resp(stream, 400, e).await,
                }
            }
            if has_tags {
                match parse_tags(&req) {
                    Ok(t) => slot["tags"] = json!(t),
                    Err(e) => return json_resp(stream, 400, e).await,
                }
            }
            slot["updated_at"] = json!(now_iso());
            let out = slot.clone();
            board["inspirations"] = Value::Array(items);
            match save_board(&mut board) {
                Ok(_) => json_resp(stream, 200, json!({"ok": true, "inspiration": out})).await,
                Err(e) => json_resp(stream, 500, err("K500", format!("写盘失败: {e}"), None)).await,
            }
        }
        // D 轮：删除灵感。已采纳（adopted）默认拒绝——它已经是任务的来源记录，删掉会让
        // 任务凭空出现；要删必须显式 force。G1：与 agent 侧 CLI **不同**——`insp-rm` 已
        // 从 board.py 移除，删除是用户自己的权利（UI 唯一入口），agent 只读灵感。
        "inspiration.rm" => {
            let id = match take_str(&req, "id", 40) {
                Ok(v) => v,
                Err(e) => return json_resp(stream, 400, e).await,
            };
            let force = req.get("force").and_then(Value::as_bool).unwrap_or(false);
            let items = board["inspirations"].as_array().cloned().unwrap_or_default();
            let Some(slot) = items.iter().find(|i| i.get("id").and_then(Value::as_str) == Some(id.as_str())) else {
                return json_resp(stream, 404, err("K404", format!("未找到灵感 {id}"), Some("id"))).await;
            };
            if slot.get("status").and_then(Value::as_str) == Some("adopted") && !force {
                let tasks: Vec<&str> = slot
                    .get("tasks")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                return json_resp(
                    stream,
                    409,
                    err(
                        "K409",
                        format!("灵感 {id} 已被任务采纳（{}）：删除会断掉任务来源链，确认请带 force:true", tasks.join(", ")),
                        Some("id"),
                    ),
                )
                .await;
            }
            let kept: Vec<Value> = items
                .into_iter()
                .filter(|i| i.get("id").and_then(Value::as_str) != Some(id.as_str()))
                .collect();
            board["inspirations"] = Value::Array(kept);
            match save_board(&mut board) {
                Ok(_) => json_resp(stream, 200, json!({"ok": true, "id": id})).await,
                Err(e) => json_resp(stream, 500, err("K500", format!("写盘失败: {e}"), None)).await,
            }
        }
        // D 轮：改灵感正文/标签。B 轮只给了 add/set——用户实测「记错了只能归档再重打一遍」。

        "refresh" => {
            let mut fixed = 0u32;
            let mut tasks = board["tasks"].as_array().cloned().unwrap_or_default();
            for t in tasks.iter_mut() {
                let Some(obj) = t.as_object_mut() else { continue };
                for (k, dv) in [("status", json!("planning")), ("priority", json!("P2")),
                                ("plan", json!([])), ("acceptance", json!([])),
                                ("inspirations", json!([])), ("artifacts", json!([]))] {
                    if !obj.contains_key(k) {
                        obj.insert(k.into(), dv);
                        fixed += 1;
                    }
                }
                if !obj.get("resourcing").map(Value::is_object).unwrap_or(false) {
                    obj.insert("resourcing".into(), json!({"model": "current", "effort": "medium",
                        "skills": [], "tools": [], "budget_secs": 0}));
                    fixed += 1;
                }
                if !obj.get("progress").map(Value::is_object).unwrap_or(false) {
                    obj.insert("progress".into(), json!({"done": 0, "total": 0, "note": ""}));
                    fixed += 1;
                }
                // title 是看板唯一必读字段：缺失/空白/超长一律归一（前端按字数截断显示，
                // 但落盘必须是规整值——否则看板与 board.py 的校验口径会漂移）。
                let bad_title = match obj.get("title").and_then(Value::as_str) {
                    Some(s) => s.trim().is_empty() || s.chars().count() > MAX_TITLE,
                    None => true,
                };
                if bad_title {
                    let keep: String = obj
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(MAX_TITLE)
                        .collect();
                    let t = if keep.is_empty() { "(未命名任务)".to_string() } else { keep };
                    obj.insert("title".into(), json!(t));
                    fixed += 1;
                }
            }
            board["tasks"] = Value::Array(tasks);
            let mut insps = board["inspirations"].as_array().cloned().unwrap_or_default();
            for i in insps.iter_mut() {
                let Some(obj) = i.as_object_mut() else { continue };
                for (k, dv) in [("tags", json!([])), ("tasks", json!([])), ("status", json!("open"))] {
                    if !obj.contains_key(k) {
                        obj.insert(k.into(), dv);
                        fixed += 1;
                    }
                }
            }
            board["inspirations"] = Value::Array(insps);
            match save_board(&mut board) {
                Ok(p) => json_resp(stream, 200, json!({"ok": true, "fixed": fixed, "path": p.display().to_string()})).await,
                Err(e) => json_resp(stream, 500, err("K500", format!("写盘失败: {e}"), None)).await,
            }
        }
        other => {
            json_resp(
                stream,
                400,
                err(
                    "K400",
                    format!("未知 op: {other}（可选 inspiration.add / inspiration.set / layout.set / refresh；任务写走 scripts/board.py）"),
                    Some("op"),
                ),
            )
            .await
        }
    }
}

/// GET /api/board/link?id=T-...：反查引用过该任务/灵感的对话轮次（D4）。
///
/// 数据来源是 trace 的 `user` 事件里的 `refs` 字段（chat.rs 在写 user 事件时落盘）。
/// 为什么不让前端自己读 trace：trace 在 `MEMORY_DATA_DIR` 下、可能上百万字节，
/// 前端全量拉一遍只为找几条引用是不划算的；而且**筛选口径只该有一处**（
/// `scripts/board.py link` 是同一口径的 CLI 版），两边分头实现必然漂移。
///
/// 只读、无写面。session 名过安全校验（它会被拼进文件名）。
pub(crate) async fn get_board_link(stream: &mut TcpStream, query: &str) -> anyhow::Result<()> {
    let q: std::collections::HashMap<String, String> = query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), url_decode(v)))
        })
        .collect();
    let id = q.get("id").cloned().unwrap_or_default();
    if id.is_empty() {
        return json_resp(stream, 400, bad_request("缺 id 参数", Some("id"))).await;
    }
    let session = q.get("session").cloned().unwrap_or_else(|| "main".into());
    // session 来自 URL：只允许字母/数字/连字符/下划线/#（与 trace_file_for 的校验同向）。
    if session.is_empty()
        || session.len() > 64
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '#'))
    {
        return json_resp(stream, 400, bad_request("session 名不合法", Some("session"))).await;
    }
    let limit: usize = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(12)
        .clamp(1, 100);

    let dir = match std::env::var("MEMORY_DATA_DIR") {
        Ok(v) if !v.trim().is_empty() => std::path::PathBuf::from(v.trim()).join("traces"),
        _ => std::path::PathBuf::from(config::workspace_root())
            .join("runtime")
            .join("plugins")
            .join("memory")
            .join("data")
            .join("traces"),
    };
    let path = dir.join(format!("{session}.jsonl"));
    let mut turns: Vec<Value> = Vec::new();
    let mut user_index: i64 = -1;
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
            if ev.get("type").and_then(Value::as_str) != Some("user") {
                continue;
            }
            user_index += 1;
            let Some(Value::Array(refs)) = ev.get("refs") else { continue };
            if !refs.iter().any(|r| r.as_str() == Some(id.as_str())) {
                continue;
            }
            let text_preview: String = ev
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            turns.push(json!({
                "user_index": user_index,
                "ts": ev.get("ts").cloned().unwrap_or(Value::Null),
                "text": text_preview,
                "refs": refs,
            }));
        }
    }
    let total = turns.len();
    if turns.len() > limit {
        turns = turns.split_off(total - limit);
    }
    json_resp(
        stream,
        200,
        json!({
            "ok": true,
            "id": id,
            "session": session,
            "turns": turns,
            "total": total,
            "trace": path.display().to_string(),
        }),
    )
    .await
}

/// 极简 URL 解码（只处理 %XX 与 +；query 里的 id/session 形状受限，不需要完整实现）。
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
/// 看板文件是否位于工作区内（诊断用：UI 与 agent 必须看到同一份文件）。
#[allow(dead_code)]
pub(super) fn board_in_workspace() -> bool {
    let ws = Path::new(&config::workspace_root()).to_path_buf();
    let Ok(ws) = ws.canonicalize() else { return false };
    let Ok(b) = board_path().canonicalize() else { return true }; // 尚未创建 = 正常（首次读）
    b.starts_with(&ws)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// civil_from_unix 必须与人类历法对齐（无 chrono 依赖时的正确性锚点）。
    #[test]
    fn civil_from_unix_matches_known_dates() {
        // 1970-01-01T00:00:00Z
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-11-07T00:00:00Z = 1794009600
        assert_eq!(civil_from_unix(1_794_009_600), (2026, 11, 7, 0, 0, 0));
        // 闰日 2024-02-29T12:34:56Z = 1709210096
        assert_eq!(civil_from_unix(1_709_210_096), (2024, 2, 29, 12, 34, 56));
        // 年末边界 2025-12-31T23:59:59Z = 1767225599
        assert_eq!(civil_from_unix(1_767_225_599), (2025, 12, 31, 23, 59, 59));
    }

    #[test]
    fn next_id_skips_used_and_is_shape_valid() {
        let used = vec![json!({"id": "I-20261107-01"}), json!({"id": "I-20261107-02"})];
        let stamp = today_stamp();
        let id = next_id(&used, "I");
        assert!(id.starts_with(&format!("I-{stamp}-")), "got {id}");
        assert!(!used.iter().any(|u| u["id"] == json!(id)), "不得返回已用 ID");
        // 空表 → -01
        assert_eq!(next_id(&[], "T").rsplit('-').next().unwrap(), "01");
    }

    #[test]
    fn empty_and_corrupt_board_shapes() {
        // 空板形状：三键齐备、数组为空
        let b = empty_board();
        assert_eq!(b["version"], json!(1));
        assert!(b["tasks"].as_array().unwrap().is_empty());
        assert!(b["inspirations"].as_array().unwrap().is_empty());
        assert!(b.get("corrupt").is_none(), "全新空板不得带 corrupt 标记");
    }

    #[test]
    fn take_str_rejects_blank_and_overlong() {
        assert!(take_str(&json!({"text": "  "}), "text", 10).is_err(), "空白必须拒绝");
        assert!(take_str(&json!({}), "text", 10).is_err(), "缺失必须拒绝");
        assert!(take_str(&json!({"text": "一二三四五六七八九十一"}), "text", 10).is_err(), "超长必须拒绝");
        assert!(take_str(&json!({"text": " 正常 "}), "text", 10).is_ok());
    }

    #[test]
    fn parse_tags_bounds() {
        assert!(parse_tags(&json!({"tags": ["a", "b"]})).unwrap() == vec!["a", "b"]);
        assert!(parse_tags(&json!({})).unwrap().is_empty(), "缺省无标签");
        assert!(parse_tags(&json!({"tags": [""]})).is_err(), "空标签拒绝");
        let too_many: Vec<String> = (0..11).map(|i| format!("t{i}")).collect();
        assert!(parse_tags(&json!({"tags": too_many})).is_err(), "超量拒绝");
    }

    /// C 轮：布局规整——非法 kind / 重复 id / 空表各有确定结局，且**顺序被 kind 钉死**
    /// （手改乱序改变不了「任务→指标→灵感」，这是「有约束」的可执行断言）。
    #[test]
    fn normalize_layout_enforces_kinds_and_order() {
        // 空/缺失 → 兜底两章（任务 + 灵感）
        let d = default_layout();
        assert_eq!(normalize_layout(None), d, "缺失 layout 必须回退兜底");
        assert_eq!(normalize_layout(Some(&json!({"sections": []}))), d, "空 sections 同样兜底");

        // 乱序 + 混入非法项：合法项保留、非法项丢弃、顺序按 kind 重排
        let raw = json!({"sections": [
            {"id": "insp", "kind": "inspirations", "title": "灵感"},
            {"id": "bad", "kind": "unknown", "title": "x"},
            {"id": "m", "kind": "metrics", "metrics": ["verdict", "nope"]},
            {"id": "t", "kind": "tasks", "group_by": "nope", "filter": "nope", "limit": 9999},
            {"id": "t", "kind": "tasks", "title": "重复 id 丢弃"}
        ]});
        let out = normalize_layout(Some(&raw));
        let secs = out["sections"].as_array().unwrap();
        let kinds: Vec<&str> = secs.iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["tasks", "metrics", "inspirations"], "顺序必须被 kind 钉死");
        assert_eq!(secs[0]["group_by"], json!("status"));
        assert_eq!(secs[0]["filter"], json!("all"));
        assert_eq!(secs[0]["limit"], json!(200), "limit 必须夹到上界");
        assert_eq!(secs[1]["metrics"], json!(["verdict"]), "metrics 只留白名单内的");
    }

    /// C 轮：board.rs 的布局白名单必须与 scripts/board.py 同源（改一侧忘另一侧 = 前端白屏）。
    #[test]
    fn layout_whitelist_matches_python_cli() {
        let py_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scripts/board.py");
        let Ok(py) = std::fs::read_to_string(&py_path) else {
            return; // no Python CLI in this repo: nothing to keep in sync with
        };
        for k in LAYOUT_KINDS {
            assert!(py.contains(&format!("\"{k}\"")), "board.py 缺 kind {k}");
        }
        for g in GROUP_BYS {
            assert!(py.contains(&format!("\"{g}\"")), "board.py 缺 group_by {g}");
        }
        for f in FILTERS {
            assert!(py.contains(&format!("\"{f}\"")), "board.py 缺 filter {f}");
        }
        for m in METRIC_IDS {
            assert!(py.contains(&format!("\"{m}\"")), "board.py 缺 metric {m}");
        }
    }

    /// v11 数据根路径：两个 env 参与判定，而 cargo 多测共享**同一进程环境**。
    /// 并行的 board 相关用例互相 set_var/remove_var 会读到对方的中间态（实测：
    /// `board_follows_rsi_home_not_workspace` 读到另一个用例留下的 WORKSPACE_ROOT 而红）。
    /// 故这一组用互斥锁串行——不是掩盖问题，是承认「进程级环境」本身就是共享可变状态。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// board_path 必须落在**数据根**内的 .rsi/board.json（除非显式给了绝对路径）
    /// ——防 UI 与 agent 看两份文件。
    ///
    /// v11 语义变更：这个「根」从 `WORKSPACE_ROOT` 改成了 `data_root()`（RSI_HOME 优先）。
    /// 旧注释写的是「落在工作区内」，但它在多窗口形态下是**错的**：子窗口的
    /// WORKSPACE_ROOT 是用户项目目录，照那个断言写就会让每个窗口各自长出一块板。
    /// 真正的不变量是「两者解析到同一个文件」，而不是「落在工作区里」。
    #[test]
    fn board_path_is_workspace_relative_by_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("RSI_BOARD_PATH");
        let p = board_path();
        assert!(p.ends_with(Path::new(".rsi").join("board.json")), "got {p:?}");
        let root = data_root();
        assert!(p.starts_with(&root), "board_path 必须落在 data_root 内: {p:?} vs {root:?}");
    }

    /// v11 核心不变量：RSI_HOME 在场时，看板跟着**代码根**走，与 WORKSPACE_ROOT 无关。
    ///
    /// 这是「新窗口看板没变化」的另一半修法（数据侧）。子窗口把 WORKSPACE_ROOT
    /// 换成用户项目，若看板仍绑工作区，主窗口 22 条任务、子窗口 0 条，两本账永不相见。
    #[test]
    fn board_follows_rsi_home_not_workspace() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let code = std::env::temp_dir().join("rsi-board-test-code-root");
        let proj = std::env::temp_dir().join("rsi-board-test-project");
        let saved_home = std::env::var("RSI_HOME").ok();
        let saved_ws = std::env::var("WORKSPACE_ROOT").ok();
        let saved_bp = std::env::var("RSI_BOARD_PATH").ok();
        std::env::remove_var("RSI_BOARD_PATH");
        std::env::set_var("RSI_HOME", &code);
        std::env::set_var("WORKSPACE_ROOT", &proj);
        let p = board_path();
        match saved_home { Some(v) => std::env::set_var("RSI_HOME", v), None => std::env::remove_var("RSI_HOME") }
        match saved_ws { Some(v) => std::env::set_var("WORKSPACE_ROOT", v), None => std::env::remove_var("WORKSPACE_ROOT") }
        if let Some(v) = saved_bp { std::env::set_var("RSI_BOARD_PATH", v) }
        assert_eq!(p, code.join(".rsi").join("board.json"),
                   "RSI_HOME 在场时必须锚代码根，实际 {p:?}");
        assert!(!p.starts_with(&proj), "看板不得落在工作区（会让每个窗口各有一块板）");
    }

    /// v12「按工作区分板 vs 一本账 + 归属标签」的核心不变量：
    /// 标签取**目录名**，且与 `scripts/board.py::workspace_tag` 同口径——两边一旦取法
    /// 不同（一边全路径一边目录名），UI 记的标签与 CLI 记的就对不上，「本项目」档
    /// 永远半空，用户会以为是过滤坏了。
    #[test]
    fn workspace_tag_takes_last_path_segment() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os("WORKSPACE_ROOT");
        std::env::set_var("WORKSPACE_ROOT", "D:/ZH/vibe code/WF/1108");
        assert_eq!(workspace_tag(), "1108", "取目录名，不是全路径");
        // 尾随分隔符不能让标签变成空串（`Path::file_name` 已处理，此处钉住）
        std::env::set_var("WORKSPACE_ROOT", "D:/ZH/vibe code/WF/1108/");
        assert_eq!(workspace_tag(), "1108");
        // 未设置 → 空串（「不知道归属」，不是「归属叫空」）
        std::env::set_var("WORKSPACE_ROOT", "   ");
        assert_eq!(workspace_tag(), "", "空白工作区必须给空标签，不能编一个");
        match saved {
            Some(v) => std::env::set_var("WORKSPACE_ROOT", v),
            None => std::env::remove_var("WORKSPACE_ROOT"),
        }
    }

    /// 有界：标签会进 JSON 与 UI，必须截断（路径名可能极长）。
    #[test]
    fn workspace_tag_is_bounded() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os("WORKSPACE_ROOT");
        std::env::set_var("WORKSPACE_ROOT", format!("D:/tmp/{}", "x".repeat(200)));
        assert!(workspace_tag().chars().count() <= 64, "标签必须 ≤64 字符");
        match saved {
            Some(v) => std::env::set_var("WORKSPACE_ROOT", v),
            None => std::env::remove_var("WORKSPACE_ROOT"),
        }
    }
}
