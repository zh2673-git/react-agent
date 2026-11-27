//! GET/PUT /api/config：配置中心（08 §2.2）——llm / tools / agent 热通道 + mcp_servers
//! 持久通道（W17 迭代七自 api.rs 按域拆出，纯搬家零逻辑改动）。
//! agent 段 env 热应用（P5/E1，agent-loop InProcess 本进程自读，下轮对话即生效）；
//! 全成才 merge 落盘 config.json，任一失败 → 字段级 400 且不落盘（重启即回滚）。
use super::super::{bad_request, dispatch_or_err, json_resp};
use crate::config;
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use tokio::net::TcpStream;

/// agent 段数值键（P5/E1）：非法值（非数字）直接 400，防垃圾配置静默失效。
const AGENT_NUMERIC_KEYS: &[&str] = &[
    "max_rounds",
    "history_limit",
    "compact_trigger",
    "compact_keep",
    "tool_result_limit",
    "budget_secs",
    "token_budget",
    "retry_attempts",
    "retry_base_ms",
    "llm_context_tokens",
];

/// agent 段设置热应用（P5/E1）：逐键 set env（agent-loop InProcess 在本进程自读，
/// 下轮对话即生效；MAX_ROUNDS 每轮读取，同样热生效）。返回应用的键数。
fn apply_agent_to_env(agent: &Value) -> Result<usize, String> {
    let Some(obj) = agent.as_object() else {
        return Err("agent 需为对象".into());
    };
    let mut n = 0;
    for (k, v) in obj {
        if v.is_null() {
            continue;
        }
        if AGENT_NUMERIC_KEYS.contains(&k.as_str()) {
            let ok = v.is_number() || v.as_str().map(|s| s.trim().parse::<f64>().is_ok()).unwrap_or(false);
            if !ok {
                return Err(format!("{k} 需为数字"));
            }
        }
        let Some((_, env)) = config::AGENT_ENV_MAP.iter().find(|(k2, _)| k2 == k) else {
            continue; // 未知键忽略（前向兼容），不落 env
        };
        let s = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        std::env::set_var(env, s);
        n += 1;
    }
    Ok(n)
}

/// agent 段回显视图（P5/E1）：env 当前值（含缺省语义）。system_prompt 在面板编辑；
/// PROMPT 具名模板仍由 REPL /prompt 管理，不在此回显。
fn agent_config_view() -> Value {
    fn env_num(k: &str, d: i64) -> i64 {
        std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d)
    }
    json!({
        "max_rounds": env_num("MAX_ROUNDS", 8),
        "system_prompt": std::env::var("AGENT_SYSTEM_PROMPT").unwrap_or_default(),
        "history_limit": env_num("HISTORY_LIMIT", 0),
        "compact_trigger": env_num("COMPACT_TRIGGER", 40),
        "compact_keep": env_num("COMPACT_KEEP", 10),
        "tool_result_limit": env_num("TOOL_RESULT_LIMIT", 8000),
        "budget_secs": env_num("CHAT_BUDGET_SECS", 300),
        "token_budget": env_num("CHAT_TOKEN_BUDGET", 0),
        // 与 agent-loop 默认一致（retry_attempts=1）。
        "retry_attempts": env_num("LLM_RETRY_ATTEMPTS", 1),
        "retry_base_ms": env_num("LLM_RETRY_BASE_MS", 500),
        "llm_context_tokens": env_num("LLM_CONTEXT_TOKENS", 0),
    })
}

/// media 段单组回显（tools PLAN §九）：字段透传（config.json > env 缺省），key 掩码同
/// llm.key 规则——只回 key_set + 尾 4 位，绝不回明文。
fn media_section_view(sec: Option<&Value>, fields: &[(&str, &str)]) -> Value {
    let mut out = serde_json::Map::new();
    for (field, env) in fields {
        let v = sec
            .and_then(|s| s.get(*field))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var(*env).ok())
            .unwrap_or_default();
        if *field == "key" {
            out.insert("key".into(), key_view(&v));
        } else {
            out.insert((*field).into(), json!(v));
        }
    }
    // W19 站点默认参数：extra 为对象，不走字符串字段透传——原样回显（无掩码需求）
    if let Some(extra) = sec.and_then(|s| s.get("extra")).filter(|v| v.is_object()) {
        out.insert("extra".into(), extra.clone());
    }
    Value::Object(out)
}

/// key 掩码视图（与 llm.key / mcp env 键同规则）。
fn key_view(v: &str) -> Value {
    if v.is_empty() {
        json!({"key_set": false})
    } else {
        let tail: String = v.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        json!({"key_set": true, "key_tail": tail})
    }
}

/// media 段校验（PLAN §九，宽松）：image 组 base_url/model/key；video 组 base_url/model/
/// key/submit_url/query_url。规则：值须为字符串；base_url 非空须 http(s):// 开头；
/// submit_url/query_url 非空须 http(s):// 或 / 开头（相对路径以 base_url 为基由脚本拼接）；
/// video.extra（W19 站点默认参数）须为对象。未知键忽略（前向兼容）。合法返回 Ok(())，
/// 否则 Err(可读信息)。
fn validate_media(media: &Value) -> Result<(), String> {
    let url_ok = |s: &str, allow_relative: bool| {
        s.starts_with("http://") || s.starts_with("https://") || (allow_relative && s.starts_with('/'))
    };
    for (group, fields) in [
        ("image", vec![("base_url", false), ("model", false), ("key", false)]),
        ("video", vec![("base_url", false), ("model", false), ("key", false), ("submit_url", true), ("query_url", true)]),
    ] {
        let Some(sec) = media.get(group).filter(|v| v.is_object()) else {
            continue; // 组不传 = 不修改该组
        };
        for (field, allow_relative) in fields {
            if let Some(v) = sec.get(field) {
                if v.is_null() {
                    continue;
                }
                let Some(s) = v.as_str() else {
                    return Err(format!("media.{group}.{field} 需为字符串"));
                };
                let s = s.trim();
                if s.is_empty() {
                    continue; // 空串 = 不修改（与 llm key 留空同规则）
                }
                if (field == "base_url" || field == "submit_url" || field == "query_url")
                    && !url_ok(s, allow_relative)
                {
                    let shape = if allow_relative { "http(s):// 或 / 开头的路径" } else { "http(s):// 开头" };
                    return Err(format!("media.{group}.{field} 形态非法（需{shape}）: {s}"));
                }
            }
        }
        // W19 站点默认参数（仅 video 组有消费方）：extra 须为对象；image 组暂无消费方，
        // extra 作为未知键忽略（前向兼容，见 media_section_validates_urls_and_types 探针）
        if group == "video" {
            if let Some(extra) = sec.get("extra").filter(|v| !v.is_null()) {
                if !extra.is_object() {
                    return Err("media.video.extra 需为对象（站点默认参数 JSON，如 {\"mode\":\"text\",\"size\":\"720P\"}）".to_string());
                }
            }
        }
    }
    Ok(())
}

/// media 段回显视图（GET /api/config）：image（生图）/ video（生视频）两组。
fn media_config_view(cfg: &Value) -> Value {
    let media = cfg.get("media");
    json!({
        "image": media_section_view(
            media.and_then(|m| m.get("image")),
            &[("base_url", "MEDIA_IMAGE_BASE_URL"), ("model", "MEDIA_IMAGE_MODEL"), ("key", "MEDIA_IMAGE_KEY")],
        ),
        "video": media_section_view(
            media.and_then(|m| m.get("video")),
            &[
                ("base_url", "MEDIA_VIDEO_BASE_URL"),
                ("model", "MEDIA_VIDEO_MODEL"),
                ("key", "MEDIA_VIDEO_KEY"),
                ("submit_url", "MEDIA_VIDEO_SUBMIT_URL"),
                ("query_url", "MEDIA_VIDEO_QUERY_URL"),
            ],
        ),
    })
}

/// external_channels 段回显视图：对每个通道段里的敏感字段
/// （client_secret/token/key/api_key/password）只回 key_set + key_tail，绝不回明文；
/// 非敏感字段原样回显。这是通用外部通道扩展点：宿主不绑定任何具体 IM 平台，
/// 通道段与字段名都由消费方定义。
fn external_channels_view(cfg: &Value) -> Value {
    let mut channels = cfg.get("external_channels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    const SECRET_FIELDS: &[&str] = &["client_secret", "token", "key", "api_key", "password"];
    for section in channels.values_mut() {
        let Some(obj) = section.as_object_mut() else { continue; };
        for k in SECRET_FIELDS {
            if let Some(v) = obj.get(*k).and_then(Value::as_str) {
                let redacted = if v.is_empty() {
                    json!({"key_set": false})
                } else {
                    let tail: String = v.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
                    json!({"key_set": true, "key_tail": tail})
                };
                obj.insert((*k).to_string(), redacted);
            }
        }
    }
    json!(channels)
}

/// GET /api/config：llm 视图（config.json > env 缺省；key 只回 key_set + 尾 4 位，绝不回明文）
/// + tools 全集视图（list all=true，含未启用项，各项附 enabled）+ skills 计数。
pub(crate) async fn get_config(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
    let cfg = config::load_config();
    let llm = cfg.get("llm").cloned().unwrap_or_else(|| json!({}));
    let provider = llm
        .get("provider")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "mock".into()));
    let anthropic = provider == "anthropic";
    let model_default = match provider.as_str() {
        "openai" => "gpt-4o-mini",
        "anthropic" => "claude-3-5-sonnet-latest",
        "ollama" => "qwen2.5:7b",
        _ => "mock-1",
    };
    let model = llm
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| std::env::var("LLM_MODEL").unwrap_or_else(|_| model_default.into()));
    let (base_env, base_default, key_env) = if anthropic {
        ("ANTHROPIC_BASE_URL", "https://api.anthropic.com", "ANTHROPIC_API_KEY")
    } else {
        ("LLM_BASE_URL", "https://api.openai.com/v1", "OPENAI_API_KEY")
    };
    let base_url = llm
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| std::env::var(base_env).unwrap_or_else(|_| base_default.into()));
    let ollama_host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "localhost:11434".into());
    let key = llm
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| std::env::var(key_env).ok())
        .unwrap_or_default();
    let key_view = if key.is_empty() {
        json!({"key_set": false})
    } else {
        let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        json!({"key_set": true, "key_tail": tail})
    };

    // 工具三池聚合视图（前端按 pool 分组渲染）：内置（list all，mcp__ 前缀除外）+
    // 技能工具（skill_tools all）+ MCP（mcp_tools）；各项统一 {name,enabled,pool,...}。
    let mut tools: Vec<Value> = Vec::new();
    if let Ok(v) = dispatch_or_err(kernel, "tools", json!({"op": "list", "all": true})).await {
        if let Some(arr) = v.get("tools").and_then(Value::as_array) {
            for t in arr {
                let name = t.get("name").and_then(Value::as_str).unwrap_or("?");
                if name.starts_with("mcp__") {
                    continue; // MCP 工具走下方 mcp_tools 视图（pool/mcp_server 标注更完整）
                }
                tools.push(json!({
                    "name": name,
                    "enabled": t.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                    "pool": "builtin",
                    "description": t.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": t.get("parameters").cloned().unwrap_or(Value::Null),
                }));
            }
        }
    }
    if let Ok(v) = dispatch_or_err(kernel, "tools", json!({"op": "skill_tools", "all": true})).await {
        if let Some(arr) = v.get("tools").and_then(Value::as_array) {
            for t in arr {
                tools.push(json!({
                    "name": t.get("name").and_then(Value::as_str).unwrap_or("?"),
                    "enabled": t.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                    "pool": "skill",
                    "skill": t.get("skill").cloned().unwrap_or(Value::Null),
                    "description": t.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": t.get("parameters").cloned().unwrap_or(Value::Null),
                }));
            }
        }
    }
    // §八 MCP 区块：servers 状态（状态徽章数据源）；工具项并入上方聚合数组
    let mut mcp_servers: Value = json!([]);
    let mcp_decl = cfg.get("mcp_servers").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
    if let Ok(v) = dispatch_or_err(kernel, "tools", json!({"op": "mcp_tools"})).await {
        mcp_servers = v.get("servers").cloned().unwrap_or_else(|| json!([]));
        if let Some(arr) = v.get("tools").and_then(Value::as_array) {
            for t in arr {
                tools.push(json!({
                    "name": t.get("name").and_then(Value::as_str).unwrap_or("?"),
                    "enabled": t.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                    "pool": "mcp",
                    "mcp_server": t.get("server").cloned().unwrap_or(Value::Null),
                    "description": t.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": t.get("parameters").cloned().unwrap_or(Value::Null),
                }));
            }
        }
    }
    // config.json 声明的 mcp_servers（持久配置视图）：回显 command/cwd + env 各键的 key 状态
    // （key 只回 key_set + 尾 4 位，绝不回明文）。前端按此渲染 API key 输入框。
    let mcp_decl_view: Vec<Value> = mcp_decl
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(name, spec)| {
                    let env = spec.get("env").and_then(Value::as_object).cloned().unwrap_or_default();
                    let env_view: Vec<Value> = env
                        .iter()
                        .map(|(k, v)| {
                            let s = v.as_str().unwrap_or("");
                            if s.is_empty() {
                                json!({"key": k, "key_set": false})
                            } else {
                                let tail: String = s.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
                                json!({"key": k, "key_set": true, "key_tail": tail})
                            }
                        })
                        .collect();
                    json!({
                        "name": name,
                        "command": spec.get("command").cloned().unwrap_or(Value::Null),
                        "cwd": spec.get("cwd").cloned().unwrap_or(Value::Null),
                        "env": env_view,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // 状态合并：tools 回显的 servers 状态（ready/failed/...）与声明兜底（未启动前声明即所见）
    let mut mcp_decl_merge: Vec<Value> = mcp_decl_view;
    if let Ok(v) = dispatch_or_err(kernel, "tools", json!({"op": "mcp_tools"})).await {
        if let Some(arr) = v.get("servers").and_then(Value::as_array) {
            for (_i, decl) in mcp_decl_merge.iter_mut().enumerate() {
                let name = decl.get("name").and_then(Value::as_str).unwrap_or("");
                if let Some(st) = arr.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(name)) {
                    decl["status"] = st.get("status").cloned().unwrap_or(Value::Null);
                    decl["tools"] = st.get("tools").cloned().unwrap_or(Value::Null);
                    if let Some(err) = st.get("error") {
                        decl["error"] = err.clone();
                    }
                }
            }
        }
    }
    let skills_count = match dispatch_or_err(kernel, "assets", json!({"op": "skills.list"})).await {
        Ok(v) => v.get("skills").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
        Err(_) => 0,
    };
    json_resp(
        stream,
        200,
        json!({
            "ok": true,
            "config": {
                "llm": {"provider": provider, "model": model, "base_url": base_url, "ollama_host": ollama_host, "key": key_view},
                "tools": tools,
                "mcp": {"servers": mcp_servers, "declared": mcp_decl_merge},
                "skills_count": skills_count,
                "agent": agent_config_view(),
                "media": media_config_view(&cfg),
                "external_channels": external_channels_view(&cfg),
                "instance_name": std::env::var("REACT_INSTANCE_NAME").unwrap_or_default(),
            },
        }),
    )
    .await
}

/// PUT /api/config：llm → llm-adapter configure（env 热应用）；tools → tools configure（白名单替换）；
/// agent → env 热应用（agent-loop InProcess 自读，下轮对话生效）；media → 校验 + 落盘
/// （持久通道：guest spawn 时 env 固化，改后需重启 host）。全成后 merge 落 config.json
/// （重启由持久通道还原）；任一失败 → 字段级 400 且不落盘（重启即回滚）。
pub(crate) async fn put_config(stream: &mut TcpStream, kernel: &Kernel, body: &[u8]) -> anyhow::Result<()> {
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        return json_resp(stream, 400, bad_request("body 非法 JSON", None)).await;
    };
    let llm = req.get("llm").filter(|v| v.is_object());
    let tools = req.get("tools").filter(|v| v.is_object());
    let agent = req.get("agent").filter(|v| v.is_object());
    let media = req.get("media").filter(|v| v.is_object());
    // §八 MCP：仅持久通道（server 子进程生命周期归 tools 插件 init/destroy，无热通道）。
    // PUT 落盘 config.json mcp_servers，改后需重启 host 生效。
    let mcp_servers = req.get("mcp_servers").filter(|v| v.is_object());
    // external_channels：通用外部通道配置段（IM 网关等消费方使用），只落盘不热应用。
    let external_channels = req.get("external_channels").filter(|v| v.is_object());
    if llm.is_none() && tools.is_none() && agent.is_none() && mcp_servers.is_none() && media.is_none() && external_channels.is_none() {
        return json_resp(
            stream,
            400,
            bad_request("body 需含 llm / tools / agent / mcp_servers / media / external_channels 对象", None),
        )
        .await;
    }
    if let Some(llm) = llm {
        let mut payload = json!({"op": "configure"});
        for k in ["provider", "model", "base_url", "api_key"] {
            if let Some(v) = llm.get(k).filter(|v| !v.is_null()) {
                payload[k] = v.clone();
            }
        }
        if let Err(e) = dispatch_or_err(kernel, "llm-adapter", payload).await {
            return json_resp(stream, 400, e).await;
        }
    }
    if let Some(tools) = tools {
        let Some(enabled) = tools.get("enabled").cloned() else {
            return json_resp(stream, 400, bad_request("tools 缺 enabled 数组", Some("enabled"))).await;
        };
        if let Err(e) = dispatch_or_err(kernel, "tools", json!({"op": "configure", "enabled": enabled})).await {
            return json_resp(stream, 400, e).await;
        }
    }
    // agent 段（P5/E1）：数值轻校验 → env 热应用（失败即 400 不落盘）
    if let Some(agent) = agent {
        if let Err(msg) = apply_agent_to_env(agent) {
            return json_resp(stream, 400, bad_request(msg, Some("agent"))).await;
        }
    }
    // 全成 → merge 落盘（llm 逐字段合并保留未改字段如 api_key；tools.enabled 整体替换）
    let mut cfg = {
        let mut c = config::load_config();
        if c.is_null() {
            c = json!({});
        }
        c
    };
    if let Some(llm) = llm {
        let mut cur = cfg.get("llm").cloned().unwrap_or_else(|| json!({}));
        if let Some(obj) = llm.as_object() {
            for (k, v) in obj {
                if !v.is_null() {
                    cur[k] = v.clone();
                }
            }
        }
        cfg["llm"] = cur;
    }
    if let Some(enabled) = tools.and_then(|t| t.get("enabled")).cloned() {
        let mut cur = cfg.get("tools").cloned().unwrap_or_else(|| json!({}));
        cur["enabled"] = enabled;
        cfg["tools"] = cur;
    }
    // agent 段 merge（P5/E1）：逐字段替换（null 不覆盖），未知键原样保留（前向兼容）
    if let Some(agent) = agent.and_then(|a| a.as_object().cloned()) {
        let mut cur = cfg.get("agent").cloned().unwrap_or_else(|| json!({}));
        for (k, v) in agent {
            if !v.is_null() {
                cur[k] = v;
            }
        }
        cfg["agent"] = cur;
    }
    // media 段 merge（PLAN §九）：先宽松校验（url 形态/字符串类型），再逐字段替换
    // （null/空串不覆盖既有值——与 llm「留空不修改」同规则；清除配置请直接编辑 config.json）。
    if let Some(media) = media.and_then(|m| m.as_object().cloned()) {
        if let Err(msg) = validate_media(&json!(media)) {
            return json_resp(
                stream,
                400,
                json!({"ok": false, "error": {"code": "K400", "message": msg}}),
            )
            .await;
        }
        let mut cur = cfg.get("media").cloned().unwrap_or_else(|| json!({}));
        for (group, spec) in media {
            if let (Some(dst), Some(src)) = (cur.get_mut(&group).and_then(|v| v.as_object_mut()), spec.as_object()) {
                for (k, v) in src {
                    let empty = v.as_str().map(str::is_empty).unwrap_or(false);
                    if !v.is_null() && !empty {
                        dst.insert(k.clone(), v.clone());
                    }
                }
            } else if spec.is_object() {
                cur[group] = spec.clone();
            }
        }
        cfg["media"] = cur;
    }
    // external_channels merge：通用段只落盘；只接受已知基础类型字段，空串/null 不覆盖既有值，
    // 未知字段保持兼容（原样写入），由消费方自行解释。只回写不生效，消费方下次读取时生效。
    if let Some(external) = external_channels.as_ref().and_then(|e| e.as_object().cloned()) {
        for (section, spec) in external {
            let Some(spec_obj) = spec.as_object() else { continue; };
            let mut cur = cfg.get("external_channels")
                .and_then(|v| v.get(&section))
                .cloned()
                .unwrap_or_else(|| json!({}));
            for (k, v) in spec_obj {
                let bad = match v {
                    Value::Null => false,
                    Value::String(_) | Value::Bool(_) | Value::Number(_) => false,
                    _ => true,
                };
                if bad {
                    return json_resp(
                        stream,
                        400,
                        json!({"ok": false, "error": {"code": "K400", "message": format!("external_channels.{} 只接受 string/bool/number/null", k)}}),
                    )
                    .await;
                }
            }
            if let Some(cur_obj) = cur.as_object_mut() {
                for (k, v) in spec_obj {
                    let empty = v.as_str().map(str::is_empty).unwrap_or(false);
                    if !v.is_null() && !empty {
                        cur_obj.insert(k.clone(), v.clone());
                    }
                }
            } else {
                cur = json!(spec_obj);
            }
            if !cfg.get("external_channels").map(|v| v.is_object()).unwrap_or(false) {
                cfg["external_channels"] = json!({});
            }
            cfg["external_channels"][&section] = cur;
        }
    }
    // §八 MCP merge：按 server 名合并声明（command/cwd 未传则保留原值；env 逐键合并，
    // 前端提交空串环境变量值 = 保持已有 key 不动）。只落盘，重启由持久通道生效。
    if let Some(mcp) = mcp_servers.and_then(|m| m.as_object().cloned()) {
        let mut cur = cfg.get("mcp_servers").cloned().unwrap_or_else(|| json!({}));
        if let Some(obj) = cur.as_object_mut() {
            for (name, spec) in mcp {
                let old = obj.get(&name).cloned().unwrap_or_else(|| json!({}));
                let mut merged = old;
                if let Some(o) = merged.as_object_mut() {
                    if let Some(cmd) = spec.get("command") {
                        if !cmd.is_null() {
                            o.insert("command".into(), cmd.clone());
                        }
                    }
                    if let Some(cwd) = spec.get("cwd") {
                        o.insert("cwd".into(), cwd.clone());
                    }
                    // env 逐键合并：空串/空对象 = 不修改；有值则覆盖
                    if let Some(env_add) = spec.get("env").and_then(Value::as_object) {
                        let env_cur = o.get("env").cloned().unwrap_or_else(|| json!({}));
                        let mut env = env_cur.as_object().cloned().unwrap_or_default();
                        for (k, v) in env_add {
                            if let Some(s) = v.as_str() {
                                if !s.is_empty() {
                                    env.insert(k.clone(), json!(s));
                                }
                            } else if !v.is_null() {
                                env.insert(k.clone(), v.clone());
                            }
                        }
                        o.insert("env".into(), json!(env));
                    }
                }
                obj.insert(name, merged);
            }
        }
        cfg["mcp_servers"] = cur;
    }
    // external_channels 段 merge（外部通道通用配置）：只接受已知键，类型严格校验，
    // 任一非法 → 400 不落盘；空串/null = 不修改（与 media 同规则）。
    const IM_BOOL_KEYS: &[&str] = &["enabled", "enable_dingtalk", "enable_qq"];
    const IM_STR_KEYS: &[&str] = &["host_base", "dingtalk_client_id", "dingtalk_client_secret", "qq_appid", "qq_token", "allow_users", "allow_chats"];
    const IM_NUM_KEYS: &[&str] = &["max_reply_chars", "chat_timeout_secs"];
    if let Some(ec) = external_channels.and_then(|v| v.get("im")).filter(|v| v.is_object()) {
        for (k, v) in ec.as_object().unwrap() {
            if v.is_null() { continue; }
            if IM_BOOL_KEYS.contains(&k.as_str()) && !v.is_boolean() {
                return json_resp(stream, 400, bad_request(format!("external_channels.im.{k} 需为布尔"), Some("external_channels"))).await;
            }
            if IM_STR_KEYS.contains(&k.as_str()) && !v.is_string() {
                return json_resp(stream, 400, bad_request(format!("external_channels.im.{k} 需为字符串"), Some("external_channels"))).await;
            }
            if IM_NUM_KEYS.contains(&k.as_str()) && !v.is_number() && v.as_str().map(|s| s.trim().parse::<f64>().is_ok()).unwrap_or(false) == false {
                return json_resp(stream, 400, bad_request(format!("external_channels.im.{k} 需为数字"), Some("external_channels"))).await;
            }
        }
        let mut cur = cfg.get("external_channels").cloned().unwrap_or_else(|| json!({}));
        let mut im = cur.get("im").cloned().unwrap_or_else(|| json!({}));
        let im_obj = im.as_object_mut().unwrap();
        for (k, v) in ec.as_object().unwrap() {
            let empty = v.as_str().map(str::is_empty).unwrap_or(false);
            if !v.is_null() && !empty {
                im_obj.insert(k.clone(), v.clone());
            }
        }
        cur["im"] = im;
        cfg["external_channels"] = cur;
    }
    match config::persist_config(&cfg) {
        Ok(()) => json_resp(
            stream,
            200,
            json!({"ok": true, "persisted": config::config_file().to_string_lossy()}),
        )
        .await,
        Err(e) => json_resp(
            stream,
            500,
            json!({"ok": false, "error": {"code": "K500", "message": format!("配置已热应用但落盘失败: {e}")}}),
        )
        .await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_channels_view_redacts_secrets_only() {
        let cfg = serde_json::json!({
            "external_channels": {"im": {
                "enabled": true,
                "host_base": "http://127.0.0.1:8710",
                "client_id": "app-id",
                "client_secret": "super-secret-1234",
                "token": "tk",
                "api_key": "ak",
                "password": "pw",
                "custom_field": "plain-value"
            }}
        });
        let view = external_channels_view(&cfg);
        let im = view.get("im").unwrap();
        assert_eq!(im["enabled"], true);
        assert_eq!(im["client_id"], "app-id");
        assert_eq!(im["custom_field"], "plain-value");
        let sec = im["client_secret"].as_object().unwrap();
        assert_eq!(sec["key_set"], true);
        assert_eq!(sec["key_tail"], "1234");
        assert!(sec.get("super-secret-1234").is_none());
        assert_eq!(im["token"].as_object().unwrap()["key_tail"], "tk");
        assert_eq!(im["api_key"].as_object().unwrap()["key_tail"], "ak");
        assert_eq!(im["password"].as_object().unwrap()["key_tail"], "pw");
    }

    #[test]
    fn external_channels_view_redacts_all_sections() {
        let cfg = serde_json::json!({
            "external_channels": {
                "im": {"token": "sec-im-token"},
                "webhook": {"api_key": "sec-hook-key", "url": "https://example.test/h"}
            }
        });
        let view = external_channels_view(&cfg);
        assert_eq!(view["im"]["token"]["key_tail"], "oken");
        assert_eq!(view["webhook"]["api_key"]["key_tail"], "-key");
        assert_eq!(view["webhook"]["url"], "https://example.test/h");
    }

    #[test]
    fn external_channels_view_missing_is_empty() {
        let cfg = serde_json::json!({"llm": {}});
        let view = external_channels_view(&cfg);
        assert!(view.as_object().unwrap().is_empty());
    }

    #[test]
    fn agent_config_applies_to_env_and_validates_numbers() {
        // P5/E1：agent 段热通道——合法键落 env、数值键轻校验、未知键忽略。
        // F4：与 config.rs 的两条同类测试共享 MAX_ROUNDS / CHAT_TOKEN_BUDGET 等键
        let _guard = crate::config::env_lock();
        let keys = ["MAX_ROUNDS", "CHAT_TOKEN_BUDGET", "AGENT_SYSTEM_PROMPT", "COMPACT_TRIGGER"];
        let saved: Vec<(String, Option<String>)> = keys.iter().map(|k| (k.to_string(), std::env::var(k).ok())).collect();

        let ok = serde_json::json!({"max_rounds": 12, "token_budget": 500, "system_prompt": "P5 PROMPT", "unknown_key": 1});
        let n = apply_agent_to_env(&ok).expect("valid agent");
        assert_eq!(n, 3, "3 个已知键生效，未知键忽略: n={n}");
        assert_eq!(std::env::var("MAX_ROUNDS").unwrap(), "12");
        assert_eq!(std::env::var("CHAT_TOKEN_BUDGET").unwrap(), "500");
        assert_eq!(std::env::var("AGENT_SYSTEM_PROMPT").unwrap(), "P5 PROMPT");

        // 数值键传字符串数字 → 放行（面板可能以字符串提交）
        let s = serde_json::json!({"compact_trigger": "20"});
        assert!(apply_agent_to_env(&s).is_ok());
        assert_eq!(std::env::var("COMPACT_TRIGGER").unwrap(), "20");

        // 数值键传垃圾 → 400 语义
        let bad = serde_json::json!({"max_rounds": "abc"});
        assert!(apply_agent_to_env(&bad).is_err());

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn media_section_validates_urls_and_types() {
        // PLAN §九：media 段轻校验——url 宽松形态 + 字符串类型；未知键忽略。
        let ok = serde_json::json!({
            "image": {"base_url": "https://img.example.com/v1", "model": "img-1", "key": "k", "extra": 1},
            "video": {"base_url": "https://vid.example.com", "submit_url": "/v1/video/submit",
                       "query_url": "https://vid.example.com/v1/video/status?id={task_id}"}
        });
        assert!(validate_media(&ok).is_ok());

        // base_url 非 http(s) → 拒
        let bad_base = serde_json::json!({"image": {"base_url": "img.example.com"}});
        assert!(validate_media(&bad_base).is_err());
        // submit_url 允许相对路径（/ 开头）；裸域名拒
        assert!(validate_media(&serde_json::json!({"video": {"submit_url": "/v1/video/submit"}})).is_ok());
        assert!(validate_media(&serde_json::json!({"video": {"submit_url": "v1/video/submit"}})).is_err());
        // key 非字符串 → 拒
        let bad_key = serde_json::json!({"image": {"key": 123}});
        assert!(validate_media(&bad_key).is_err());
        // 空串/null/组缺省 = 放行
        assert!(validate_media(&serde_json::json!({"image": {"key": ""}})).is_ok());
        assert!(validate_media(&serde_json::json!({"image": {"key": null}})).is_ok());
        assert!(validate_media(&serde_json::json!({})).is_ok());
        // W19：extra 须为对象；null 视为不修改；字符串形态拒
        assert!(validate_media(&serde_json::json!({"video": {"extra": {"mode": "text", "size": "720P"}}})).is_ok());
        assert!(validate_media(&serde_json::json!({"video": {"extra": null}})).is_ok());
        assert!(validate_media(&serde_json::json!({"video": {"extra": "{\"mode\":\"text\"}"}})).is_err());
    }
}
