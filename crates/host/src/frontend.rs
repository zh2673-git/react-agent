//! 前端装配（Phase 3-2，01 §Phase3）：`trait Frontend` 两实现。
//!
//! - `ReplFrontend`：终端交互（REPL / 单轮），斜杠命令见 `repl_command`。
//! - `WebFrontend`：Cursor 暖色系「事件流式会话」（主题 token 真值见 host PLAN W1）——
//!   静态单页（web-dist/index.html 运行时 serve）+ SSE 实时渲染 + 日志重放恢复。
//!
//! host 是组合根：前端是**入口组件**而非 guest 能力——网关需调 `agent.chat` + `session.trace`，
//! 而 guest 不可互调（内核物理约束）；前端切换必伴随重启，热插拔无收益。
//! 选择：`REACT_FRONTEND=repl`（默认）/ `web`（`WEB_ADDR` 默认 127.0.0.1:8710）。
use crate::config;
use agent_kernel_sdk::{Envelope, PluginId};
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[async_trait::async_trait]
pub trait Frontend: Send + Sync {
    /// 阻塞运行前端直到退出（REPL EOF/exit；web 伺服器永不返回）。
    async fn run(&self, kernel: Arc<Kernel>, session: String) -> anyhow::Result<()>;
}

/// 按 `REACT_FRONTEND` env 装配前端（默认 repl）。
pub fn from_env() -> Box<dyn Frontend> {
    match std::env::var("REACT_FRONTEND")
        .unwrap_or_else(|_| "repl".into())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "web" => Box::new(WebFrontend::from_env()),
        _ => Box::new(ReplFrontend),
    }
}

// ───────────────────────────── ReplFrontend ─────────────────────────────

pub struct ReplFrontend;

#[async_trait::async_trait]
impl Frontend for ReplFrontend {
    async fn run(&self, kernel: Arc<Kernel>, session: String) -> anyhow::Result<()> {
        println!(
            "react-agent ready（session={session}）。输入 exit/quit 退出；/help 查看命令（/prompt、/skill 等）。"
        );
        let mut line = String::new();
        loop {
            print!("react-agent> ");
            let _ = std::io::stdout().flush();
            line.clear();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => break, // EOF
                Ok(_) => {}
            }
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            if text == "exit" || text == "quit" {
                break;
            }
            if text.starts_with('/') {
                repl_command(&kernel, text).await;
                continue;
            }
            turn(&kernel, &session, text).await;
        }
        Ok(())
    }
}

/// 单轮对话：dispatch agent.chat，打印答案与 steps 汇总（过程已由 react_progress 实时回显）。
pub async fn turn(kernel: &Kernel, session: &str, text: &str) {
    let env = Envelope::new(
        PluginId::new("agent-loop"),
        json!({"op": "chat", "session_id": session, "user_text": text}),
    );
    match kernel.dispatch(env).await {
        Ok(v) if v.get("ok") == Some(&json!(true)) => {
            if let Some(ans) = v.get("answer").and_then(Value::as_str) {
                println!("{ans}");
            }
            if let Some(steps) = v.get("steps").and_then(Value::as_array) {
                if !steps.is_empty() {
                    let summary = steps
                        .iter()
                        .map(|s| {
                            format!(
                                "r{}:{}({}ms)",
                                s.get("round").and_then(Value::as_u64).unwrap_or(0),
                                s.get("tool").and_then(Value::as_str).unwrap_or("?"),
                                s.get("ms").and_then(Value::as_u64).unwrap_or(0)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("[steps] {summary}");
                }
            }
        }
        Ok(v) => eprintln!("[agent error] {}", v.get("error").cloned().unwrap_or(v)),
        Err(e) => eprintln!("[kernel error] {e}"),
    }
}

/// REPL 斜杠命令（Phase 2-3）：/prompt、/skill。assets 为软依赖——不可用时命令报错不崩溃。
async fn repl_command(kernel: &Kernel, text: &str) {
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match (cmd, arg) {
        ("/help", "") => {
            println!(
                "命令：\n  /prompt            列出可用提示词模板\n  /prompt <name>     切换系统提示词（后续会话生效）\n  /prompt off        恢复内置缺省提示词\n  /skill             列出可用技能\n  /skill <name>      查看技能全文（SKILL.md）\n  exit | quit        退出"
            );
        }
        ("/prompt", "") => match assets(kernel, json!({"op": "prompts.list"})).await {
            Some(v) => {
                let items = v["prompts"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("（无可用提示词模板；prompts/ 目录为空或 assets 不可用）");
                } else {
                    for p in &items {
                        println!(
                            "- {}: {}",
                            p["name"].as_str().unwrap_or("?"),
                            p["description"].as_str().unwrap_or("")
                        );
                    }
                    println!("当前 PROMPT={}", std::env::var("PROMPT").unwrap_or_else(|_| "（未设置，用内置缺省）".into()));
                }
            }
            None => println!("[assets 不可用]"),
        },
        ("/prompt", "off") => {
            std::env::remove_var("PROMPT");
            println!("已恢复内置缺省提示词");
        }
        ("/prompt", name) => match assets(kernel, json!({"op": "prompts.get", "name": name})).await {
            Some(v) if v.get("ok") == Some(&json!(true)) => {
                std::env::set_var("PROMPT", name);
                println!("系统提示词已切换为 {name}（下轮对话生效）");
            }
            _ => println!("未知提示词模板: {name}（/prompt 查看列表）"),
        },
        ("/skill", "") => match assets(kernel, json!({"op": "skills.list"})).await {
            Some(v) => {
                let items = v["skills"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("（无可用技能；skills/ 目录为空或 assets 不可用）");
                } else {
                    for s in items {
                        println!(
                            "- {}: {}",
                            s["name"].as_str().unwrap_or("?"),
                            s["description"].as_str().unwrap_or("")
                        );
                    }
                    println!("（技能由模型经 load_skill 按需激活，无需手动加载）");
                }
            }
            None => println!("[assets 不可用]"),
        },
        ("/skill", name) => match assets(kernel, json!({"op": "skills.load", "name": name})).await {
            Some(v) if v.get("ok") == Some(&json!(true)) => {
                println!("{}", v["content"].as_str().unwrap_or(""));
            }
            _ => println!("未知技能: {name}（/skill 查看列表）"),
        },
        (other, _) => println!("未知命令 {other}（/help 查看命令）"),
    }
}

/// assets 软依赖调用：失败返回 None（REPL 命令降级提示，不崩溃）。
async fn assets(kernel: &Kernel, payload: Value) -> Option<Value> {
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("assets"), payload))
        .await
        .ok()?;
    if r.get("ok") == Some(&json!(true)) {
        Some(r)
    } else {
        None
    }
}

// ───────────────────────────── WebFrontend ─────────────────────────────

/// web 前端单页：运行时从 `web-dist/index.html` 读取（serve 目录模式），
/// 改样式后刷新浏览器即生效，无需重编 host 二进制。定位见 `web_index_path`。

/// dsh 风格 web 网关：极简手写 HTTP（tokio TcpListener，不引 web 框架依赖）。
///
/// 路由：
///   GET  /                      → 单页（web-dist/index.html，运行时读取，改样式刷新即生效）
///   GET  /api/events?session=&after= → SSE：增量轮询 memory trace.read，逐事件推送
///   POST /api/chat              → {"session_id","message","attachments"?} → agent.chat（阻塞到收敛）
///   POST /api/chat/cancel?session= → agent-loop cancel + llm-adapter abort（取消，立即返回）
///   POST /api/chat/rollback     → {"session_id","upto_user_index"} → memory rollback（R2 回滚）
///   GET  /api/presets           → llm-adapter presets.list（OpenAI 兼容站点预设清单）
pub struct WebFrontend {
    addr: String,
}

impl WebFrontend {
    pub fn from_env() -> Self {
        Self {
            addr: std::env::var("WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8710".into()),
        }
    }

    async fn run_inner(&self, kernel: Arc<Kernel>) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.addr).await?;
        Self::serve_listener(listener, kernel).await
    }

    /// 伺服循环（绑定与伺服分离：测试用 port 0 绑定后取 local_addr）。
    pub async fn serve_listener(listener: TcpListener, kernel: Arc<Kernel>) -> anyhow::Result<()> {
        let local = listener.local_addr()?;
        tracing::info!("web 网关就绪: http://{local}（事件流式会话，SSE 实时 + 刷新重放恢复）");
        loop {
            let (stream, _peer) = listener.accept().await?;
            let kernel = kernel.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, kernel).await {
                    tracing::debug!("web 连接处理结束: {e}");
                }
            });
        }
    }
}

#[async_trait::async_trait]
impl Frontend for WebFrontend {
    async fn run(&self, kernel: Arc<Kernel>, _session: String) -> anyhow::Result<()> {
        self.run_inner(kernel).await
    }
}

// ── HTTP 最小实现 ──

/// 定位 web 前端静态文件 `web-dist/index.html`，按优先级尝试：
/// 1. 编译期注入的源码目录（`CARGO_MANIFEST_DIR`，`cargo run`/`build` 时有效）；
/// 2. 相对 cwd 的 `crates/host/web-dist/index.html`（在 workspace 根启动）；
/// 3. 相对二进制同级的 `web-dist/index.html`（打包发布）。
fn web_index_path() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("web-dist").join("index.html"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("crates").join("host").join("web-dist").join("index.html"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("web-dist").join("index.html"));
        }
    }
    candidates.into_iter().find(|p| p.exists())
}

/// `GET /`：从 `web-dist/index.html` 读取并返回；文件缺失给出明确 500（而非 panic）。
/// 首页强制 `no-store`：前端是运行时 serve 的，任何改动刷新即生效，绝不被浏览器缓存旧 JS。
async fn serve_index(stream: &mut TcpStream) -> anyhow::Result<()> {
    match web_index_path() {
        Some(p) => match tokio::fs::read(&p).await {
            Ok(bytes) => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(head.as_bytes()).await?;
                stream.write_all(&bytes).await?;
                stream.flush().await?;
                Ok(())
            }
            Err(e) => respond(
                stream,
                500,
                "application/json; charset=utf-8",
                json!({"ok": false, "error": {"code": "K500", "message": format!("读取前端文件失败: {e}")}})
                    .to_string()
                    .as_bytes(),
            )
            .await,
        },
        None => respond(
            stream,
            500,
            "application/json; charset=utf-8",
            json!({"ok": false, "error": {"code": "K500", "message": "找不到 web-dist/index.html（应在 crates/host/web-dist/ 下；cargo run 在 workspace 根或 crate 同级即可）"}})
                .to_string()
                .as_bytes(),
        )
        .await,
    }
}

async fn handle_conn(mut stream: TcpStream, kernel: Arc<Kernel>) -> anyhow::Result<()> {
    // 读请求头（至多 64KB）
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    let head_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("客户端关闭");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            anyhow::bail!("请求头过大");
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let path = parts.next().unwrap_or("/").to_string();
    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    // 读请求体
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    let (route, query) = match path.split_once('?') {
        Some((r, q)) => (r, q),
        None => (path.as_str(), ""),
    };

    match (method.as_str(), route) {
        ("GET", "/") | ("GET", "/index.html") => serve_index(&mut stream).await,
        ("GET", "/api/events") => sse_events(stream, kernel, query).await,
        ("GET", "/api/config") => get_config(&mut stream, &kernel).await,
        ("GET", "/api/models") => get_models(&mut stream, &kernel, query).await,
        ("GET", "/api/presets") => get_presets(&mut stream, &kernel).await,
        ("PUT", "/api/config") => put_config(&mut stream, &kernel, &body).await,
        ("GET", "/api/skills") => get_skills(&mut stream, &kernel).await,
        ("GET", r) if r.starts_with("/api/skills/") => {
            get_skill(&mut stream, r.trim_start_matches("/api/skills/")).await
        }
        ("PUT", r) if r.starts_with("/api/skills/") => {
            put_skill(&mut stream, r.trim_start_matches("/api/skills/"), &body).await
        }
        ("DELETE", r) if r.starts_with("/api/skills/") => {
            delete_skill(&mut stream, r.trim_start_matches("/api/skills/")).await
        }
        ("POST", "/api/reveal") => reveal_target(&mut stream, query).await,
        ("GET", r) if r.starts_with("/files/") => {
            serve_workspace_file(&mut stream, r.trim_start_matches("/files/"), query).await
        }
        ("POST", "/api/chat/cancel") => {
            // P2/T1：取消运行中的 chat。转发 agent-loop `cancel` op（置位取消标志，
            // 循环在轮次边界收敛为 K499）。不等待 chat 结束，立即返回。
            // R1 补强：并行向 llm-adapter dispatch `abort`——流式生成逐帧检查命中即关流，
            // 单轮长生成无需等轮次边界（llm-adapter 已为 Concurrent 语义，可即时受理）。
            let session = parse_query(query).get("session").cloned().unwrap_or_default();
            if session.is_empty() {
                return json_resp(&mut stream, 400, bad_request("缺 session 参数", Some("session"))).await;
            }
            match dispatch_or_err(&kernel, "agent-loop", json!({"op": "cancel", "session_id": session})).await {
                Ok(_) => {
                    if let Err(e) =
                        dispatch_or_err(&kernel, "llm-adapter", json!({"op": "abort", "session_id": session})).await
                    {
                        tracing::warn!(session = %session, "llm-adapter abort dispatch failed（轮次边界取消仍有效）: {e}");
                    }
                    json_resp(
                        &mut stream,
                        200,
                        json!({"ok": true, "session_id": session, "note": "取消信号已置位；流式生成中断或轮次边界收敛"}),
                    )
                    .await
                }
                Err(e) => json_resp(&mut stream, 502, e).await,
            }
        }
        ("POST", "/api/chat/rollback") => {
            // R2 回滚：按「第 N 条 user 消息」（0 基）截断——memory 插件原子回滚
            // 会话消息（LLM 上下文）与 trace 事件日志（UI 重放），二者同源对齐。
            let req: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return respond(
                        &mut stream,
                        400,
                        "application/json; charset=utf-8",
                        json!({"ok": false, "error": {"code": "K400", "message": format!("body 非法 JSON: {e}")}})
                            .to_string()
                            .as_bytes(),
                    )
                    .await
                }
            };
            let Some(session) = req.get("session_id").and_then(Value::as_str) else {
                return json_resp(&mut stream, 400, bad_request("缺 session_id", Some("session_id"))).await;
            };
            let Some(idx) = req.get("upto_user_index").and_then(Value::as_u64) else {
                return json_resp(&mut stream, 400, bad_request("缺 upto_user_index", Some("upto_user_index"))).await;
            };
            match dispatch_or_err(
                &kernel,
                "memory",
                json!({"op": "rollback", "session_id": session, "upto_user_index": idx}),
            )
            .await
            {
                Ok(v) => json_resp(&mut stream, 200, v).await,
                Err(e) => json_resp(&mut stream, 502, e).await,
            }
        }
        ("POST", "/api/chat") => {
            let req: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return respond(
                        &mut stream,
                        400,
                        "application/json; charset=utf-8",
                        json!({"ok": false, "error": {"code": "K400", "message": format!("body 非法 JSON: {e}")}})
                            .to_string()
                            .as_bytes(),
                    )
                    .await
                }
            };
            let Some(session) = req.get("session_id").and_then(Value::as_str) else {
                return respond(
                    &mut stream,
                    400,
                    "application/json; charset=utf-8",
                    json!({"ok": false, "error": {"code": "K400", "message": "缺 session_id"}})
                        .to_string()
                        .as_bytes(),
                )
                .await;
            };
            let Some(message) = req.get("message").and_then(Value::as_str) else {
                return respond(
                    &mut stream,
                    400,
                    "application/json; charset=utf-8",
                    json!({"ok": false, "error": {"code": "K400", "message": "缺 message"}})
                        .to_string()
                        .as_bytes(),
                )
                .await;
            };
            // R3 附件（可选）：[{name, mime, data_b64}]——校验形状与体量上限后透传
            // agent-loop（构造多模态 user 消息）。非法/超限 = 确定性 K400，不静默丢弃。
            let mut chat_payload = json!({"op": "chat", "session_id": session, "user_text": message});
            if let Some(att) = req.get("attachments") {
                match validate_attachments(att) {
                    Ok(Some(list)) => {
                        chat_payload["attachments"] = list;
                    }
                    Ok(None) => {}
                    Err(e) => return json_resp(&mut stream, 400, e).await,
                }
            }
            let resp = kernel
                .dispatch(Envelope::new(PluginId::new("agent-loop"), chat_payload))
                .await
                .unwrap_or_else(|e| json!({"ok": false, "error": {"code": "K500", "message": e.to_string()}}));
            respond(
                &mut stream,
                200,
                "application/json; charset=utf-8",
                resp.to_string().as_bytes(),
            )
            .await
        }
        _ => {
            respond(
                &mut stream,
                404,
                "application/json; charset=utf-8",
                json!({"ok": false, "error": {"code": "K404", "message": format!("no route: {method} {route}")}})
                    .to_string()
                    .as_bytes(),
            )
            .await
        }
    }
}

// ── 配置中心与技能 CRUD（08 §2.2）──

/// GET /api/presets：转发 llm-adapter `presets.list`（OpenAI 兼容站点预设清单，
/// 数据源 plugins/llm_adapter/presets.py——切换站点 = configure 热应用，零重启）。
async fn get_presets(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
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
async fn get_models(stream: &mut TcpStream, kernel: &Kernel, query: &str) -> anyhow::Result<()> {
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

async fn json_resp(stream: &mut TcpStream, status: u16, v: Value) -> anyhow::Result<()> {
    respond(stream, status, "application/json; charset=utf-8", v.to_string().as_bytes()).await
}

fn bad_request(msg: impl Into<String>, field: Option<&str>) -> Value {
    let mut error = json!({"code": "K400", "message": msg.into()});
    if let Some(f) = field {
        error["field"] = json!(f);
    }
    json!({"ok": false, "error": error})
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
fn validate_attachments(v: &Value) -> Result<Option<Value>, Value> {
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

/// guest 调用便捷封装：ok → Ok(v)；业务失败/传输失败 → Err（错误 payload）。
async fn dispatch_or_err(kernel: &Kernel, target: &str, payload: Value) -> Result<Value, Value> {
    match kernel.dispatch(Envelope::new(PluginId::new(target), payload)).await {
        Ok(v) if v.get("ok") == Some(&json!(true)) => Ok(v),
        Ok(v) => Err(v),
        Err(e) => Err(json!({"ok": false, "error": {"code": "K500", "message": e.to_string()}})),
    }
}

/// GET /api/config：llm 视图（config.json > env 缺省；key 只回 key_set + 尾 4 位，绝不回明文）
/// + tools 全集视图（list all=true，含未启用项，各项附 enabled）+ skills 计数。
async fn get_config(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
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

    let tools = match dispatch_or_err(kernel, "tools", json!({"op": "list", "all": true})).await {
        Ok(v) => v
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|t| {
                        json!({
                            "name": t.get("name").and_then(Value::as_str).unwrap_or("?"),
                            "enabled": t.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        Err(_) => vec![],
    };
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
async fn put_config(stream: &mut TcpStream, kernel: &Kernel, body: &[u8]) -> anyhow::Result<()> {
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        return json_resp(stream, 400, bad_request("body 非法 JSON", None)).await;
    };
    let llm = req.get("llm").filter(|v| v.is_object());
    let tools = req.get("tools").filter(|v| v.is_object());
    let agent = req.get("agent").filter(|v| v.is_object());
    if llm.is_none() && tools.is_none() && agent.is_none() {
        return json_resp(stream, 400, bad_request("body 需含 llm / tools / agent 对象", None)).await;
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
async fn get_skills(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
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
async fn get_skill(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
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
async fn put_skill(stream: &mut TcpStream, name: &str, body: &[u8]) -> anyhow::Result<()> {
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
async fn delete_skill(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
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
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => json_resp(stream, 200, json!({"ok": true, "name": name})).await,
        Err(e) => json_resp(stream, 500, json!({"ok": false, "error": {"code": "K500", "message": format!("删除失败: {e}")}})).await,
    }
}

/// POST /api/reveal?target=config|tools|skills|skill&name=<name>：在系统文件管理器中
/// 打开配置源文件所在位置（浏览器 http 页面无法跳 file:// 链接，由 host 代为打开）。
/// 只允许白名单目标——config.json / plugins/tools / skills 根 / 具名技能目录
/// （名字约束杜绝路径注入）。explorer 退出码无意义，spawn 后不等待。
async fn reveal_target(stream: &mut TcpStream, query: &str) -> anyhow::Result<()> {
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

/// 工具源码目录（内置 8 件 + 动态装载池）：PLUGINS_DIR/tools，缺省 <workspace>/plugins/tools。
fn tools_dir() -> std::path::PathBuf {
    std::env::var_os("PLUGINS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config::workspace_dir().join("plugins"))
        .join("tools")
}

/// GET /files/{path}[?download=1]：按 mime 服务工作区内文件。前端文件卡片（artifact
/// trace 事件渲染）经此取内容/下载——浏览器禁 file:// 链接，host 代为 serve。
/// 与文件工具同一越界纪律：realpath ⊆ WORKSPACE_ROOT；目录/越界/缺失/超限均明确拒绝。
async fn serve_workspace_file(stream: &mut TcpStream, raw: &str, query: &str) -> anyhow::Result<()> {
    const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
    // 根口径与文件工具一致：WORKSPACE_ROOT env 优先（越界拦截同一基准），缺省编译期工作区
    let ws_base = std::env::var_os("WORKSPACE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(config::workspace_dir);
    let ws = std::fs::canonicalize(&ws_base).unwrap_or_else(|_| ws_base);
    // 路径归一正斜杠（历史 artifact 可能含反斜杠/绝对前缀），穿越校验统一按 '/'
    let rel = percent_decode(raw.trim_end_matches('/')).replace('\\', "/");
    if rel.is_empty() || rel.split('/').any(|seg| seg == "..") {
        return json_resp(stream, 400, bad_request("非法路径", Some("path"))).await;
    }
    let full = ws.join(&rel);
    let real = match std::fs::canonicalize(&full) {
        Ok(r) => r,
        Err(_) => {
            // 兜底：历史产物路径可能带工作区绝对前缀（含空格路径被空白截断/模型复述
            // 绝对路径）——含「工作区目录名/」时截掉前缀重试一次。最终仍走 realpath 越界校验。
            let root_name = ws.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let retry = (!root_name.is_empty())
                .then(|| rel.split_once(&format!("{root_name}/")).map(|(_, rest)| rest))
                .flatten()
                .map(|rest| std::fs::canonicalize(ws.join(rest)));
            match retry {
                Some(Ok(r)) => r,
                _ => {
                    return json_resp(
                        stream,
                        404,
                        json!({"ok": false, "error": {"code": "K404", "message": format!("文件不存在: {rel}")}}),
                    )
                    .await
                }
            }
        }
    };
    let (a, b) = (os_normcase(&real), os_normcase(&ws));
    if !(a == b || a.starts_with(&format!("{b}{}", std::path::MAIN_SEPARATOR_STR))
        || a.starts_with(&format!("{b}\\")))
    {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": "路径越界：不在工作区内"}}),
        )
        .await;
    }
    if !real.is_file() {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": "该路径不是文件（目录不支持预览）"}}),
        )
        .await;
    }
    let meta = std::fs::metadata(&real)?;
    if meta.len() > MAX_FILE_BYTES {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": format!("文件超过 {}MB 上限，不支持经 /files 传输", MAX_FILE_BYTES / 1024 / 1024)}}),
        )
        .await;
    }
    let bytes = tokio::fs::read(&real).await?;
    let mime = mime_for(&real);
    let download = parse_query(query).contains_key("download");
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let disp = if download {
        format!("attachment; filename*=UTF-8''{}", percent_encode(&name))
    } else {
        "inline".to_string()
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {mime}\r\ncontent-disposition: {disp}; filename=\"{}\"\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        name.replace(['\\', '"', '\r', '\n'], "_"),
        bytes.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

fn os_normcase(p: &std::path::Path) -> String {
    #[cfg(target_os = "windows")]
    {
        p.to_string_lossy().to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        p.to_string_lossy().into_owned()
    }
}

/// 按扩展名推 Content-Type（MVP 清单：产物卡片用到的 + 常见文本/图片）。
fn mime_for(p: &std::path::Path) -> &'static str {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" | "log" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// 最小 percent-encode（RFC 5987 filename*）：非 unreserved 字节转 %XX。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// SSE：从 `after` 起增量轮询 memory 的 trace.read，逐事件 `data:` 推送；客户端断开即返回。
/// 连接建立即从 after=0 重放全量（刷新恢复 = 日志重放），随后跟随实时增量。
async fn sse_events(mut stream: TcpStream, kernel: Arc<Kernel>, query: &str) -> anyhow::Result<()> {
    let params = parse_query(query);
    let session = params.get("session").cloned().unwrap_or_else(|| "default".into());
    let mut after: u64 = params.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);

    // 写 SSE 响应头
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ncache-control: no-cache\r\nconnection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let (mut read_half, mut write_half) = tokio::io::split(stream);
    // 重放阶段（after 尚未 catch up）：只推持久 trace 事件、不推流式旁路（最终 assistant 已含完整内容）；
    // 一旦某批 trace.read 返回空（catch up）即进入实时阶段，开始推 stream_*。
    let mut replaying = true;
    let mut ping_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    // 流式旁路：llm-adapter 边生成边写增量，本连接 tail 后与 trace 事件合并推送。
    // session 名非法（防路径穿越）→ None，退化为纯 trace 重放。
    let stream_path = config::stream_file(&session);
    let mut stream_off: u64 = 0;
    // R11 子代理旁路：连接建立时刻（门闩基准）+ 逐子文件偏移表（每个子会话一个旁路文件）。
    let connected_at = std::time::SystemTime::now();
    let mut sub_offs: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    loop {
        // 增量拉取
        let resp = kernel
            .dispatch(Envelope::new(
                PluginId::new("memory"),
                json!({"op": "trace.read", "session_id": session, "after": after}),
            ))
            .await;
        let mut has_new = false;
        match resp {
            Ok(v) if v.get("ok") == Some(&json!(true)) => {
                if let Some(events) = v.get("events").and_then(Value::as_array) {
                    for e in events {
                        let line = format!("data: {e}\n\n");
                        write_half.write_all(line.as_bytes()).await?;
                        has_new = true;
                    }
                }
                after = v.get("next").and_then(Value::as_u64).unwrap_or(after);
                // 本批无新事件 = 已 catch up（重放结束）→ 此后进入实时阶段，开始推流式旁路
                let empty = v.get("events").and_then(Value::as_array).map_or(true, |a| a.is_empty());
                if empty {
                    replaying = false;
                }
            }
            Ok(v) => {
                let line = format!(
                    "event: error\ndata: {}\n\n",
                    json!({"type": "error", "where": "trace.read", "message": v.to_string()})
                );
                write_half.write_all(line.as_bytes()).await?;
            }
            Err(e) => {
                let line = format!(
                    "event: error\ndata: {}\n\n",
                    json!({"type": "error", "where": "trace.read", "message": e.to_string()})
                );
                write_half.write_all(line.as_bytes()).await?;
            }
        }
        // 流式旁路增量：读文件新增字节（按行；未写完的半行留给下一轮补齐）。
        // 仅在实时阶段（replaying=false）推送——重放阶段已由最终 assistant 事件覆盖，不再推中间态。
        let mut streaming = false;
        if !replaying {
        if let Some(path) = &stream_path {
            if let Ok(buf) = std::fs::read(path) {
                let len = buf.len() as u64;
                if len < stream_off {
                    stream_off = 0; // 新一轮以 "w" 覆盖重写 → 从头读
                }
                if len > stream_off {
                    let text = String::from_utf8_lossy(&buf[stream_off as usize..]).to_string();
                    let mut consumed = 0usize;
                    for line in text.split_inclusive('\n') {
                        if !line.ends_with('\n') {
                            break; // 半行：等下一轮
                        }
                        consumed += line.len();
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(mut ev) = serde_json::from_str::<Value>(trimmed) else {
                            continue; // 损坏行：跳过（offset 已推进，不重试）
                        };
                        // 加 stream_ 前缀，避免与 trace 事件类型撞名（如 error）
                        let mapped = match ev.get("type").and_then(Value::as_str) {
                            Some("start") => "stream_start",
                            Some("delta") => "stream_delta",
                            Some("end") => "stream_end",
                            Some("error") => "stream_error",
                            _ => continue,
                        };
                        ev["type"] = json!(mapped);
                        write_half.write_all(format!("data: {ev}\n\n").as_bytes()).await?;
                        streaming = true;
                        has_new = true;
                    }
                    stream_off += consumed as u64;
                }
            }
        }
        }
        // R11 子代理流式旁路：tail `{session}#sub-*.jsonl`（sink 每轮以 "w" 覆写，语义同主文件）。
        // 门闩：仅 tail mtime 晚于连接建立的子文件——上回合/回滚残留的陈旧子文件不复活。
        // 帧打 `sub` 标签（`#` 后缀），前端据此路由进过程框内的「子代理」框。
        if !replaying {
            if let Ok(entries) = std::fs::read_dir(config::stream_dir()) {
                let prefix = format!("{session}#");
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let Some(sub) = name
                        .strip_prefix(&prefix)
                        .and_then(|s| s.strip_suffix(".jsonl"))
                        .filter(|s| !s.is_empty() && !s.contains('#'))
                    else {
                        continue;
                    };
                    if entry.metadata().ok().map_or(true, |m| match m.modified() {
                        Ok(mt) => mt <= connected_at,
                        Err(_) => true,
                    }) {
                        continue; // 陈旧子文件或取不到 mtime：不 tail
                    }
                    let Ok(buf) = std::fs::read(entry.path()) else {
                        continue;
                    };
                    let len = buf.len() as u64;
                    let off = sub_offs.entry(name.clone()).or_insert(0);
                    if len < *off {
                        *off = 0; // 新一轮以 "w" 覆写重写 → 从头读
                    }
                    if len > *off {
                        let text = String::from_utf8_lossy(&buf[*off as usize..]).to_string();
                        let mut consumed = 0usize;
                        for line in text.split_inclusive('\n') {
                            if !line.ends_with('\n') {
                                break; // 半行：等下一轮
                            }
                            consumed += line.len();
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let Ok(mut ev) = serde_json::from_str::<Value>(trimmed) else {
                                continue; // 损坏行：跳过（offset 已推进，不重试）
                            };
                            let mapped = match ev.get("type").and_then(Value::as_str) {
                                Some("start") => "stream_start",
                                Some("delta") => "stream_delta",
                                Some("end") => "stream_end",
                                Some("error") => "stream_error",
                                _ => continue,
                            };
                            ev["type"] = json!(mapped);
                            ev["sub"] = json!(sub);
                            write_half.write_all(format!("data: {ev}\n\n").as_bytes()).await?;
                            streaming = true;
                            has_new = true;
                        }
                        *off += consumed as u64;
                    }
                }
            }
        }
        if has_new {
            write_half.flush().await?;
            if !streaming {
                continue; // 非流式积压立刻再拉；流式期间让位给下面的 sleep，避免忙轮询
            }
        }
        // 心跳注释行（防中间层空闲断连）
        if tokio::time::Instant::now() >= ping_deadline {
            write_half.write_all(b": ping\n\n").await?;
            write_half.flush().await?;
            ping_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        }
        // 等待：流式进行中 50ms（跟手），空闲 300ms（省 memory 轮询开销）
        let poll_ms = if streaming { 50 } else { 300 };
        let mut byte = [0u8; 1];
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {}
            r = read_half.read(&mut byte) => {
                if r.is_err() || r.unwrap_or(1) == 0 {
                    break; // 客户端离开
                }
                // 收到数据（不太可能）：忽略继续
            }
        }
    }
    Ok(())
}

async fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

/// 极简 query 解析（含 %XX 与 '+' 解码）。
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 1 && i + 2 < bytes.len() + 1 => {
                let hex = bytes.get(i + 1..i + 3).and_then(|h| {
                    std::str::from_utf8(h).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
                });
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parses_and_decodes() {
        let q = parse_query("session=a%20b&after=7&x=");
        assert_eq!(q.get("session").unwrap(), "a b");
        assert_eq!(q.get("after").unwrap(), "7");
        assert_eq!(q.get("x").unwrap(), "");
        assert!(q.get("missing").is_none());
    }

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
