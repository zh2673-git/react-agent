//! e2e：MCP stdio 工具（tools PLAN §八）——第三池装载/命名空间/启用闸/调用/失败回喂/
//! 崩溃 server fail-closed。fixture = tests/fixtures/mcp_echo_server.py（回显 + boom +
//! RA_MCP_CRASH 崩溃模式）。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use serde_json::json;

fn tools_id() -> PluginId {
    PluginId::new("tools")
}

async fn spawn_tools(extra: &[(&str, String)]) -> std::sync::Arc<agent_kernel_kernel::Kernel> {
    let py = find_interpreter().expect("python");
    let kernel = fresh_kernel();
    let script = plugins_dir().join("tools").join("tools_plugin.py");
    let pp = spawn_python(py, &script, guest_manifest("tools", &["tools.exec"]), extra)
        .await
        .expect("spawn tools guest");
    register(&kernel, pp).await;
    kernel
}

fn mcp_servers_env(py: &str, server: &std::path::Path) -> String {
    json!({
        "echo": {"command": [py, server.to_string_lossy()]},
        "crasher": {"command": [py, server.to_string_lossy()], "env": {"RA_MCP_CRASH": "1"}},
    })
    .to_string()
}

#[tokio::test]
async fn mcp_pool_connect_enable_call_and_fail_closed() {
    let Some(py) = find_interpreter() else {
        skip("python not found");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }
    let server = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mcp_echo_server.py");
    let ws = std::env::temp_dir().join(format!("react-mcp-e2e-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws).unwrap();

    let kernel = spawn_tools(&[
        ("WORKSPACE_ROOT", ws.to_string_lossy().into_owned()),
        // 启用闸：内置 bash + 两件 MCP 工具显式启用；crasher 崩溃后其工具不存在于池
        ("TOOLS_ENABLED", "bash,mcp__echo__echo,mcp__echo__boom".into()),
        ("MCP_SERVERS", mcp_servers_env(py, &server)),
    ])
    .await;

    // Q1 list：启用的 MCP 工具常驻会话清单（agent-loop 清单注入契约，零改动验证）
    let r = kernel.dispatch(Envelope::new(tools_id(), json!({"op": "list"}))).await.expect("list");
    let names: Vec<String> = expect_ok(&r, "list")["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"mcp__echo__echo".to_string()), "echo 工具应在清单: {names:?}");
    assert!(names.contains(&"mcp__echo__boom".to_string()), "boom 工具应在清单: {names:?}");
    assert!(!names.iter().any(|n| n.starts_with("mcp__crasher__")), "崩溃 server 工具不入池: {names:?}");
    assert!(!names.contains(&"read_file".to_string()), "未启用内置不可见: {names:?}");

    // Q2 list all=true：附 enabled + mcp_server（host 配置视图字段）
    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "list", "all": true})))
        .await
        .expect("list all");
    let items = expect_ok(&r, "list all")["tools"].as_array().unwrap().clone();
    let echo = items.iter().find(|t| t["name"] == json!("mcp__echo__echo")).expect("echo entry");
    assert_eq!(echo["enabled"], json!(true), "{echo}");
    assert_eq!(echo["mcp_server"], json!("echo"), "{echo}");
    assert_eq!(echo["parameters"]["type"], json!("object"), "inputSchema 透传为 parameters: {echo}");

    // Q3 调用：content text 拼接回 {"ok":true,"result"} 契约形
    let r = kernel
        .dispatch(Envelope::new(
            tools_id(),
            json!({"op": "call", "name": "mcp__echo__echo", "args": {"text": "hello mcp"}}),
        ))
        .await
        .expect("call echo");
    assert_eq!(expect_ok(&r, "echo")["result"], json!("echo: hello mcp"), "{r}");

    // Q4 工具级失败：isError=true → 字段级错误回喂（不伪装成功）
    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "call", "name": "mcp__echo__boom", "args": {}})))
        .await
        .expect("call boom");
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["error"]["code"], json!("MCP_TOOL_ERROR"), "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("boom-failed"), "{r}");

    // I1 崩溃 server：fail-closed 跳过，调用立即 MCP_UNAVAILABLE（不挂起不阻断）
    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "call", "name": "mcp__crasher__echo", "args": {}})))
        .await
        .expect("call crashed server tool");
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["error"]["code"], json!("UNKNOWN_TOOL"), "未入池即未知工具: {r}");

    // Q5 mcp_tools 配置视图：servers 状态（echo ready / crasher failed 附 error）
    let r = kernel.dispatch(Envelope::new(tools_id(), json!({"op": "mcp_tools"}))).await.expect("mcp_tools");
    let v = expect_ok(&r, "mcp_tools");
    let servers = v["servers"].as_array().unwrap();
    let echo_srv = servers.iter().find(|s| s["name"] == json!("echo")).expect("echo server");
    assert_eq!(echo_srv["status"], json!("ready"), "{echo_srv}");
    assert_eq!(echo_srv["tools"], json!(2), "{echo_srv}");
    let crasher = servers.iter().find(|s| s["name"] == json!("crasher")).expect("crasher server");
    assert_eq!(crasher["status"], json!("failed"), "{crasher}");
    assert!(crasher["error"].as_str().map(|e| !e.is_empty()).unwrap_or(false), "{crasher}");
    let mt = v["tools"].as_array().unwrap();
    assert_eq!(mt.len(), 2, "只有 echo server 的工具入池: {mt:?}");
    assert!(mt.iter().all(|t| t["enabled"] == json!(true)), "{mt:?}");

    // Q6 启用闸：configure 收窄后 MCP 工具 TOOL_DISABLED（装载≠启用语义与内置一致）
    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "configure", "enabled": ["bash"]})))
        .await
        .expect("configure");
    assert_eq!(expect_ok(&r, "configure")["enabled"], json!(["bash"]), "{r}");
    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "call", "name": "mcp__echo__echo", "args": {"text": "x"}})))
        .await
        .expect("call disabled mcp");
    assert_eq!(r["error"]["code"], json!("TOOL_DISABLED"), "{r}");

    // I2 configure 拒绝未知/崩溃 server 工具名（字段级 400）
    let r = kernel
        .dispatch(Envelope::new(
            tools_id(),
            json!({"op": "configure", "enabled": ["bash", "mcp__crasher__echo"]}),
        ))
        .await
        .expect("configure unknown");
    assert_eq!(r["error"]["code"], json!("K400"), "{r}");
    assert_eq!(r["error"]["field"], json!("enabled"), "{r}");

    kernel.stop(); // destroy 路径：MCP server 逆序回收（幂等，不 panic 即过）
    kernel.destroy().await;
    let _ = std::fs::remove_dir_all(&ws);
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
