//! Web 前端（W10 自 frontend.rs 拆分）：dsh 风格 web 网关——极简手写 HTTP
//! （tokio TcpListener，不引 web 框架依赖）。
//!
//! 子模块：
//! - [`gateway`]：SSE 事件流（trace 重放 + 流式旁路 + 子代理旁路）
//! - [`api`]：配置中心 / 技能 CRUD / models / presets / 附件校验
//! - [`files`]：`/files` 工作区文件服务
//!
//! host 是组合根：前端是**入口组件**而非 guest 能力——网关需调 `agent.chat` +
//! `session.trace`，而 guest 不可互调（内核物理约束）；前端切换必伴随重启。
//! 静态单页 `web-dist/`（index.html + style.css + app.js）运行时 serve，改样式刷新即生效。
mod api;
mod files;
mod gateway;

use crate::frontend::Frontend;
use agent_kernel_sdk::{Envelope, PluginId};
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Cursor 暖色系「事件流式会话」前端。
///
/// 路由：
///   GET  / /index.html /style.css /app.js → web-dist/ 静态文件（白名单，运行时读取）
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

/// 定位 web 前端静态文件 `web-dist/{name}`（白名单：index.html / style.css / app.js，
/// 杜绝目录穿越），按优先级尝试：
/// 1. 编译期注入的源码目录（`CARGO_MANIFEST_DIR`，`cargo run`/`build` 时有效）；
/// 2. 相对 cwd 的 `crates/host/web-dist/`（在 workspace 根启动）；
/// 3. 相对二进制同级的 `web-dist/`（打包发布）。
fn web_dist_file(name: &str) -> Option<PathBuf> {
    const ALLOWED: &[&str] = &["index.html", "style.css", "app.js"];
    if !ALLOWED.contains(&name) {
        return None;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("web-dist").join(name));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("crates").join("host").join("web-dist").join(name));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("web-dist").join(name));
        }
    }
    candidates.into_iter().find(|p| p.exists())
}

/// `GET /`、`/style.css`、`/app.js`：从 `web-dist/` 读取并返回；文件缺失给出明确 500（而非 panic）。
/// 强制 `no-store`：前端是运行时 serve 的，任何改动刷新即生效，绝不被浏览器缓存旧 JS。
async fn serve_static(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
    let content_type = match name {
        "style.css" => "text/css; charset=utf-8",
        "app.js" => "text/javascript; charset=utf-8",
        _ => "text/html; charset=utf-8",
    };
    match web_dist_file(name) {
        Some(p) => match tokio::fs::read(&p).await {
            Ok(bytes) => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
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
            json!({"ok": false, "error": {"code": "K500", "message": format!("找不到 web-dist/{name}（应在 crates/host/web-dist/ 下；cargo run 在 workspace 根或 crate 同级即可）")}})
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
        ("GET", "/") | ("GET", "/index.html") => serve_static(&mut stream, "index.html").await,
        ("GET", "/style.css") => serve_static(&mut stream, "style.css").await,
        ("GET", "/app.js") => serve_static(&mut stream, "app.js").await,
        ("GET", "/api/events") => gateway::sse_events(stream, kernel, query).await,
        ("GET", "/api/config") => api::get_config(&mut stream, &kernel).await,
        ("GET", "/api/models") => api::get_models(&mut stream, &kernel, query).await,
        ("GET", "/api/presets") => api::get_presets(&mut stream, &kernel).await,
        ("PUT", "/api/config") => api::put_config(&mut stream, &kernel, &body).await,
        ("GET", "/api/skills") => api::get_skills(&mut stream, &kernel).await,
        ("GET", r) if r.starts_with("/api/skills/") => {
            api::get_skill(&mut stream, r.trim_start_matches("/api/skills/")).await
        }
        ("PUT", r) if r.starts_with("/api/skills/") => {
            api::put_skill(&mut stream, r.trim_start_matches("/api/skills/"), &body).await
        }
        ("DELETE", r) if r.starts_with("/api/skills/") => {
            api::delete_skill(&mut stream, r.trim_start_matches("/api/skills/")).await
        }
        ("POST", "/api/reveal") => api::reveal_target(&mut stream, query).await,
        ("GET", r) if r.starts_with("/files/") => {
            files::serve_workspace_file(&mut stream, r.trim_start_matches("/files/"), query).await
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
                match api::validate_attachments(att) {
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

/// guest 调用便捷封装：ok → Ok(v)；业务失败/传输失败 → Err（错误 payload）。
async fn dispatch_or_err(kernel: &Kernel, target: &str, payload: Value) -> Result<Value, Value> {
    match kernel.dispatch(Envelope::new(PluginId::new(target), payload)).await {
        Ok(v) if v.get("ok") == Some(&json!(true)) => Ok(v),
        Ok(v) => Err(v),
        Err(e) => Err(json!({"ok": false, "error": {"code": "K500", "message": e.to_string()}})),
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
