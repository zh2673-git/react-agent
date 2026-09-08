//! Web API 端点（W10 自 frontend.rs 拆分；W17 迭代七按域拆四文件）：配置中心（08 §2.2）、
//! 技能 CRUD、models/presets 转发、附件校验与 agent 段热通道（P5/E1）。
//!
//! 文件划分（纯搬家零逻辑改动，路由表 `api::` 调用路径经本文件 re-export 保持不变）：
//! - [`config`]：GET/PUT /api/config（llm / tools / agent 热通道 + mcp_servers 持久通道）
//! - [`skills`]：技能 CRUD（/api/skills）与 frontmatter 校验 / origin 打标
//! - [`misc`]：session-exists / presets / models / reveal / 多实例端点（W17）
//! 本文件保留共享件：R3 附件校验（`/api/chat` 闸）。
mod config;
mod misc;
mod skills;

pub(super) use config::{get_config, put_config};
pub(super) use misc::{
    create_instance, delete_instance, get_models, get_presets, list_instances, pick_folder,
    reveal_target, session_exists,
};
pub(super) use skills::{delete_skill, get_skill, get_skills, put_skill};

use super::bad_request;
use serde_json::{json, Value};

/// R3 附件上限（系统边界校验）：条数与单体 base64 体量。
/// 2MB 原始 ≈ 2.8MB base64 字符；图片直接进 LLM，文本文件由 agent-loop 截断后拼入。
const ATTACH_MAX_COUNT: usize = 4;
const ATTACH_MAX_B64_CHARS: usize = 2_800_000;

/// R3 附件校验：`[{name,mime,data_b64}]` 形状 + 条数/体量上限。
/// 返回 `Ok(None)` = 未携带/空数组（不带 attachments 键透传）；
/// `Ok(Some(list))` = 规整后的附件数组；`Err` = 确定性 K400 payload。
/// 注意：200 字符上限只约束 name/mime 元数据字段；data_b64 走 2MB 体量闸
/// （b64 ≤ `ATTACH_MAX_B64_CHARS`），两者不可混用。
pub(super) fn validate_attachments(v: &Value) -> Result<Option<Value>, Value> {
    let Some(arr) = v.as_array() else {
        return Err(bad_request("attachments 必须是数组", Some("attachments")));
    };
    if arr.is_empty() {
        return Ok(None);
    }
    if arr.len() > ATTACH_MAX_COUNT {
        return Err(bad_request(
            format!("附件最多 {} 个", ATTACH_MAX_COUNT),
            Some("attachments"),
        ));
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(bad_request(format!("attachments[{i}] 必须是对象"), Some("attachments")));
        };
        // 元数据字段：非空字符串 ≤200 字符
        let mut meta = Vec::with_capacity(2);
        for key in ["name", "mime"] {
            match obj.get(key).and_then(Value::as_str).map(str::trim) {
                Some(s) if !s.is_empty() && s.len() <= 200 => meta.push(s.to_string()),
                Some(_) => {
                    return Err(bad_request(
                        format!("attachments[{i}].{key} 过长（>200 字符）"),
                        Some("attachments"),
                    ))
                }
                None => {
                    return Err(bad_request(
                        format!("attachments[{i}].{key} 缺失或非空字符串"),
                        Some("attachments"),
                    ))
                }
            }
        }
        // 数据字段：非空，≤ 2MB 体量闸（b64 字符数）
        let data = obj.get("data_b64").and_then(Value::as_str).unwrap_or("").trim();
        if data.is_empty() {
            return Err(bad_request(
                format!("attachments[{i}].data_b64 缺失或非空字符串"),
                Some("attachments"),
            ));
        }
        if data.len() > ATTACH_MAX_B64_CHARS {
            return Err(bad_request(
                format!("attachments[{i}] 超过 2MB 上限"),
                Some("attachments"),
            ));
        }
        out.push(json!({"name": meta[0], "mime": meta[1], "data_b64": data}));
    }
    Ok(Some(Value::Array(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_attachments_shapes_and_limits() {
        // R3：未携带（非数组）→ K400；空数组 → Ok(None)（不透传 attachments 键）
        assert!(validate_attachments(&serde_json::json!("x")).is_err());
        assert!(validate_attachments(&serde_json::json!([])).unwrap().is_none());

        // 合法单附件 → 规整透传
        let ok = serde_json::json!([{"name": "a.png", "mime": "image/png", "data_b64": "QUJD"}]);
        let v = validate_attachments(&ok).unwrap().expect("some");
        assert_eq!(v[0]["name"], "a.png");

        // data_b64 不受 200 字符元数据上限约束：200 字符 ~ 2.8M 之间合法（真实文件体量）
        let mid = serde_json::json!([{"name": "a.txt", "mime": "text/plain", "data_b64": "Q".repeat(300_000)}]);
        assert!(validate_attachments(&mid).is_ok(), "中等体量附件必须合法");

        // 形状错误：元素非对象 / 缺字段 / 空串
        assert!(validate_attachments(&serde_json::json!(["x"])).is_err());
        assert!(validate_attachments(&serde_json::json!([{"name": "a", "mime": "image/png"}])).is_err());
        assert!(validate_attachments(
            &serde_json::json!([{"name": "a", "mime": "image/png", "data_b64": ""}])
        )
        .is_err());

        // 条数超限（> 4）
        let item = serde_json::json!({"name": "a", "mime": "text/plain", "data_b64": "x"});
        assert!(validate_attachments(&serde_json::Value::Array(vec![item; 5])).is_err());

        // 体量超限（b64 > 2_800_000 字符）
        let big = serde_json::json!([{"name": "a", "mime": "text/plain", "data_b64": "A".repeat(2_800_001)}]);
        assert!(validate_attachments(&big).is_err());
    }
}
