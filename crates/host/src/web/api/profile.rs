//! 画像（`.rsi/profile.json`）的 **只读 + 受限删除** Web 端点。
//!
//! 为什么在 host 而不是让前端直接读文件：画像的写入纪律在 `scripts/profile.py`
//! （形状校验、域上限、note ID 跨层唯一、原子替换）。前端若是自己改 JSON，就会
//! 绕过全部纪律——UI 里点一下"删掉这条观察"变成删错域、写坏形状的机会。
//!
//! 因此本模块**刻意只做两件事**：
//! 1. `GET /api/profile`：读全量（含域与观察），给 UI 展示；
//! 2. `POST /api/profile`：**只支持删除**（`note-rm` / `section-rm` / `domain-rm`）。
//!
//! 不提供「新增/编辑」的原因（与 B 轮看板写权限同一裁决线）：画像记的是 **agent 对一个
//! 人的结论**，结论该由观察产生；用户在 UI 上手写一条"我喜欢简洁"不是观察，是一条命令
//! ——命令该说在对话里，由 agent 落成画像。但**删除必须是用户的权力**：写错的结论会让
//! agent 每轮带着它犯同一个错，而它只写在文件里、用户看不见也改不掉。
//! 见 `docs/self-evolution/04-知识落层与多域上下文.md` §5。
//!
//! 数据落点：`<data_root>/.rsi/profile.json`（`RSI_PROFILE_PATH` 可覆盖，与 profile.py 同源）。
//! `data_root` = `RSI_HOME`（代码根）优先、退化到工作区——与 board 同一判据，
//! 所以画像与看板永远落在同一个根上。

use super::super::{bad_request, json_resp};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::net::TcpStream;

/// 与 `scripts/profile.py` 同源的形状上限（改一侧必须改另一侧，pytest 有在位断言）。
const MAX_DOMAINS: usize = 8;
const SECTION_KINDS: &[&str] = &["user", "model", "workflow", "env"];

/// 画像文件路径：`RSI_PROFILE_PATH` 覆盖 → `<data_root>/.rsi/profile.json`。
pub(super) fn profile_path() -> PathBuf {
    let raw = std::env::var("RSI_PROFILE_PATH").unwrap_or_default();
    let raw = raw.trim();
    let p = if raw.is_empty() {
        PathBuf::from(".rsi").join("profile.json")
    } else {
        PathBuf::from(raw)
    };
    if p.is_absolute() {
        p
    } else {
        super::board::data_root().join(p)
    }
}

/// 读画像。**坏文件不覆盖、不静默清零**（与 board 同纪律）：按空画像返回并带 `corrupt`，
/// 让用户看见「解析失败」而不是以为「我的画像本来就是空的」。
pub(super) fn load_profile() -> Value {
    let p = profile_path();
    let Ok(text) = std::fs::read_to_string(&p) else {
        return json!({
            "sections": [], "notes": [], "domains": {}, "current_domain": "",
            "exists": false
        });
    };
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&text) else {
        return json!({
            "sections": [], "notes": [], "domains": {}, "current_domain": "",
            "exists": true, "corrupt": true
        });
    };
    let mut out = serde_json::Map::new();
    out.insert("exists".into(), json!(true));
    for key in ["sections", "notes"] {
        let arr: Vec<Value> = obj
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter(|x| x.is_object()).cloned().collect())
            .unwrap_or_default();
        out.insert(key.into(), Value::Array(arr));
    }
    // 域：读侧宽容（形状不合法的键忽略而不是报错），与 profile.py::_clean_domain 同口径
    let mut domains = serde_json::Map::new();
    if let Some(Value::Object(doms)) = obj.get("domains") {
        for (name, body) in doms {
            if !valid_domain(name) || !body.is_object() {
                continue;
            }
            let mut d = serde_json::Map::new();
            for key in ["sections", "notes"] {
                let arr: Vec<Value> = body
                    .get(key)
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter(|x| x.is_object()).cloned().collect())
                    .unwrap_or_default();
                d.insert(key.into(), Value::Array(arr));
            }
            domains.insert(name.clone(), Value::Object(d));
        }
    }
    out.insert("domains".into(), Value::Object(domains));
    let cur = obj.get("current_domain").and_then(Value::as_str).unwrap_or("");
    out.insert("current_domain".into(), json!(if valid_domain(cur) { cur } else { "" }));
    if let Some(v) = obj.get("updated_at").and_then(Value::as_str) {
        out.insert("updated_at".into(), json!(v));
    }
    Value::Object(out)
}

/// 域名字形状（与 profile.py::DOMAIN_RE 同源）：小写字母/数字/连字符，≤24。
fn valid_domain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 删一条 note / 一个 section / 一个域，写回文件。
///
/// `domain` 空串 = 全局层。删域只删该域，**不改 `current_domain`**——用户可能正站在
/// 那个域里，静默改粘性状态会让下一轮注入的上下文与用户认知不一致；站在被删域上时
/// 由前端提示「当前域已不存在」，与 profile.py 的行为一致。
fn delete_from(profile: &mut Value, op: &str, id: &str, domain: &str) -> Result<(), (u16, Value)> {
    let key = if op == "note-rm" { "notes" } else { "sections" };
    let layer: &mut Value = if domain.is_empty() {
        profile
    } else {
        match profile.get_mut("domains").and_then(|d| d.get_mut(domain)) {
            Some(v) => v,
            None => {
                return Err((
                    404,
                    json!({"ok": false, "error": {"code": "K404",
                        "message": format!("未找到域 {domain}"), "field": "domain"}}),
                ))
            }
        }
    };
    let Some(arr) = layer.get_mut(key).and_then(Value::as_array_mut) else {
        return Err((400, bad_request(format!("{key} 不是数组"), Some("profile"))));
    };
    let before = arr.len();
    arr.retain(|x| x.get("id").and_then(Value::as_str) != Some(id));
    if arr.len() == before {
        return Err((
            404,
            json!({"ok": false, "error": {"code": "K404",
                "message": format!("未找到 {id}（{key} / 域 {})", if domain.is_empty() { "全局" } else { domain }),
                "field": "id"}}),
        ));
    }
    Ok(())
}

/// `GET /api/profile`：全量读（UI 展示用）。不裁剪——画像本身上限很小（章节/域/观察
/// 都有独立上限），裁剪反而让「我到底记了什么」看不全。
pub(crate) async fn get_profile(stream: &mut TcpStream) -> anyhow::Result<()> {
    let mut p = load_profile();
    if let Some(obj) = p.as_object_mut() {
        obj.insert("path".into(), json!(profile_path().display().to_string()));
        obj.insert("max_domains".into(), json!(MAX_DOMAINS));
        obj.insert("section_kinds".into(), json!(SECTION_KINDS));
    }
    json_resp(stream, 200, json!({"ok": true, "profile": p})).await
}

/// `POST /api/profile`：受限写——只接受三种删除 op（见模块注释）。
pub(crate) async fn post_profile(stream: &mut TcpStream, body: &[u8]) -> anyhow::Result<()> {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return json_resp(
                stream,
                400,
                json!({"ok": false, "error": {"code": "K400", "message": format!("body 非法 JSON: {e}")}}),
            )
            .await
        }
    };
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    if !matches!(op, "note-rm" | "section-rm" | "domain-rm") {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400",
                "message": "只支持删除操作（note-rm / section-rm / domain-rm）；新增与编辑请让 agent 经 profile.py 完成",
                "field": "op"}}),
        )
        .await;
    }
    let id = req.get("id").and_then(Value::as_str).unwrap_or("");
    let domain = req.get("domain").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return json_resp(stream, 400, bad_request("缺 id", Some("id"))).await;
    }
    if !domain.is_empty() && !valid_domain(domain) {
        return json_resp(stream, 400, bad_request("domain 形状不合法（小写字母/数字/连字符，≤24）", Some("domain"))).await;
    }

    let path = profile_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return json_resp(
            stream,
            404,
            json!({"ok": false, "error": {"code": "K404", "message": "画像文件不存在（还没有记过任何结论）"}}),
        )
        .await;
    };
    let mut profile: Value = match serde_json::from_str(&text) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            // 坏 JSON 不覆盖：拒绝写入，让用户去修（覆盖等于丢结论）
            return json_resp(
                stream,
                409,
                json!({"ok": false, "error": {"code": "K409",
                    "message": "profile.json 解析失败，拒绝写入（不让删除把坏文件覆盖成空画像）"}}),
            )
            .await;
        }
    };

    let result = if op == "domain-rm" {
        let Some(doms) = profile.get_mut("domains").and_then(Value::as_object_mut) else {
            return json_resp(stream, 404, json!({"ok": false, "error": {"code": "K404", "message": "没有域"}})).await;
        };
        if doms.remove(id).is_none() {
            return json_resp(
                stream,
                404,
                json!({"ok": false, "error": {"code": "K404", "message": format!("未找到域 {id}"), "field": "id"}}),
            )
            .await;
        }
        Ok(())
    } else {
        delete_from(&mut profile, op, id, domain)
    };

    if let Err((code, payload)) = result {
        return json_resp(stream, code, payload).await;
    }

    if let Some(obj) = profile.as_object_mut() {
        obj.insert("version".into(), json!(2));
        obj.insert("updated_at".into(), json!(now_iso()));
        obj.remove("corrupt");
    }
    if let Err(e) = save_profile(&profile) {
        return json_resp(
            stream,
            500,
            json!({"ok": false, "error": {"code": "K500", "message": format!("写画像失败: {e}")}}),
        )
        .await;
    }
    json_resp(stream, 200, json!({"ok": true, "op": op, "id": id, "profile": load_profile()})).await
}

/// 原子写（tmp + rename），与 board.rs / profile.py 同纪律。
fn save_profile(profile: &Value) -> std::io::Result<()> {
    let p = profile_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(profile).unwrap_or_else(|_| "{}".into());
    let tmp = p.with_file_name("profile.json.tmp");
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, &p)
}

/// Unix 秒 → RFC3339（Z）。无 chrono 依赖：拼装可排序的固定宽度时间戳，
/// 与 profile.py 的 `%Y-%m-%dT%H:%M:%SZ` 同形。
fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_shape_matches_python_contract() {
        // 与 profile.py::DOMAIN_RE 同源：这条断言就是跨语言契约的锁
        assert!(valid_domain("work"));
        assert!(valid_domain("side-2"));
        assert!(!valid_domain("Work"), "大写必须拒（写侧严格）");
        assert!(!valid_domain(""));
        assert!(!valid_domain("a_b"), "下划线不在白名单");
        assert!(!valid_domain(&"x".repeat(25)));
    }

    #[test]
    fn delete_only_touches_target_layer() {
        // 同名 id 在全局与域各一条：删域里那条不能动全局的
        let mut p = json!({
            "sections": [{"id": "tone", "body": "global"}],
            "notes": [],
            "domains": {"work": {"sections": [{"id": "tone", "body": "domain"}], "notes": []}}
        });
        delete_from(&mut p, "section-rm", "tone", "work").unwrap();
        assert_eq!(p["sections"][0]["body"], "global", "全局同名章节必须留着");
        assert_eq!(p["domains"]["work"]["sections"].as_array().unwrap().len(), 0);

        // 反向：删全局不动域
        let mut q = json!({
            "sections": [{"id": "tone", "body": "global"}],
            "notes": [],
            "domains": {"work": {"sections": [{"id": "tone", "body": "domain"}], "notes": []}}
        });
        delete_from(&mut q, "section-rm", "tone", "").unwrap();
        assert_eq!(q["domains"]["work"]["sections"][0]["body"], "domain");
    }

    #[test]
    fn delete_missing_is_k404_not_silent_ok() {
        let mut p = json!({"sections": [], "notes": [], "domains": {}});
        let e = delete_from(&mut p, "note-rm", "N-nope", "").unwrap_err();
        assert_eq!(e.0, 404);
        // 域不存在也是 404，不是「删了个空域却返回成功」
        let e2 = delete_from(&mut p, "note-rm", "N-x", "ghost").unwrap_err();
        assert_eq!(e2.0, 404);
    }

    #[test]
    fn now_iso_shape_is_sortable_rfc3339() {
        let s = now_iso();
        assert_eq!(s.len(), 20, "固定宽度才能字符串排序: {s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
    }
}
