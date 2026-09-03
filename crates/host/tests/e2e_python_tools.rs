//! e2e：Python guest（tools 插件）——spawn → dispatch → 断言 → 清理。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use serde_json::json;

#[tokio::test]
async fn tools_list_and_calculator_roundtrip() {
    let Some(py) = find_interpreter() else {
        skip("python not found");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }

    let kernel = fresh_kernel();
    let script = plugins_dir().join("tools").join("tools_plugin.py");
    let pp = spawn_python(py, &script, guest_manifest("tools", &["tools.exec"]), &[])
        .await
        .expect("spawn tools guest");
    register(&kernel, pp).await;

    // list
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("tools"), json!({"op": "list"})))
        .await
        .expect("dispatch tools.list");
    let names = expect_ok(&r, "tools.list")["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(names.contains(&"calculator".to_string()));

    // call: 1+2*3 → 7
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("tools"),
            json!({"op": "call", "name": "calculator", "args": {"expr": "1+2*3"}}),
        ))
        .await
        .expect("dispatch tools.call");
    assert_eq!(expect_ok(&r, "calculator")["result"], json!(7));

    // call: 未知工具 → ok:false
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("tools"), json!({"op": "call", "name": "nope", "args": {}})))
        .await
        .expect("dispatch tools.call(nope)");
    assert_eq!(r["ok"], json!(false));

    kernel.stop();
    kernel.destroy().await;
}
