//! e2e：web 网关全链路（Phase 3-2 / Phase 4）——真实 memory/llm(mock)/tools guest + 真 agent-loop。
//! POST /api/chat → agent.chat 收敛；GET /api/events → SSE 从 0 重放全量事件；
//! 非法请求 → 400 字段级错误。HTTP 客户端用裸 TcpStream（不引测试依赖）。
//! Phase 4（08）：/api/config 读写（llm 热应用 + 落盘 + tools 白名单）、/api/skills CRUD。

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

/// Phase 4（08 §2.2）：配置中心 + 技能 CRUD 全链路。
/// 前置：CONFIG_FILE / SKILLS_DIR 指向临时目录（宿主进程级 env，本文件内另一测试不读它们）。
#[tokio::test]
async fn web_config_center_and_skills_crud() {
    let Some(py) = find_interpreter() else {
        skip("python not found");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }

    // 隔离：config.json 与 skills 目录都落临时路径（不影响真实项目文件）
    let tag = format!("{}-{}", std::process::id(), nanos());
    let cfg_file = std::env::temp_dir().join(format!("react-agent-cfg-{tag}.json"));
    let skills_dir = std::env::temp_dir().join(format!("react-agent-skills-{tag}"));
    std::fs::create_dir_all(&skills_dir).expect("create skills dir");
    // 先设 env 再 spawn：宿主 skills_dir() 与 assets guest 同源
    // （同二进制并行测试不受影响：唯一兄弟测试不读这些变量）
    std::env::set_var("CONFIG_FILE", &cfg_file);
    std::env::set_var("SKILLS_DIR", &skills_dir);
    // key 断言需确定性：暂存并移除宿主进程可能携带的真实 key
    let saved_keys: Vec<_> = ["OPENAI_API_KEY", "ANTHROPIC_API_KEY"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (*k, v)))
        .collect();
    for (k, _) in &saved_keys {
        std::env::remove_var(k);
    }

    let kernel = fresh_kernel();
    let llm = spawn_python(
        py,
        &plugins_dir().join("llm_adapter").join("llm_plugin.py"),
        guest_manifest("llm-adapter", &["llm.chat"]),
        &[("LLM_PROVIDER", "mock".into()), ("LLM_MODEL", "mock-1".into())],
    )
    .await
    .expect("spawn llm-adapter");
    register(&kernel, llm).await;

    let tools = spawn_python(
        py,
        &plugins_dir().join("tools").join("tools_plugin.py"),
        guest_manifest("tools", &["tools.exec"]),
        &[("WORKSPACE_ROOT", std::env::temp_dir().to_string_lossy().into_owned())],
    )
    .await
    .expect("spawn tools");
    register(&kernel, tools).await;

    let assets = spawn_python(
        py,
        &plugins_dir().join("assets").join("assets_plugin.py"),
        guest_manifest("assets", &["assets.registry"]),
        &[("SKILLS_DIR", skills_dir.to_string_lossy().into_owned())],
    )
    .await
    .expect("spawn assets");
    register(&kernel, assets).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind :0");
    let addr = listener.local_addr().expect("local_addr");
    let server_kernel = kernel.clone();
    tokio::spawn(async move {
        let _ = react_agent_host::frontend::WebFrontend::serve_listener(listener, server_kernel).await;
    });

    // 1. GET /api/config：初始视图（spawn env 为真相；未落盘前 tools 全启用）
    let (status, body) = http(addr, "GET", "/api/config", None).await;
    assert_eq!(status, 200, "get config: {body}");
    let v: Value = serde_json::from_str(&body).expect("config JSON");
    assert_eq!(v["config"]["llm"]["provider"], json!("mock"), "{v}");
    assert_eq!(v["config"]["llm"]["key"]["key_set"], json!(false), "{v}");
    let tools: Vec<&Value> = v["config"]["tools"].as_array().expect("tools array").iter().collect();
    assert!(tools.iter().any(|t| t["name"] == json!("bash") && t["enabled"] == json!(true)), "{v}");

    // 2. PUT /api/config：llm（model+api_key）+ tools 白名单（只留 read_file）
    let (status, body) = http(
        addr,
        "PUT",
        "/api/config",
        Some(&json!({
            "llm": {"model": "mock-2", "api_key": "sk-test-1234"},
            "tools": {"enabled": ["read_file"]}
        }).to_string()),
    )
    .await;
    assert_eq!(status, 200, "put config: {body}");

    // 3. 回读：模型/key 尾号已更新；bash 出池 read_file 在池（热生效）
    let (status, body) = http(addr, "GET", "/api/config", None).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).expect("config JSON");
    assert_eq!(v["config"]["llm"]["model"], json!("mock-2"), "{v}");
    assert_eq!(v["config"]["llm"]["key"]["key_set"], json!(true), "{v}");
    assert_eq!(v["config"]["llm"]["key"]["key_tail"], json!("1234"), "key 只回尾 4 位: {v}");
    let tools_arr = v["config"]["tools"].as_array().expect("tools array");
    let bash = tools_arr.iter().find(|t| t["name"] == json!("bash")).expect("bash in all-view");
    assert_eq!(bash["enabled"], json!(false), "configure 后 bash 应未启用: {v}");
    let rf = tools_arr.iter().find(|t| t["name"] == json!("read_file")).expect("read_file");
    assert_eq!(rf["enabled"], json!(true), "{v}");

    // 4. 持久化：config.json 落盘且含 model/api_key/tools.enabled（重启还原通道）
    let persisted: Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg_file).expect("config.json exists")).expect("persisted JSON");
    assert_eq!(persisted["llm"]["model"], json!("mock-2"), "{persisted}");
    assert_eq!(persisted["llm"]["api_key"], json!("sk-test-1234"), "{persisted}");
    assert_eq!(persisted["tools"]["enabled"], json!(["read_file"]), "{persisted}");

    // 5. 技能 CRUD：PUT → GET 列表（assets 重扫可见）→ GET 单个回读
    let skill_md = "---\nname: e2e-skill\ndescription: e2e 测试技能\n---\n\n# 执行指引\n测试内容\n";
    let (status, body) = http(
        addr,
        "PUT",
        "/api/skills/e2e-skill",
        Some(&json!({"content": skill_md}).to_string()),
    )
    .await;
    assert_eq!(status, 200, "put skill: {body}");
    let (status, body) = http(addr, "GET", "/api/skills", None).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("e2e-skill"), "列表应含新技能: {body}");
    let (status, body) = http(addr, "GET", "/api/skills/e2e-skill", None).await;
    assert_eq!(status, 200, "get skill: {body}");
    let v: Value = serde_json::from_str(&body).expect("skill JSON");
    assert_eq!(v["content"], json!(skill_md), "回读应与写入一致: {v}");

    // 6. 技能写入校验：frontmatter 缺失 → 400；name 不一致 → 400；非法名（路径注入）→ 400
    let (status, body) = http(addr, "PUT", "/api/skills/bad", Some(&json!({"content": "no frontmatter"}).to_string())).await;
    assert_eq!(status, 400, "frontmatter 缺失: {body}");
    let bad_fm = "---\nname: other\ndescription: x\n---\nbody";
    let (status, _) = http(addr, "PUT", "/api/skills/bad", Some(&json!({"content": bad_fm}).to_string())).await;
    assert_eq!(status, 400, "name 不一致应 400");
    let (status, _) = http(addr, "PUT", "/api/skills/a%2Fb", Some(&json!({"content": bad_fm}).to_string())).await;
    assert_eq!(status, 400, "路径注入名应 400");

    // 7. DELETE：删后列表与单个均不可见
    let (status, _) = http(addr, "DELETE", "/api/skills/e2e-skill", None).await;
    assert_eq!(status, 200, "delete skill");
    let (status, _) = http(addr, "GET", "/api/skills/e2e-skill", None).await;
    assert_eq!(status, 404, "删除后应 404");
    let (status, body) = http(addr, "GET", "/api/skills", None).await;
    assert_eq!(status, 200);
    assert!(!body.contains("e2e-skill"), "列表不应再含已删技能: {body}");

    // 8. reveal 白名单与校验：未知 target → 400；非法技能名 → 400；不存在的技能 → 404。
    //    仅测错误路径（成功路径会 spawn 真实资源管理器窗口，不入自动化）。
    let (status, _) = http(addr, "POST", "/api/reveal?target=nope", None).await;
    assert_eq!(status, 400, "未知 target 应 400");
    let (status, _) = http(addr, "POST", "/api/reveal?target=skill&name=a%2Fb", None).await;
    assert_eq!(status, 400, "路径注入名应 400");
    let (status, _) = http(addr, "POST", "/api/reveal?target=skill&name=ghost-skill", None).await;
    assert_eq!(status, 404, "不存在的技能应 404");

    // 9. /files 服务：路径穿越 → 400；不存在 → 404；工作区内真实文件 → 200 且内容一致
    let (status, _) = http(addr, "GET", "/files/../config.json", None).await;
    assert_eq!(status, 400, "路径穿越应 400");
    let (status, _) = http(addr, "GET", "/files/definitely-missing.txt", None).await;
    assert_eq!(status, 404, "缺失文件应 404");
    let ws = react_agent_host::config::workspace_dir();
    let probe_rel = format!("target/artifact-e2e-{}.txt", nanos());
    let probe = ws.join(&probe_rel);
    std::fs::write(&probe, b"artifact-bytes").expect("write probe file");
    let (status, body) = http(addr, "GET", &format!("/files/{probe_rel}"), None).await;
    assert_eq!(status, 200, "产物文件应 200");
    assert_eq!(body, "artifact-bytes", "内容应一致");
    let _ = std::fs::remove_file(&probe);

    let _ = std::fs::remove_file(&cfg_file);
    let _ = std::fs::remove_dir_all(&skills_dir);
    for (k, v) in saved_keys {
        std::env::set_var(k, v);
    }
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
