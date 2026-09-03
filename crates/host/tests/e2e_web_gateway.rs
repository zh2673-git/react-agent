//! e2e：web 网关全链路（Phase 3-2）——真实 memory/llm(mock)/tools guest + 真 agent-loop。
//! POST /api/chat → agent.chat 收敛；GET /api/events → SSE 从 0 重放全量事件；
//! 非法请求 → 400 字段级错误。HTTP 客户端用裸 TcpStream（不引测试依赖）。

mod common;

use common::*;
use react_agent_agent_loop::new as agent_loop;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn web_gateway_serves_chat_and_sse_replay() {
    let Some(py) = find_interpreter() else {
        skip("python not found");
        return;
    };
    let Some(node) = find_node() else {
        skip("node not found (>= 22.6 required)");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }

    // mock 序列：① list_dir 工具调用 ② 最终收敛
    let script = json!([
        {"ok": true, "content": null, "tool_calls": [{"id": "call-w", "name": "list_dir", "arguments": {"path": "."}}], "model": "mock", "finish_reason": "tool_calls"},
        {"ok": true, "content": "web-gateway-final", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
    ]);

    let mem_dir = std::env::temp_dir().join(format!("react-agent-gw-{}-{}", std::process::id(), nanos()));
    let ws_dir = std::env::temp_dir().join(format!("react-agent-gw-ws-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws_dir).expect("create workspace");

    let kernel = fresh_kernel();

    let mem = spawn_node_ts(
        node,
        &plugins_dir().join("memory").join("memory_plugin.ts"),
        guest_manifest("memory", &["memory.session", "session.trace"]),
        &[("MEMORY_DATA_DIR", mem_dir.to_string_lossy().into())],
    )
    .await
    .expect("spawn memory");
    register(&kernel, mem).await;

    let llm = spawn_python(
        py,
        &plugins_dir().join("llm_adapter").join("llm_plugin.py"),
        guest_manifest("llm-adapter", &["llm.chat"]),
        &[("LLM_PROVIDER", "mock".into()), ("MOCK_SCRIPT", script.to_string())],
    )
    .await
    .expect("spawn llm-adapter");
    register(&kernel, llm).await;

    let tools = spawn_python(
        py,
        &plugins_dir().join("tools").join("tools_plugin.py"),
        guest_manifest("tools", &["tools.exec"]),
        &[("WORKSPACE_ROOT", ws_dir.to_string_lossy().into_owned())],
    )
    .await
    .expect("spawn tools");
    register(&kernel, tools).await;

    kernel.register(agent_loop(8)).await;

    // 启动 web 网关（port 0 → 测试取真实端口）
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind :0");
    let addr = listener.local_addr().expect("local_addr");
    let server_kernel = kernel.clone();
    tokio::spawn(async move {
        let _ = react_agent_host::frontend::WebFrontend::serve_listener(listener, server_kernel).await;
    });

    // 1. POST /api/chat：阻塞到 agent 收敛
    let (status, body) = http(
        addr,
        "POST",
        "/api/chat",
        Some(&json!({"session_id": "gw", "message": "list the dir"}).to_string()),
    )
    .await;
    assert_eq!(status, 200, "chat status: {body}");
    let v: Value = serde_json::from_str(&body).expect("chat body JSON");
    assert_eq!(v["ok"], json!(true), "chat: {v}");
    assert_eq!(v["answer"], json!("web-gateway-final"), "chat: {v}");

    // 2. GET /api/events：SSE 从 0 全量重放（读到 assistant 事件为止）
    let (status, sse) = sse_read_until(addr, "assistant", Duration::from_secs(10)).await;
    assert_eq!(status, 200, "sse status");
    let types: Vec<String> = sse
        .split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|e| e["type"].as_str().map(str::to_string))
        .collect();
    for expected in ["user", "tool_call", "tool_result", "assistant"] {
        assert!(types.iter().any(|t| t == expected), "SSE 事件流缺 {expected}: {types:?}");
    }

    // 3. 非法请求：缺 message → 400 字段级错误
    let (status, body) = http(addr, "POST", "/api/chat", Some(&json!({"session_id": "gw"}).to_string())).await;
    assert_eq!(status, 400, "missing message: {body}");
    assert!(body.contains("message"), "错误应指明缺失字段: {body}");

    // 4. 未知路由 → 404
    let (status, _) = http(addr, "GET", "/nope", None).await;
    assert_eq!(status, 404);

    let _ = std::fs::remove_dir_all(&mem_dir);
    let _ = std::fs::remove_dir_all(&ws_dir);
    kernel.stop();
    kernel.destroy().await;
}

/// 裸 HTTP/1.1 请求（connection: close，读到 EOF）。
async fn http(addr: std::net::SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.expect("write req");
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut raw))
        .await
        .expect("read response timeout")
        .expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// SSE：读到包含 target 子串或超时；返回 (status, 已收到的全部帧文本)。
async fn sse_read_until(addr: std::net::SocketAddr, target: &str, timeout: Duration) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect sse");
    stream
        .write_all(b"GET /api/events?session=gw&after=0 HTTP/1.1\r\nhost: localhost\r\naccept: text/event-stream\r\n\r\n")
        .await
        .expect("write sse req");

    // 读响应头（至 \r\n\r\n），status 行取第 2 个 token
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    let head_end = loop {
        let n = stream.read(&mut buf).await.expect("read sse head");
        assert!(n > 0, "SSE 连接提前关闭");
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let rest = String::from_utf8_lossy(&raw[head_end + 4..]).into_owned();
    (status, sse_body(stream, rest, target, timeout).await)
}

/// 头部后继读 SSE 帧，直到出现 target 或超时。
async fn sse_body(
    mut stream: TcpStream,
    mut acc: String,
    target: &str,
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    while !acc.contains(target) {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let mut buf = [0u8; 4096];
        match tokio::time::timeout(deadline - now, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => acc.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => break, // 超时
        }
    }
    acc
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
