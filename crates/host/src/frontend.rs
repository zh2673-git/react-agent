//! 前端装配（Phase 3-2，01 §Phase3）：`trait Frontend` 两实现。
//!
//! - `ReplFrontend`：终端交互（REPL / 单轮），斜杠命令见 `repl_command`。
//! - `WebFrontend`：dsh 风格「事件流式会话」——静态单页 + SSE 实时渲染 + 日志重放恢复。
//!
//! host 是组合根：前端是**入口组件**而非 guest 能力——网关需调 `agent.chat` + `session.trace`，
//! 而 guest 不可互调（内核物理约束）；前端切换必伴随重启，热插拔无收益。
//! 选择：`REACT_FRONTEND=repl`（默认）/ `web`（`WEB_ADDR` 默认 127.0.0.1:8710）。
use agent_kernel_sdk::{Envelope, PluginId};
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use std::io::Write;
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

/// dsh 风格单页（编译期内嵌，无外部静态文件）。
static WEB_HTML: &str = include_str!("web.html");

/// dsh 风格 web 网关：极简手写 HTTP（tokio TcpListener，不引 web 框架依赖）。
///
/// 路由：
///   GET  /                      → dsh 风格单页（web.html，include_str! 内嵌）
///   GET  /api/events?session=&after= → SSE：增量轮询 memory trace.read，逐事件推送
///   POST /api/chat              → {"session_id","message"} → agent.chat（阻塞到收敛）
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
        ("GET", "/") | ("GET", "/index.html") => {
            respond(&mut stream, 200, "text/html; charset=utf-8", WEB_HTML.as_bytes()).await
        }
        ("GET", "/api/events") => sse_events(stream, kernel, query).await,
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
            let resp = kernel
                .dispatch(Envelope::new(
                    PluginId::new("agent-loop"),
                    json!({"op": "chat", "session_id": session, "user_text": message}),
                ))
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

/// SSE：从 `after` 起增量轮询 memory 的 trace.read，逐事件 `data:` 推送；客户端断开即返回。
/// 连接建立即从 after=0 重放全量（刷新恢复 = 日志重放），随后跟随实时增量。
async fn sse_events(mut stream: TcpStream, kernel: Arc<Kernel>, query: &str) -> anyhow::Result<()> {
    let params = parse_query(query);
    let session = params.get("session").cloned().unwrap_or_else(|| "default".into());
    let mut after: u64 = params.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);

    // 写 SSE 响应头
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let mut ping_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
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
        if has_new {
            write_half.flush().await?;
            continue; // 有积压立刻再拉
        }
        // 心跳注释行（防中间层空闲断连）
        if tokio::time::Instant::now() >= ping_deadline {
            write_half.write_all(b": ping\n\n").await?;
            write_half.flush().await?;
            ping_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        }
        // 等待：300ms 轮询 or 客户端断开
        let mut byte = [0u8; 1];
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => {}
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
}
