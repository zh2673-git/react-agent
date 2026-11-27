//! 内容分发配置（`GET|POST /api/publish/config`，I-20261108-07）。
//!
//! 为什么配置落在 `config/publish.json` 而不是 `config.json`：
//! 这里存的是**第三方平台凭证**（AppSecret）。与主配置分开有三个直接好处——
//! 可以单独设文件权限、可以单独不进备份、`config.json` 被 agent 读取时不会顺带带出
//! 公众号密钥。
//!
//! 与 `scripts/publish.py` 的契约（改一侧必须改另一侧）：
//! - 文件位置 `WORKSPACE_ROOT/config/publish.json`
//! - 形状 `{ "platforms": {<name>: {...}}, "defaults": {"account": "..."} }`
//! - `app_secret` 是**写敏感**字段：GET 只回 `secret_set:true`，POST 回空串表示不修改
//!
//! 职责边界：本模块只做「配置的读写与脱敏」，**不发布内容**。发布由
//! `scripts/publish.py send` 执行——host 不该在 HTTP 请求里等一个可能要扫码的浏览器上传。

use super::super::{bad_request, json_resp};
use crate::config;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use tokio::net::TcpStream;

/// 平台清单（与 `scripts/publish.py::build_adapters` 同源）。
/// `how` 决定就绪判据：`sau` 看命令在不在，`api` 看凭证填没填。
const PLATFORMS: &[(&str, &str, &str)] = &[
    ("xiaohongshu", "小红书", "sau"),
    ("bilibili", "B站", "sau"),
    ("tencent", "微信视频号", "sau"),
    ("douyin", "抖音", "sau"),
    ("wechat", "微信公众号", "api"),
];

fn publish_path() -> PathBuf {
    // 允许 `RSI_PUBLISH_PATH` 覆盖（测试隔离用，与 RSI_BOARD_PATH 同一约定）
    let raw = std::env::var("RSI_PUBLISH_PATH").unwrap_or_default();
    let raw = raw.trim();
    let p = if raw.is_empty() {
        PathBuf::from("config").join("publish.json")
    } else {
        PathBuf::from(raw)
    };
    if p.is_absolute() {
        p
    } else {
        PathBuf::from(config::workspace_root()).join(p)
    }
}

/// 读配置；缺文件/坏 JSON → 空配置（不炸：面板要能显示「都还没配」）。
fn load() -> Value {
    let Ok(text) = std::fs::read_to_string(publish_path()) else {
        return json!({ "platforms": {}, "defaults": { "account": "creator" } });
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({ "platforms": {}, "defaults": { "account": "creator" }, "corrupt": true }),
    }
}

/// 判断 `sau` 是否可用——决定「浏览器自动化平台」那组能不能用。
///
/// 两处探测，缺一不可：
/// 1. `PATH` 上有没有（用户自己装好了、或 run.cmd 注入过）
/// 2. 工作区内固定位置 `tools/sau/.venv/Scripts/sau.exe` 有没有
///
/// 为什么要第 2 条：host 是长驻进程，`PATH` 是它**启动那一刻**的快照。
/// 用户在 host 跑起来之后才装好 sau、往 PATH 里加目录，这个进程永远看不见——
/// 面板会一直显示「未找到 sau」，而实际上已经装好。固定位置探测填掉这个
/// 「装完必须重启才认」的坑：文件在就认为就绪。
fn sau_on_path() -> bool {
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for ext in &exts {
                if dir.join(format!("sau{ext}")).is_file() {
                    return true;
                }
            }
        }
    }
    // 工作区固定安装位置（tools/sau 由安装步骤落在这里）
    let base = PathBuf::from(config::workspace_root()).join("tools").join("sau");
    let candidates = [
        base.join(".venv").join("Scripts").join("sau.exe"),
        base.join("Scripts").join("sau.exe"),
        base.join("sau"),
    ];
    candidates.iter().any(|p| p.is_file())
}

/// 就绪判定：把「缺什么」翻译成用户能照做的下一步。
fn readiness(how: &str, cfg: &Map<String, Value>) -> (bool, String) {
    if how == "sau" {
        if !sau_on_path() {
            return (
                false,
                "未找到 sau 命令。请先安装 social-auto-upload（MIT）：\n  \
                 见 github.com/dreammis/social-auto-upload 的安装说明\n\
                 装好后终端执行 `sau --help` 应能列出平台子命令，再回来这里点一次保存。"
                    .into(),
            );
        }
        return (true, String::new());
    }
    // wechat：要 AppID + AppSecret
    let appid = cfg.get("appid").and_then(Value::as_str).unwrap_or("");
    let secret_set = cfg
        .get("app_secret")
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if appid.trim().is_empty() || !secret_set {
        return (
            false,
            "缺 AppID/AppSecret。到公众平台 → 设置与开发 → 基本配置 取 AppID 与 AppSecret 填入下面两栏。".into(),
        );
    }
    (true, String::new())
}

/// GET /api/publish/config：配置 + 各平台就绪状态。**不回明文 secret**。
pub(crate) async fn get_publish_config(stream: &mut TcpStream) -> anyhow::Result<()> {
    let cfg = load();
    let platforms = cfg.get("platforms").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut rows = Vec::new();
    for (name, label, how) in PLATFORMS.iter().copied() {
        let p = platforms.get(name).and_then(Value::as_object).cloned().unwrap_or_default();
        let (ready, reason) = readiness(how, &p);
        let mut row = json!({
            "platform": name,
            "label": label,
            "how": how,
            "enabled": p.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            "ready": ready,
            "reason": reason,
        });
        if let Some(obj) = row.as_object_mut() {
            // 只露「设过没有」，绝不回明文——前端据此显示占位文案
            obj.insert(
                "secret_set".into(),
                json!(p.get("app_secret").and_then(Value::as_str).map(|s| !s.trim().is_empty()).unwrap_or(false)),
            );
            for k in ["appid", "author"] {
                obj.insert(k.into(), p.get(k).cloned().unwrap_or(json!("")));
            }
            obj.insert(
                "auto_publish".into(),
                json!(p.get("auto_publish").and_then(Value::as_bool).unwrap_or(false)),
            );
        }
        rows.push(row);
    }
    json_resp(
        stream,
        200,
        json!({
            "ok": true,
            "platforms": rows,
            "defaults": cfg.get("defaults").cloned().unwrap_or(json!({"account": "creator"})),
            "path": publish_path().display().to_string(),
            "corrupt": cfg.get("corrupt").cloned().unwrap_or(json!(false)),
        }),
    )
    .await
}

/// POST /api/publish/config：写配置。**空 secret = 不修改**（与 llm key 同一约定）。
pub(crate) async fn post_publish_config(stream: &mut TcpStream, body: &[u8]) -> anyhow::Result<()> {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return json_resp(
                stream,
                400,
                json!({"ok": false, "error": {"message": format!("body 非法 JSON: {e}")}}),
            )
            .await
        }
    };
    let Some(incoming) = req.get("platforms").and_then(Value::as_object) else {
        return json_resp(stream, 400, bad_request("缺 platforms", Some("platforms"))).await;
    };
    let mut cfg = load();
    if cfg.get("corrupt").and_then(Value::as_bool) == Some(true) {
        // 坏配置拒绝写入：否则会按空配置覆盖掉用户手写的正确内容
        return json_resp(
            stream,
            409,
            json!({"ok": false, "error": {"message": format!(
                "分发配置无法解析，拒绝写入以免覆盖：{}（请先修正或删除该文件）", publish_path().display())}}),
        )
        .await;
    }
    let known: Vec<&str> = PLATFORMS.iter().map(|(n, _, _)| *n).collect();
    let store = cfg.as_object_mut().expect("load 保证是对象");
    let platforms = store.entry("platforms").or_insert_with(|| json!({})).as_object_mut().unwrap();
    for (name, patch) in incoming {
        if !known.contains(&name.as_str()) {
            continue; // 未知平台静默丢弃（前端版本比后端新时不炸）
        }
        let Some(patch) = patch.as_object() else { continue };
        let slot = platforms.entry(name.clone()).or_insert_with(|| json!({})).as_object_mut().unwrap();
        for (k, v) in patch {
            // 写敏感字段：空串/缺省 = 保留原值，绝不用空串把已有 secret 擦掉
            if k == "app_secret" && v.as_str().map(|s| s.trim().is_empty()).unwrap_or(true) {
                continue;
            }
            slot.insert(k.clone(), v.clone());
        }
    }
    if let Some(d) = req.get("defaults").and_then(Value::as_object) {
        let account = d.get("account").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let mut merged = store
            .get("defaults")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !account.is_empty() {
            merged.insert("account".into(), json!(account));
        }
        store.insert("defaults".into(), Value::Object(merged));
    }

    let p = publish_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".into());
    let tmp = p.with_file_name("publish.json.tmp");
    if let Err(e) = std::fs::write(&tmp, format!("{text}\n")).and_then(|_| std::fs::rename(&tmp, &p)) {
        return json_resp(stream, 500, json!({"ok": false, "error": {"message": format!("写盘失败: {e}")}})).await;
    }
    json_resp(stream, 200, json!({"ok": true, "path": p.display().to_string()})).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_list_covers_the_three_asked_for() {
        let names: Vec<&str> = PLATFORMS.iter().map(|(n, _, _)| *n).collect();
        // 用户原话点名的三处：小红书、B站、公众号
        assert!(names.contains(&"xiaohongshu"), "小红书");
        assert!(names.contains(&"bilibili"), "B站");
        assert!(names.contains(&"wechat"), "公众号");
    }

    #[test]
    fn every_platform_declares_a_route() {
        for (name, label, how) in PLATFORMS.iter().copied() {
            assert!(!name.is_empty() && !label.is_empty());
            assert!(how == "sau" || how == "api", "{name} 的 how 必须能判定");
        }
    }

    #[test]
    fn wechat_without_secret_is_not_ready() {
        let (ready, why) = readiness("api", &Map::new());
        assert!(!ready);
        assert!(why.contains("AppID"), "原因要能指导用户去填：{why}");
    }

    #[test]
    fn wechat_with_credentials_is_ready() {
        let mut m = Map::new();
        m.insert("appid".into(), json!("wx123"));
        m.insert("app_secret".into(), json!("s3cret"));
        let (ready, why) = readiness("api", &m);
        assert!(ready, "{why}");
    }

    #[test]
    fn blank_secret_counts_as_unset() {
        let mut m = Map::new();
        m.insert("appid".into(), json!("wx123"));
        m.insert("app_secret".into(), json!("   "));
        let (ready, _) = readiness("api", &m);
        assert!(!ready, "空白 secret 不能算已配置");
    }

    /// sau 的探测必须**不止看 PATH**。
    ///
    /// host 是长驻进程，PATH 是启动那一刻的快照：用户在 host 跑起来之后才装好
    /// sau，这个进程永远看不见，面板会一直显示「未找到 sau」。所以固定位置
    /// `tools/sau` 必须也在探测范围内——本测钉死这一点，防止有人把它简化回
    /// 「只查 PATH」：那个改动看起来更干净，实际会让装好的用户白白重启一次。
    #[test]
    fn sau_detect_covers_workspace_install_not_only_path() {
        let src = include_str!("publish.rs");
        let body_start = src.find("fn sau_on_path").expect("sau_on_path 必须存在");
        let tail = &src[body_start..];
        let body_end = tail[1..].find("\nfn ").map(|i| i + 1).unwrap_or(tail.len());
        let body = &tail[..body_end];
        assert!(
            body.contains("\"tools\"") && body.contains("\"sau\""),
            "sau_on_path 必须探测工作区固定位置 tools/sau：{body}"
        );
    }
}
