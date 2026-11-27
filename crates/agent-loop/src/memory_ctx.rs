//! 记忆 L2 层装载（G 轮）：把 `.rsi/memory/L3/index.json` 的**时间轴元数据**注入系统提示词。
//!
//! 为什么需要：本仓库的记忆原本是两层——trace 全量原文（`memory/traces/*.jsonl`，单个
//! 会话动辄十几 MB）与画像（`.rsi/profile.json`，只有结论）。中间那层是空的，于是
//! 「上周那件事我们怎么定的」只有两个坏答案：读 trace 塞不进上下文，读画像记不住经过。
//!
//! 本模块补的是**最薄的一层**：只读 `L3/index.json`（几百字节/天），把「最近哪几天有活、
//! 各多大」注入提示词。要细节让模型按天精确打开 L2——而不是先把几 MB 原文读进上下文。
//!
//! ## 三条边界
//!
//! 1. **只读元数据，不读正文**。这里只碰 index.json 一个文件；`bytes_read` 与该文件
//!    大小相等是可核对的形式（见 `scripts/memory_consolidate.py::recall`）。
//! 2. **模型自己算不准日期**。`days_ago` 在服务端算好写进提示词——让模型拿当前日期去
//!    减「20261103」是它最容易错的算术之一。
//! 3. **有界且可空**。没有 index / 坏 JSON / 窗口内没记录 → 空串（**不注入空壳标题**：
//!    每轮 prompt 里一句「最近没记录」是纯噪声，还会让模型以为用户真的什么都没干）。
//!
//! 空间契约：只读。写通道唯一是 `scripts/memory_consolidate.py build`（确定性重算，
//! 派生视图不是独立账本——删掉它能从 trace 完全重建）。
//!
//! 可测性契约（沿用 C/D 轮结论）：**渲染与 I/O 分离**——`render_recall` 是纯函数
//! （入参 index Value + 今天的日期），`memory_context` 才做路径解析与读文件。

use serde_json::Value;
use std::path::PathBuf;

/// 注入块字符上限（超出即整段截断）。
const MAX_BLOCK_CHARS: usize = 900;
/// 注入时最多列几天（再多就不是「近期」了）。
const MAX_DAYS_IN_PROMPT: usize = 7;
/// 单行字符上限。
const MAX_LINE_CHARS: usize = 140;

/// 记忆数据的根目录：与 `board.py` / `profile.py` / `memory_consolidate.py` 同一判据、
/// 同一顺序（`RSI_HOME` → `WORKSPACE_ROOT` → cwd）。三处必须逐字一致，否则会出现
/// 「看板在代码根、记忆在项目目录」这种没人能想明白的分裂。
fn data_root() -> Option<PathBuf> {
    for key in ["RSI_HOME", "WORKSPACE_ROOT"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    std::env::current_dir().ok()
}

fn memory_root() -> Option<PathBuf> {
    let raw = std::env::var("RSI_MEMORY_DIR").unwrap_or_default();
    let raw = raw.trim();
    let p = if raw.is_empty() {
        PathBuf::from(".rsi").join("memory")
    } else {
        PathBuf::from(raw)
    };
    if p.is_absolute() {
        Some(p)
    } else {
        data_root().map(|root| root.join(p))
    }
}

fn index_path() -> Option<PathBuf> {
    memory_root().map(|r| r.join("L3").join("index.json"))
}

fn load_index() -> Option<Value> {
    let p = index_path()?;
    let text = std::fs::read_to_string(p).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.is_object().then_some(v)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// 把 `YYYYMMDD` 转成 `YYYY-MM-DD`；形状不符返回 None（脏数据当「没有这天」）。
fn pretty_day(day: &str) -> Option<String> {
    if day.len() != 8 || !day.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}-{}-{}", &day[0..4], &day[4..6], &day[6..8]))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// 纯函数：把 index 渲染成提示词块（不读文件、不读时钟——`today` 由调用方传入）。
///
/// 三种结局都明确：有记录 → 带标题注入；无记录 / 坏结构 → 空串（不注入空壳）；
/// 窗口外 → 空串（「最近 N 天」为空就是空，不要拿三个月前的活动冒充「最近」）。
pub(super) fn render_recall(index: &Value, today: &str, window_days: u32) -> String {
    let Some(days) = index.get("days").and_then(Value::as_array) else {
        return String::new();
    };
    let mut rows: Vec<(u32, String, &Value)> = Vec::new();
    for d in days.iter() {
        let day = s(d, "day");
        let Some(pretty) = pretty_day(day) else { continue };
        let Some(ago) = days_ago(day, today) else { continue };
        if ago > window_days {
            continue;
        }
        rows.push((ago, pretty, d));
    }
    if rows.is_empty() {
        return String::new();
    }
    // 越近越靠前：读「最近在忙什么」时的自然顺序。
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    rows.truncate(MAX_DAYS_IN_PROMPT);

    let total_sessions: u64 = rows.iter().map(|(_, _, d)| n(d, "sessions")).sum();
    let mut body = format!(
        "\n## Memory timeline (L2 layer, auto-loaded from .rsi/memory/L3/index.json)\n\
         Distilled from the append-only trace: what happened on which day, and how big it was. \
This is the **middle layer** — the trace has the full text, `.rsi/profile.json` has the \
durable conclusions, and this tells you which day to open.\n\
         Window: last {window_days} days · {total_sessions} sessions in {nd} day(s).\n\
         To read a day's detail open its L2 file under `.rsi/memory/L2/<YYYYMMDD>.md` \
(`python scripts/memory_consolidate.py recall --days {window_days}` gives the same list). \
Never re-read the raw trace to answer \\\"what did we do last week\\\" — open the L2 day instead.\n",
        nd = rows.len(),
    );
    for (ago, pretty, d) in rows.iter() {
        let when = match ago {
            0 => "today".to_string(),
            1 => "1 day ago".to_string(),
            k => format!("{k} days ago"),
        };
        let line = format!(
            "- {pretty} ({when}): {} session(s) / {} event(s) / {} file(s) changed / {} artifact(s)\n",
            n(d, "sessions"),
            n(d, "events"),
            n(d, "files"),
            n(d, "artifacts"),
        );
        body.push_str(&truncate(&line, MAX_LINE_CHARS));
        if !line.ends_with('\n') {
            body.push('\n');
        }
    }
    if body.chars().count() > MAX_BLOCK_CHARS {
        let cut: String = body.chars().take(MAX_BLOCK_CHARS).collect();
        return format!("{cut}\n…(memory timeline truncated)\n");
    }
    body
}

/// `YYYYMMDD` 距今多少天（**服务端算**，不让模型做日期减法）。
///
/// 用纯整数日期算法（Howard Hinnant 的 days_from_civil），不引入 chrono 依赖：
/// 只需两个公历日期之间的天数差，而 UTC 当天用 `SystemTime` 换算即可。
fn days_ago(day: &str, today: &str) -> Option<u32> {
    let a = civil_days(day)?;
    let b = civil_days(today)?;
    if b < a {
        return None; // 未来日期（时钟回拨 / 手写脏数据）→ 当作不存在
    }
    u32::try_from(b - a).ok()
}

/// `YYYYMMDD` → 自 1970-01-01 起的天数（含闰年规则）。
fn civil_days(day: &str) -> Option<i64> {
    if day.len() != 8 || !day.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i64 = day[0..4].parse().ok()?;
    let m: i64 = day[4..6].parse().ok()?;
    let d: i64 = day[6..8].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms"：days_from_civil
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // Mar=0 … Feb=11
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

/// 今天（UTC）的 `YYYYMMDD`。
///
/// 固定 UTC 而不是本地时区：L2 的文件名按 UTC 分天（`memory_consolidate.py::day_of`
/// 同一约定）。两侧一旦不同源，「今天」就会错一天，而错误表现是「昨天的活显示成今天」。
fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    // 反向 civil_from_days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}")
}

/// I/O 入口：读 index → 渲染。读不到（无根 / 无文件 / 坏 JSON / 窗口内空）一律空串。
pub(super) fn memory_context() -> String {
    let Some(index) = load_index() else {
        return String::new();
    };
    render_recall(&index, &today_utc(), 7)
}

/// 记忆通道说明（提示词纪律段；只在代码根真有 `scripts/memory_consolidate.py` 时注入）。
///
/// 门控锚定**代码根**（`config::rsi_home`）而不是 WORKSPACE_ROOT：后者是用户项目，
/// 「新窗口」换成别的目录后探测会失败 → 记忆通道整段消失。与看板/画像同源。
pub(super) fn memory_ops_hint() -> String {
    let Some(ws) = super::config::rsi_home() else {
        return String::new();
    };
    if !ws.join("scripts").join("memory_consolidate.py").is_file() {
        return String::new();
    }
    String::from(
        "\n### Layered memory (`.rsi/memory/`)\n\
         Three layers, don't confuse them: `memory/traces/*.jsonl` is the full append-only text \
(never read it whole — a single session can be tens of MB); `.rsi/memory/L2/<YYYYMMDD>.md` is \
one day's digest (open this when asked \"what did we do last week\"); `.rsi/profile.json` holds \
the durable conclusions. The timeline above is already loaded; use \
`python scripts/memory_consolidate.py recall --days N` to re-window it, and read the L2 file \
for the day you actually need. Do not write into `.rsi/memory/` — it is a derived view, rebuilt \
by `build` (hand edits are overwritten).\n\
         \n\
         ### Content search (`scripts/index.py`)\n\
         The timeline above answers what happened on a given day. When the question is where we decided\n\
         something (content, not date), do not grep the whole tree — it misses Chinese wording and\n\
         returns too many hits. Use the FTS5 index instead:\n\
         `python scripts/index.py search <question> --k 5` -> path:line + snippet, ranked by bm25.\n\
         A fresh clone needs `python scripts/index.py build` once (incremental afterwards;\n\
         `.rsi/index/fts.db` is a derived view, safe to delete). Prefer this over grep for prose;\n\
         keep grep/symbols_search for exact identifiers and code.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn idx() -> Value {
        json!({
            "days": [
                {"day": "20261111", "sessions": 1, "events": 900, "files": 20, "artifacts": 20},
                {"day": "20261109", "sessions": 2, "events": 5026, "files": 21, "artifacts": 20},
                {"day": "20261107", "sessions": 15, "events": 15950, "files": 103, "artifacts": 103},
                {"day": "20260901", "sessions": 3, "events": 10, "files": 1, "artifacts": 0}
            ]
        })
    }

    /// 窗口只留最近 N 天——三个月前的活动不能冒充「最近」。
    #[test]
    fn render_recall_windows_by_days() {
        let block = render_recall(&idx(), "20261111", 7);
        assert!(block.contains("2026-11-11"), "{block}");
        assert!(block.contains("2026-11-07"), "{block}");
        assert!(!block.contains("2026-09-01"), "窗口外的天不该出现: {block}");
    }

    /// days_ago 在服务端算好写进提示词（模型自己减日期最容易错）。
    #[test]
    fn render_recall_states_relative_age() {
        let block = render_recall(&idx(), "20261111", 7);
        assert!(block.contains("(today)"), "{block}");
        assert!(block.contains("(2 days ago)"), "{block}");
        assert!(block.contains("(4 days ago)"), "{block}");
    }

    /// 越近越靠前——读「最近在忙什么」时的自然顺序。
    #[test]
    fn render_recall_orders_newest_first() {
        let block = render_recall(&idx(), "20261111", 7);
        let a = block.find("2026-11-11").unwrap();
        let b = block.find("2026-11-07").unwrap();
        assert!(a < b, "新的一天应排在前面: {block}");
    }

    /// 空 / 坏结构 → 空串：不注入空壳标题（每轮 prompt 里一句「最近没记录」是纯噪声）。
    #[test]
    fn render_recall_empty_when_nothing_in_window() {
        assert_eq!(render_recall(&json!({}), "20261111", 7), "");
        assert_eq!(render_recall(&json!({"days": []}), "20261111", 7), "");
        // 只有窗口外的天
        assert_eq!(render_recall(&json!({"days": [{"day": "20200101"}]}), "20261111", 7), "");
        // 形状非法的天（脏数据）当不存在
        assert_eq!(render_recall(&json!({"days": [{"day": "2026-11-11"}]}), "20261111", 7), "");
    }

    /// 有界：天数与整块都截断，且截断显式标注（不静默少给）。
    #[test]
    fn render_recall_is_bounded() {
        let mut days = Vec::new();
        for i in 0..30 {
            days.push(json!({"day": format!("202611{:02}", i + 1), "sessions": 1, "events": 5}));
        }
        let block = render_recall(&json!({"days": days}), "20261231", 400);
        assert!(block.chars().count() <= MAX_BLOCK_CHARS + 64, "整块应被截断");
        assert!(block.contains("truncated"), "截断要有显式标记: {block}");
    }

    /// days_ago 的日期算术：跨月、跨年、闰日、未来、脏值。
    #[test]
    fn days_ago_handles_calendar_edges() {
        assert_eq!(days_ago("20261111", "20261111"), Some(0));
        assert_eq!(days_ago("20261104", "20261111"), Some(7));
        assert_eq!(days_ago("20251231", "20260101"), Some(1), "跨年");
        assert_eq!(days_ago("20260228", "20260301"), Some(1), "2026 非闰年");
        assert_eq!(days_ago("20240228", "20240301"), Some(2), "2024 闰年");
        assert_eq!(days_ago("20261112", "20261111"), None, "未来日期当不存在");
        assert_eq!(days_ago("2026111", "20261111"), None, "长度不符");
        assert_eq!(days_ago("20261301", "20261111"), None, "月份非法");
    }

    /// `today_utc` 必须产出 8 位数字——它是 L2 文件名的形状，错一天就全错位。
    #[test]
    fn today_utc_has_expected_shape() {
        let t = today_utc();
        assert_eq!(t.len(), 8, "{t}");
        assert!(t.bytes().all(|b| b.is_ascii_digit()), "{t}");
        assert!(civil_days(&t).is_some(), "{t}");
    }

    /// 与 Python 侧同一约定（UTC 分天）：钉死一个已知日期换算。
    #[test]
    fn civil_days_matches_known_epoch() {
        assert_eq!(civil_days("19700101"), Some(0));
        assert_eq!(civil_days("19700102"), Some(1));
        assert_eq!(civil_days("20261111"), Some(20768));
    }
}
