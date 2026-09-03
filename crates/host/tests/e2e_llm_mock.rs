//! e2e：Python llm-adapter（mock provider）——脚本化响应逐次弹出。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use serde_json::json;

#[tokio::test]
async fn llm_mock_echo_and_scripted_toolcall() {
    let Some(py) = find_interpreter() else {
        skip("python not found");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }

    let script = json!([
        {"ok": true, "content": "mock says hi", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
        {"ok": true, "content": null, "tool_calls": [{"id": "c9", "name": "bash", "arguments": {"command": "echo 2"}}], "model": "mock", "finish_reason": "tool_calls"},
    ]);

    let kernel = fresh_kernel();
    let plugin_path = plugins_dir().join("llm_adapter").join("llm_plugin.py");
    let pp = spawn_python(
        py,
        &plugin_path,
        guest_manifest("llm-adapter", &["llm.chat"]),
        &[("LLM_PROVIDER", "mock".into()), ("MOCK_SCRIPT", script.to_string())],
    )
    .await
    .expect("spawn llm-adapter guest");
    register(&kernel, pp).await;

    let target = PluginId::new("llm-adapter");

    // 第一次：脚本响应 1（纯文本）
    let r = kernel
        .dispatch(Envelope::new(
            target.clone(),
            json!({"op": "chat", "messages": [{"role": "user", "content": "ping"}]}),
        ))
        .await
        .expect("dispatch llm chat #1");
    let r = expect_ok(&r, "chat#1");
    assert_eq!(r["content"], json!("mock says hi"));
    assert_eq!(r["finish_reason"], json!("stop"));

    // 第二次：脚本响应 2（工具调用，arguments 已归一化为对象）
    let r = kernel
        .dispatch(Envelope::new(target, json!({"op": "chat", "messages": []})))
        .await
        .expect("dispatch llm chat #2");
    let r = expect_ok(&r, "chat#2");
    assert_eq!(r["finish_reason"], json!("tool_calls"));
    let calls = r["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["name"], json!("bash"));
    assert_eq!(calls[0]["arguments"]["command"], json!("echo 2"));

    kernel.stop();
    kernel.destroy().await;
}
