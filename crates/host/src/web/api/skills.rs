//! 技能 CRUD（/api/skills）：GET 列表（含配套工具视图）/ GET 单个 / PUT 写入（frontmatter
//! 校验 + origin 打标）/ DELETE（出厂件拒删）。（W17 迭代七自 api.rs 按域拆出，纯搬家零逻辑改动。）
use super::super::{bad_request, dispatch_or_err, json_resp};
use crate::config;
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use tokio::net::TcpStream;

/// GET /api/skills：转发 assets skills.list（每次重扫，Web 端即见最新目录）。
/// R9/W6：技能条目合入「配套工具」视图（tools.skill_tools all=true 按 skill 分组，含未启用
/// 项 + enabled 标记）——技能 tab 就地启停的数据源；tools 不可用 → 静默省略（软依赖不阻塞）。
pub(crate) async fn get_skills(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
    let mut v = match dispatch_or_err(kernel, "assets", json!({"op": "skills.list"})).await {
        Ok(v) => v,
        Err(e) => return json_resp(stream, 503, e).await,
    };
    if let Some(skills) = v.get_mut("skills").and_then(Value::as_array_mut) {
        let by_skill: std::collections::HashMap<String, Vec<Value>> =
            match dispatch_or_err(kernel, "tools", json!({"op": "skill_tools", "all": true})).await {
                Ok(t) => t
                    .get("tools")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|mut tool| {
                        let sk = tool.get("skill").and_then(Value::as_str)?.to_string();
                        if let Some(o) = tool.as_object_mut() {
                            o.remove("parameters"); // 清单视图不需要 schema
                        }
                        Some((sk, tool))
                    })
                    .fold(std::collections::HashMap::new(), |mut m, (sk, tool)| {
                        m.entry(sk).or_insert_with(Vec::new).push(tool);
                        m
                    }),
                Err(_) => std::collections::HashMap::new(),
            };
        for s in skills.iter_mut() {
            if let Some(name) = s.get("name").and_then(Value::as_str).map(str::to_string) {
                if let Some(tools) = by_skill.get(&name) {
                    if let Some(obj) = s.as_object_mut() {
                        obj.insert("tools_detail".into(), json!(tools));
                    }
                }
            }
        }
    }
    json_resp(stream, 200, v).await
}

/// GET /api/skills/{name}：读回 SKILL.md 原文（技能编辑）。直读文件（与 put/delete 同源，绕过 assets）。
pub(crate) async fn get_skill(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
    if !valid_skill_name(name) {
        return json_resp(stream, 400, bad_request("非法技能名（仅字母数字/_/-，≤64 字符）", Some("name"))).await;
    }
    let skill_md = config::skills_dir().join(name).join("SKILL.md");
    match std::fs::read_to_string(&skill_md) {
        Ok(content) => json_resp(stream, 200, json!({"ok": true, "name": name, "content": content})).await,
        Err(_) if !config::skills_dir().join(name).is_dir() => json_resp(
            stream,
            404,
            json!({"ok": false, "error": {"code": "K404", "message": format!("技能不存在: {name}")}}),
        )
        .await,
        Err(e) => json_resp(
            stream,
            500,
            json!({"ok": false, "error": {"code": "K500", "message": format!("读取失败: {e}")}}),
        )
        .await,
    }
}

/// 技能名约束：字母数字/下划线/连字符（同时杜绝路径注入——不含分隔符即无法越出 skills 根）。
pub(super) fn valid_skill_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 来源判定：SKILL.md frontmatter 显式 `origin: preset` → 出厂件；缺省/其他 → 用户件
/// （出厂件必须显式声明——预置技能归 git 管理并打标，存量用户技能缺字段即正确放开）。
/// 行式解析，与 assets guest 同语义（不引 YAML）。
fn skill_origin(content: &str) -> &'static str {
    let Some(rest) = content.strip_prefix("---") else {
        return "user";
    };
    for line in rest.lines() {
        let t = line.trim();
        if t == "---" {
            break;
        }
        if let Some(v) = t.strip_prefix("origin:") {
            return if v.trim() == "preset" { "preset" } else { "user" };
        }
    }
    "user"
}

/// Phase A：frontmatter 注入 `origin: user`（Web CRUD 写入自动打标，缺省视为出厂件）。
/// 在闭合 `---` 前插入一行；已有 origin 行则替换其值（幂等）。frontmatter 无闭合围栏时
/// 原样返回（写入前另有校验拦截）。
fn tag_origin_user(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return content.to_string();
    }
    // 闭合围栏 = 首行之后第一个 `---` 行；其后全部视为正文
    let Some(close) = lines.iter().skip(1).position(|l| l.trim() == "---").map(|i| i + 1) else {
        return content.to_string();
    };
    let mut head: Vec<String> = lines[1..close].iter().map(|l| l.to_string()).collect();
    match head.iter().position(|l| l.trim_start().starts_with("origin:")) {
        Some(i) => head[i] = "origin: user".into(), // 替换旧 origin 行
        None => head.push("origin: user".into()),
    }
    let mut out = String::from("---");
    for l in &head {
        out.push('\n');
        out.push_str(l);
    }
    out.push_str("\n---");
    let tail = lines[close + 1..].join("\n");
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(&tail);
    }
    if content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// frontmatter 最小校验：`---` 开头，闭合前含 `name: {name}`（与目录名一致）与非空 `description:`。
fn skill_frontmatter_ok(content: &str, name: &str) -> Result<(), String> {
    let Some(rest) = content.strip_prefix("---") else {
        return Err("SKILL.md 须以 '---' frontmatter 开头".into());
    };
    let mut has_desc = false;
    for line in rest.lines() {
        let t = line.trim();
        if t == "---" {
            break;
        }
        if let Some(v) = t.strip_prefix("name:") {
            if v.trim() != name {
                return Err(format!("frontmatter name '{}' 与目录名 '{name}' 不一致", v.trim()));
            }
        }
        if let Some(v) = t.strip_prefix("description:") {
            if !v.trim().is_empty() {
                has_desc = true;
            }
        }
    }
    // name 行存在性：由闭合前的循环保证（未找到即失败）
    let name_found = rest
        .lines()
        .map(str::trim)
        .take_while(|t| *t != "---")
        .any(|t| t.strip_prefix("name:").map(|v| v.trim() == name).unwrap_or(false));
    if !name_found {
        return Err(format!("frontmatter 缺 name: {name}"));
    }
    if !has_desc {
        return Err("frontmatter 缺非空 description".into());
    }
    Ok(())
}

/// PUT /api/skills/{name}：写 SKILL.md（文件即注册表；assets 重扫后下轮对话目录可见）。
pub(crate) async fn put_skill(stream: &mut TcpStream, name: &str, body: &[u8]) -> anyhow::Result<()> {
    if !valid_skill_name(name) {
        return json_resp(stream, 400, bad_request("非法技能名（仅字母数字/_/-，≤64 字符）", Some("name"))).await;
    }
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        return json_resp(stream, 400, bad_request("body 非法 JSON", None)).await;
    };
    let Some(content) = req.get("content").and_then(Value::as_str) else {
        return json_resp(stream, 400, bad_request("缺 content（SKILL.md 全文）", Some("content"))).await;
    };
    if let Err(e) = skill_frontmatter_ok(content, name) {
        return json_resp(stream, 400, bad_request(e, Some("content"))).await;
    }
    // Phase A：Web CRUD 写入自动打标 origin: user（用户件可删；出厂件不经验此通道不改动）
    let content = tag_origin_user(content);
    let dir = config::skills_dir().join(name);
    let skill_md = dir.join("SKILL.md");
    match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&skill_md, content)) {
        Ok(()) => json_resp(
            stream,
            200,
            json!({
                "ok": true,
                "name": name,
                "path": skill_md.to_string_lossy(),
                "note": "已写入；下轮对话技能目录自动可见（list 每次重扫）"
            }),
        )
        .await,
        Err(e) => json_resp(stream, 500, json!({"ok": false, "error": {"code": "K500", "message": format!("写入失败: {e}")}})).await,
    }
}

/// DELETE /api/skills/{name}：删除技能目录（名字约束已杜绝路径注入）。
/// 删除保护：出厂件（frontmatter 显式 `origin: preset`）拒删——出厂技能归 git
/// 管理，Web / agent 代删同一 API 同一规则；用户件（含缺省）可删。
pub(crate) async fn delete_skill(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
    if !valid_skill_name(name) {
        return json_resp(stream, 400, bad_request("非法技能名（仅字母数字/_/-，≤64 字符）", Some("name"))).await;
    }
    let dir = config::skills_dir().join(name);
    if !dir.is_dir() {
        return json_resp(
            stream,
            404,
            json!({"ok": false, "error": {"code": "K404", "message": format!("技能不存在: {name}")}}),
        )
        .await;
    }
    let origin = match std::fs::read_to_string(dir.join("SKILL.md")) {
        Ok(c) => skill_origin(&c),
        Err(_) => "preset", // 读不到 SKILL.md 无法证明是用户件，按出厂件保护（fail-closed）
    };
    if origin != "user" {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "field": "origin",
                "message": format!("技能 '{name}' 为出厂件（origin=preset），不可删除；如需定制请复制为用户技能")}}),
        )
        .await;
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => json_resp(stream, 200, json!({"ok": true, "name": name})).await,
        Err(e) => json_resp(stream, 500, json!({"ok": false, "error": {"code": "K500", "message": format!("删除失败: {e}")}})).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_origin_and_tag_user() {
        // 来源判定——显式 preset / 缺省 user / 非法值 user / 无 frontmatter user
        let user_md = "---\nname: a\ndescription: d\norigin: user\n---\nbody";
        let preset_md = "---\nname: a\ndescription: d\norigin: preset\n---\nbody";
        assert_eq!(skill_origin(user_md), "user");
        assert_eq!(skill_origin(preset_md), "preset");
        assert_eq!(skill_origin("---\nname: a\ndescription: d\n---\n"), "user");
        assert_eq!(skill_origin("no frontmatter"), "user");

        // 打标：缺 origin → 闭合围栏前插入；已有 origin → 替换；无围栏 → 原样
        let tagged = tag_origin_user(preset_md);
        assert_eq!(skill_origin(&tagged), "user");
        assert!(tagged.contains("description: d\norigin: user\n---"));
        assert!(tagged.ends_with("body"));
        let retagged = tag_origin_user(user_md);
        assert_eq!(retagged.matches("origin").count(), 1, "重复打标幂等（旧行被替换）");
        assert_eq!(tag_origin_user("no fence"), "no fence");

        // 打标后正文不被复制/丢失
        let md_with_body = "---\nname: a\ndescription: d\n---\n\n# H\n\ntext\n";
        let t = tag_origin_user(md_with_body);
        assert_eq!(t.matches("# H").count(), 1);
        assert!(t.starts_with("---\nname: a\n"));
        assert!(t.ends_with("text\n"));
    }
}
