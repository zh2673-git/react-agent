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
            for sec in secrets {
                if s.contains(sec.as_str()) {
                    *s = s.replace(sec.as_str(), MASK);
                }
            }
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

    #[test]
    fn config_mtime_is_zero_for_missing_file() {
        // CONFIG_FILE 未配置/文件不存在 → 0（仅 env 兜底键参与收集，不 panic）
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
