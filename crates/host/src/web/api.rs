//! Web API 端点（W10 自 frontend.rs 拆分）：配置中心（08 §2.2）、技能 CRUD、
//! models/presets 转发、附件校验与 agent 段热通道（P5/E1）。
use super::files::tools_dir;
use super::{bad_request, dispatch_or_err, json_resp, parse_query};
use crate::config;
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use tokio::net::TcpStream;

/// GET /api/presets：转发 llm-adapter `presets.list`（OpenAI 兼容站点预设清单，
/// 数据源 plugins/llm_adapter/presets.py——切换站点 = configure 热应用，零重启）。
pub(super) async fn get_presets(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
    match dispatch_or_err(kernel, "llm-adapter", json!({"op": "presets.list"})).await {
        Ok(v) => {
            let presets = v.get("presets").cloned().unwrap_or_else(|| json!([]));
            json_resp(stream, 200, json!({ "ok": true, "presets": presets })).await
        }
        Err(e) => json_resp(stream, 502, e).await,
    }
}

/// GET /api/models：转发 llm-adapter `models.list`，返回该 provider 当前可用模型 id 列表
/// （openai/deepseek 走 `/v1/models`；ollama 走 `/api/tags` + 逐模型 `/api/show` 探测原生窗口；
/// anthropic/mock 走静态清单）。models_meta 为可选扩展（原生窗口 ctx_limit），存在才透传。
pub(super) async fn get_models(stream: &mut TcpStream, kernel: &Kernel, query: &str) -> anyhow::Result<()> {
    // 前端可经 ?provider= 覆盖（未保存设置时也能拉对应 provider 的模型清单）
    let provider = parse_query(query).get("provider").cloned().unwrap_or_default();
    let mut op = json!({"op": "models.list"});
    if !provider.is_empty() {
        op["provider"] = json!(provider);
    }
    match dispatch_or_err(kernel, "llm-adapter", op).await {
        Ok(v) => {
            let models = v.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut out = json!({ "ok": true, "models": models });
            if let Some(meta) = v.get("models_meta") {
                out["models_meta"] = meta.clone();
            }
            json_resp(stream, 200, out).await
        }
        Err(e) => json_resp(stream, 502, e).await,
    }
}

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
        "retry_attempts": env_num("LLM_RETRY_ATTEMPTS", 2),
        "retry_base_ms": env_num("LLM_RETRY_BASE_MS", 500),
        "llm_context_tokens": env_num("LLM_CONTEXT_TOKENS", 0),
    })
}

/// GET /api/config：llm 视图（config.json > env 缺省；key 只回 key_set + 尾 4 位，绝不回明文）
/// + tools 全集视图（list all=true，含未启用项，各项附 enabled）+ skills 计数。
pub(super) async fn get_config(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
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
            },
        }),
    )
    .await
}

/// PUT /api/config：llm → llm-adapter configure（env 热应用）；tools → tools configure（白名单替换）；
/// agent → env 热应用（agent-loop InProcess 自读，下轮对话生效）。全成后 merge 落 config.json
/// （重启由持久通道还原）；任一失败 → 字段级 400 且不落盘（重启即回滚）。
pub(super) async fn put_config(stream: &mut TcpStream, kernel: &Kernel, body: &[u8]) -> anyhow::Result<()> {
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        return json_resp(stream, 400, bad_request("body 非法 JSON", None)).await;
    };
    let llm = req.get("llm").filter(|v| v.is_object());
    let tools = req.get("tools").filter(|v| v.is_object());
    let agent = req.get("agent").filter(|v| v.is_object());
    // §八 MCP：仅持久通道（server 子进程生命周期归 tools 插件 init/destroy，无热通道）。
    // PUT 落盘 config.json mcp_servers，改后需重启 host 生效。
    let mcp_servers = req.get("mcp_servers").filter(|v| v.is_object());
    if llm.is_none() && tools.is_none() && agent.is_none() && mcp_servers.is_none() {
        return json_resp(
            stream,
            400,
            bad_request("body 需含 llm / tools / agent / mcp_servers 对象", None),
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

/// GET /api/skills：转发 assets skills.list（每次重扫，Web 端即见最新目录）。
/// R9/W6：技能条目合入「配套工具」视图（tools.skill_tools all=true 按 skill 分组，含未启用
/// 项 + enabled 标记）——技能 tab 就地启停的数据源；tools 不可用 → 静默省略（软依赖不阻塞）。
pub(super) async fn get_skills(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
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
pub(super) async fn get_skill(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
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
fn valid_skill_name(name: &str) -> bool {
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
pub(super) async fn put_skill(stream: &mut TcpStream, name: &str, body: &[u8]) -> anyhow::Result<()> {
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
pub(super) async fn delete_skill(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
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

/// POST /api/reveal?target=config|tools|skills|skill&name=<name>：在系统文件管理器中
/// 打开配置源文件所在位置（浏览器 http 页面无法跳 file:// 链接，由 host 代为打开）。
/// 只允许白名单目标——config.json / plugins/tools / skills 根 / 具名技能目录
/// （名字约束杜绝路径注入）。explorer 退出码无意义，spawn 后不等待。
pub(super) async fn reveal_target(stream: &mut TcpStream, query: &str) -> anyhow::Result<()> {
    let q = parse_query(query);
    let target = q.get("target").map(String::as_str).unwrap_or("");
    let name = q.get("name").map(String::as_str).unwrap_or("");
    let (path, select) = match target {
        "config" => (config::config_file(), true),
        "tools" => (tools_dir(), false),
        "skills" => (config::skills_dir(), false),
        "skill" => {
            if !valid_skill_name(name) {
                return json_resp(
                    stream,
                    400,
                    bad_request("非法技能名（仅字母数字/_/-，≤64 字符）", Some("name")),
                )
                .await;
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
            (dir.join("SKILL.md"), true)
        }
        _ => {
            return json_resp(
                stream,
                400,
                bad_request("target 须为 config | tools | skills | skill（后者附 name）", Some("target")),
            )
            .await
        }
    };
    if !path.exists() {
        return json_resp(
            stream,
            404,
            json!({"ok": false, "error": {"code": "K404", "message": format!("路径不存在: {}", path.display())}}),
        )
        .await;
    }
    let spawned: std::io::Result<()> = if cfg!(target_os = "windows") {
        #[cfg(target_os = "windows")]
        {
            // /select,"<file>" = 打开所在文件夹并选中文件（raw_arg 保引号原样传递，
            // 规避含空格路径被二次引号包裹的 explorer 解析怪癖）；目录直接打开
            use std::os::windows::process::CommandExt;
            let arg = if select {
                format!("/select,\"{}\"", path.display())
            } else {
                format!("\"{}\"", path.display())
            };
            std::process::Command::new("explorer").raw_arg(arg).spawn().map(|_| ())
        }
        #[cfg(not(target_os = "windows"))]
        {
            unreachable!()
        }
    } else {
        let _ = select;
        let prog = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(prog).arg(&path).spawn().map(|_| ())
    };
    match spawned {
        Ok(()) => json_resp(stream, 200, json!({"ok": true, "path": path.display().to_string()})).await,
        Err(e) => json_resp(
            stream,
            500,
            json!({"ok": false, "error": {"code": "K500", "message": format!("打开失败: {e}")}}),
        )
        .await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_applies_to_env_and_validates_numbers() {
        // P5/E1：agent 段热通道——合法键落 env、数值键轻校验、未知键忽略。
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
