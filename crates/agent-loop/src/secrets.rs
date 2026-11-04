//! 密钥脱敏（W18 安全边界）：工具结果与观测事件进入会话历史/trace 前的统一闸。
//!
//! 威胁模型：agent 的文件/bash 工具可读 config.json（工作区=代码根时直读，bash 可任意路径
//! 读取）甚至 `printenv`，把 api_key 明文带进对话。修法不在堵某一工具，而是在**唯一咽喉**
//! （act_exec 返回值 → 历史+事件+trace 共同源头）做值脱敏：LLM 从未见过明文，就永远
//! 吐不出明文；前端内联 diff/产物检测拿到的也是脱敏后数据。
//!
//! 密钥来源（进程级缓存 + **mtime 触发重收集**，R13）：
//! 1. CONFIG_FILE 指向的 config.json——通用规则：任意层级键名含 key/token/secret/password
//!    的字符串值（覆盖 llm.api_key、media.*.key、mcp_servers[].key 及未来新增段）；
//! 2. 直接 env 兜底：OPENAI_API_KEY / ANTHROPIC_API_KEY / MEDIA_IMAGE_KEY / MEDIA_VIDEO_KEY。
//! 短于 6 字符的值不收集（防误杀普通词）。
//!
//! 刷新语义（修复"清单进程期固定"缺口）：每次访问核对 CONFIG_FILE 的 mtime——
//! Web 配置中心保存（merge 落盘）后 mtime 变化 → 下一次工具调用前自动重收集
//! （含重新读取 env 兜底键）。延迟上界 = 一次工具调用；热变更的密钥不再漏脱敏。
//! 历史 trace 不追溯清洗的边界不变。

use serde_json::Value;
use std::sync::{Arc, RwLock};

/// 脱敏占位符（肉眼可辨 + 不像任何合法值）。
const MASK: &str = "«KEY_MASKED»";

/// (收集时的 CONFIG_FILE mtime 毫秒, 清单)。mtime 变化 → 惰性重收集。
static SECRETS: RwLock<Option<(u64, Arc<Vec<String>>)>> = RwLock::new(None);

/// CONFIG_FILE 的 mtime（毫秒）；文件缺失/未配置 → 0（仅 env 兜底键参与收集）。
fn config_mtime() -> u64 {
    std::env::var("CONFIG_FILE")
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 进程级密钥清单（mtime 未变走读锁快路径；变化则写锁重收集，双重检查防并发重复收集）。
fn secrets() -> Arc<Vec<String>> {
    let mtime = config_mtime();
    if let Some((at, list)) = SECRETS.read().unwrap().as_ref() {
        if *at == mtime {
            return list.clone();
        }
    }
    let mut w = SECRETS.write().unwrap();
    if let Some((at, list)) = w.as_ref() {
        if *at == mtime {
            return list.clone();
        }
    }
    let fresh = Arc::new(collect());
    tracing::debug!(target: crate::ID, "secrets refreshed: {} keys (config mtime {mtime})", fresh.len());
    *w = Some((mtime, fresh.clone()));
    fresh
}

fn collect() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let t = s.trim();
        if t.chars().count() >= 6 && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    };
    for name in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "MEDIA_IMAGE_KEY", "MEDIA_VIDEO_KEY"] {
        if let Ok(v) = std::env::var(name) {
            push(&v);
        }
    }
    if let Ok(path) = std::env::var("CONFIG_FILE") {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(cfg) = serde_json::from_str::<Value>(&text) {
                walk_collect(&cfg, &mut push);
            }
        }
    }
    out
}

/// 递归收集：键名含敏感词（key/token/secret/password，忽略大小写）的字符串值。
fn walk_collect(v: &Value, push: &mut dyn FnMut(&str)) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                let kl = k.to_ascii_lowercase();
                if kl.contains("key") || kl.contains("token") || kl.contains("secret") || kl.contains("password") {
                    if let Some(s) = val.as_str() {
                        push(s);
                    }
                }
                walk_collect(val, push);
            }
        }
        Value::Array(a) => {
            for x in a {
                walk_collect(x, push);
            }
        }
        _ => {}
    }
}

/// 值脱敏：把每个密钥的所有出现替换为 MASK。就地修改，遍历 Object/Array。
pub fn scrub_value(v: &mut Value) {
    let list = secrets();
    if list.is_empty() {
        return;
    }
    scrub_with(v, &list);
}

/// 核心实现（可注入清单，测试用）：递归替换所有字符串值中的密钥出现。
pub fn scrub_with(v: &mut Value, secrets: &[String]) {
    if secrets.is_empty() {
        return;
    }
    match v {
        Value::String(s) => {
            *s = scrub_str(s, secrets);
        }
        Value::Object(m) => {
            for val in m.values_mut() {
                scrub_with(val, secrets);
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                scrub_with(x, secrets);
            }
        }
        _ => {}
    }
}

/// 单串脱敏：**保留路径结构**，只对每个路径分量做密钥替换。
///
/// 背景（D6/C3）：路径是工具行动的依据。若把密钥替换进整串，含密钥的目录名会被抹成
/// `«KEY_MASKED»/`，随后 list_dir 报 `NOT_FOUND`——模型拿到一个不可行动的名字，
/// 既无法定位真目录，也无法判断这是「脱敏」还是「真不存在」。
/// 分量级替换让路径骨架（分隔符 + 未命中分量）完整保留，密钥所在分量整体抹掉，
/// 脱敏强度不降（密钥字符一个不漏），但错误消息与路径仍可辨认。
fn scrub_str(s: &str, secrets: &[String]) -> String {
    let hit = secrets.iter().any(|sec| s.contains(sec.as_str()));
    if !hit {
        return s.to_string();
    }
    let is_pathish = looks_like_path(s);
    if !is_pathish {
        // 非路径串：整体替换（保持原有语义，如命令输出里的密钥）
        let mut out = s.to_string();
        for sec in secrets {
            out = out.replace(sec.as_str(), MASK);
        }
        return out;
    }
    // 路径串：按分隔符切分，逐分量替换后原样拼回（分隔符与空分量保留）
    let mut out = String::with_capacity(s.len());
    let mut seg = String::new();
    for ch in s.chars() {
        if ch == '/' || ch == '\\' {
            out.push_str(&scrub_segment(&seg, secrets));
            out.push(ch);
            seg.clear();
        } else {
            seg.push(ch);
        }
    }
    out.push_str(&scrub_segment(&seg, secrets));
    out
}

/// 是否按路径处理（分量级替换）。**必须排除 URL**：URL 里 `/` 是分隔符但语义不是
/// 路径分量（`https://x?key=SECRET` 整体替换才不破坏语义）。判据：
/// - 含 `://` → URL，走整体替换；
/// - 含 `/` 或 `\` 且**不含空白**（含空白的多是句子/日志，整体替换更自然）→ 路径。
fn looks_like_path(s: &str) -> bool {
    if s.contains("://") {
        return false;
    }
    let has_sep = s.contains('/') || s.contains('\\');
    has_sep && !s.chars().any(char::is_whitespace)
}

/// 单路径分量脱敏：命中则整个分量抹掉（不留残片，避免拼出可猜的部分明文）。
fn scrub_segment(seg: &str, secrets: &[String]) -> String {
    if seg.is_empty() {
        return String::new();
    }
    if secrets.iter().any(|sec| seg.contains(sec.as_str())) {
        return MASK.to_string();
    }
    seg.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrub_replaces_all_occurrences_nested() {
        let mut v = json!({
            "ok": true,
            "result": {"content": "api_key: ms-abcd1234\nurl: https://x?key=ms-abcd1234"},
            "list": ["plain", "token sk-xyz98765 here"]
        });
        // 直接喂清单（不经 env/config 收集，测试确定性）
        let secrets = vec!["ms-abcd1234".to_string(), "sk-xyz98765".to_string()];
        scrub_with(&mut v, &secrets);
        let s = v.to_string();
        assert!(!s.contains("ms-abcd1234") && !s.contains("sk-xyz98765"));
        assert!(s.contains(MASK));
        assert!(s.contains("plain") && s.contains("https://x?key="));
    }

    #[test]
    fn scrub_noop_when_no_secrets() {
        let mut v = json!({"a": "hello world"});
        scrub_with(&mut v, &[]);
        assert_eq!(v, json!({"a": "hello world"}));
    }

    /// C3/D6：路径串必须保留骨架——含密钥的分量被抹，其余分量与分隔符原样保留。
    /// 反例（旧行为）：密钥 "magpie" 命中整串 → 整个路径变 `«KEY_MASKED»`，工具报
    /// NOT_FOUND，模型无法分辨「被脱敏」还是「真不存在」。
    #[test]
    fn scrub_path_keeps_structure_and_masks_only_hit_segment() {
        let secrets = vec!["magpie".to_string()];
        let mut v = json!({
            "error": {"code": "NOT_FOUND", "message": "目录不存在: '_repo\\magpie\\sub'"}
        });
        scrub_with(&mut v, &secrets);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(!msg.contains("magpie"), "密钥不得出现在输出: {msg}");
        assert!(msg.contains(MASK), "命中分量应被抹: {msg}");
        // 骨架保留：前缀目录与子目录仍在，模型可据此行动
        assert!(msg.contains("_repo"), "未命中的前缀必须保留: {msg}");
        assert!(msg.contains("sub"), "未命中的子目录必须保留: {msg}");
        assert!(msg.contains('\\'), "分隔符必须保留: {msg}");
    }

    /// 非路径串（命令输出/URL）仍整体替换，脱敏强度不因路径优化而下降。
    #[test]
    fn scrub_non_path_string_still_replaces_inline() {
        let secrets = vec!["magpie".to_string()];
        let mut v = json!({"out": "downloading magpie-v1.2.zip now"});
        scrub_with(&mut v, &secrets);
        let s = v["out"].as_str().unwrap();
        assert!(!s.contains("magpie"), "内联密钥必须被抹: {s}");
        assert!(s.contains(MASK));
        assert!(s.contains("downloading") && s.contains("now"), "非密钥文本保留: {s}");
    }

    #[test]
    fn collect_skips_short_values_and_dedupes() {
        let mut out: Vec<String> = Vec::new();
        let mut push = |s: &str| {
            let t = s.trim();
            if t.chars().count() >= 6 && !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        };
        push("abc"); // 太短不收
        push("long-key-1");
        push("long-key-1"); // 去重
        push("  long-key-2  "); // trim
        assert_eq!(out, vec!["long-key-1".to_string(), "long-key-2".to_string()]);
    }

    /// CONFIG_FILE 未配置 / 指向不存在的路径 → 0（仅 env 兜底键参与收集，不 panic）。
    ///
    /// 必须显式指定一个**不存在**的路径：宿主任一实例运行时都会把 CONFIG_FILE 指到真实
    /// config.json，此时 mtime 必然非 0——旧写法断言裸 `config_mtime()` 等于 0，
    /// 结果「宿主在跑就红、不在跑就绿」，是典型的环境依赖型假红。
    #[test]
    fn config_mtime_is_zero_for_missing_file() {
        let missing = std::env::temp_dir().join("rsi-agent-no-such-config-9f2c.json");
        assert!(!missing.exists(), "前置：临时路径不应存在 {missing:?}");
        std::env::set_var("CONFIG_FILE", &missing);
        assert_eq!(config_mtime(), 0);
        std::env::remove_var("CONFIG_FILE");
        assert_eq!(config_mtime(), 0);
    }

    #[test]
    fn walk_collect_finds_sensitive_key_names() {
        let cfg = json!({
            "llm": {"api_key": "AKIA-qwerty12", "model": "m1"},
            "media": {"image": {"key": "ik-99887766", "base_url": "https://x"}},
            "mcp_servers": {"servers": [{"name": "s1", "token": "tok-abcdef99"}]},
            "note": {"keyword": "not-a-secret"} // 键名含 key 但值是普通串——仍会被收集，
                                                // 但只有真实出现在工具输出里才被替换，无害
        });
        let mut out: Vec<String> = Vec::new();
        let mut push = |s: &str| out.push(s.to_string());
        walk_collect(&cfg, &mut push);
        assert!(out.contains(&"AKIA-qwerty12".to_string()));
        assert!(out.contains(&"ik-99887766".to_string()));
        assert!(out.contains(&"tok-abcdef99".to_string()));
        assert!(!out.contains(&"m1".to_string()));
    }
}
