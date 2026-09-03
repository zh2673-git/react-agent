//! e2e：Python guest（tools 插件）——生产级 7 件套：list / 文件四件套越界拦截 / bash /
//! 字段级错误 / TOOLS_ENABLED scope / web 形状（联网可用真搜，不可用降级断言）。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use serde_json::json;

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

fn tools_id() -> PluginId {
    PluginId::new("tools")
}

#[tokio::test]
async fn tools_list_files_and_guard_roundtrip() {
    let Some(_) = find_interpreter() else {
        skip("python not found");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }
    let ws = std::env::temp_dir().join(format!("react-tools-e2e-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws).unwrap();
    let kernel = spawn_tools(&[("WORKSPACE_ROOT", ws.to_string_lossy().into_owned())]).await;

    // list：默认 7 件
    let r = kernel.dispatch(Envelope::new(tools_id(), json!({"op": "list"}))).await.expect("list");
    let names: Vec<String> = expect_ok(&r, "tools.list")["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for n in ["read_file", "write_file", "edit_file", "list_dir", "bash", "web_search", "web_read"] {
        assert!(names.contains(&n.to_string()), "missing {n}: {names:?}");
    }

    // write → read（行号）
    let r = kernel
        .dispatch(Envelope::new(
            tools_id(),
            json!({"op": "call", "name": "write_file", "args": {"path": "a.txt", "content": "hello\nworld"}}),
        ))
        .await
        .expect("write");
    assert_eq!(expect_ok(&r, "write")["result"]["bytes"], json!(11));
    assert!(ws.join("a.txt").exists(), "文件落盘于 WORKSPACE_ROOT 内");

    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "call", "name": "read_file", "args": {"path": "a.txt"}})))
        .await
        .expect("read");
    let out = expect_ok(&r, "read")["result"].as_str().unwrap();
    assert!(out.contains("1\thello") && out.contains("2\tworld"), "{out}");

    // edit：多命中歧义 → 字段级错误带命中数
    let r = kernel
        .dispatch(Envelope::new(
            tools_id(),
            json!({"op": "call", "name": "edit_file", "args": {"path": "a.txt", "old_string": "l", "new_string": "L"}}),
        ))
        .await
        .expect("edit ambiguous");
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["error"]["code"], json!("EDIT_AMBIGUOUS"));
    assert_eq!(r["error"]["field"], json!("old_string"));

    // edit：replace_all 成功
    let r = kernel
        .dispatch(Envelope::new(
            tools_id(),
            json!({"op": "call", "name": "edit_file", "args": {"path": "a.txt", "old_string": "l", "new_string": "L", "replace_all": true}}),
        ))
        .await
        .expect("edit ok");
    assert_eq!(expect_ok(&r, "edit")["result"]["replacements"], json!(3));

    // 越界拦截（.. 与绝对路径）
    for bad in ["../escape.txt", "C:\\Windows\\evil.txt"] {
        let r = kernel
            .dispatch(Envelope::new(
                tools_id(),
                json!({"op": "call", "name": "write_file", "args": {"path": bad, "content": "x"}}),
            ))
            .await
            .expect("guard");
        assert_eq!(r["ok"], json!(false), "{bad}");
        assert_eq!(r["error"]["code"], json!("PATH_OUTSIDE_WORKSPACE"), "{bad}: {r}");
    }

    // bash：exit code + cwd 锚定
    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "call", "name": "bash", "args": {"command": "echo e2e-ok"}})))
        .await
        .expect("bash");
    let out = expect_ok(&r, "bash")["result"]["output"].as_str().unwrap();
    assert!(out.contains("e2e-ok"), "{out}");

    // 未知工具 → 列出可用
    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "call", "name": "calculator", "args": {}})))
        .await
        .expect("unknown tool");
    assert_eq!(r["error"]["code"], json!("UNKNOWN_TOOL"));

    let _ = std::fs::remove_dir_all(&ws);
    kernel.stop();
    kernel.destroy().await;
}

#[tokio::test]
async fn tools_enabled_scope_filters_list_and_call() {
    let Some(_) = find_interpreter() else {
        skip("python not found");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }
    let ws = std::env::temp_dir().join(format!("react-tools-scope-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws).unwrap();
    let kernel = spawn_tools(&[
        ("WORKSPACE_ROOT", ws.to_string_lossy().into_owned()),
        ("TOOLS_ENABLED", "read_file,bash".into()),
    ])
    .await;

    let r = kernel.dispatch(Envelope::new(tools_id(), json!({"op": "list"}))).await.expect("list");
    let names: Vec<String> = expect_ok(&r, "list")["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["read_file", "bash"], "未授权工具 Schema 不可见: {names:?}");

    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "call", "name": "write_file", "args": {"path": "x", "content": "x"}})))
        .await
        .expect("call disabled");
    assert_eq!(r["error"]["code"], json!("TOOL_DISABLED"), "{r}");

    let _ = std::fs::remove_dir_all(&ws);
    kernel.stop();
    kernel.destroy().await;
}

#[tokio::test]
async fn bash_via_sandbox_helper_executes_fail_closed() {
    let Some(_) = find_interpreter() else {
        skip("python not found");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }
    // 宿主层 fail-closed 助手（CARGO_BIN_EXE 由 cargo test 注入，同包编译产物）
    let helper = env!("CARGO_BIN_EXE_sandbox-run");

    // 助手直探：受限令牌真实执行 cmd /c exit 0
    let out = tokio::process::Command::new(helper)
        .arg("probe")
        .output()
        .await
        .expect("spawn sandbox-run probe");
    assert!(out.status.success(), "sandbox probe failed: {:?} {}", out.status, String::from_utf8_lossy(&out.stderr));

    // 经 tools 插件全链路：SANDBOX_HELPER 透传 → bash 走沙箱
    let ws = std::env::temp_dir().join(format!("react-sbx-e2e-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws).unwrap();
    let kernel = spawn_tools(&[
        ("WORKSPACE_ROOT", ws.to_string_lossy().into_owned()),
        ("SANDBOX_HELPER", helper.to_string()),
    ])
    .await;

    // 沙箱内 echo + 写文件落盘（受限令牌下用户可写区正常）
    let r = kernel
        .dispatch(Envelope::new(
            tools_id(),
            json!({"op": "call", "name": "bash", "args": {"command": "echo sbx-e2e-ok > s.txt & type s.txt"}}),
        ))
        .await
        .expect("sandboxed bash");
    let v = expect_ok(&r, "sandboxed bash");
    let result = &v["result"];
    assert_eq!(result["timeout"], json!(false), "{r}");
    assert!(result["output"].as_str().unwrap().contains("sbx-e2e-ok"), "{r}");
    assert!(ws.join("s.txt").exists(), "沙箱内写工作区应落盘");

    // 助手建立失败路径：SANDBOX_HELPER 指向不存在文件 → SANDBOX_FAILED（fail-closed，不回退直跑）
    kernel.stop();
    kernel.destroy().await;

    let kernel = spawn_tools(&[
        ("WORKSPACE_ROOT", ws.to_string_lossy().into_owned()),
        ("SANDBOX_HELPER", ws.join("no-such-helper.exe").to_string_lossy().into_owned()),
    ])
    .await;
    let r = kernel
        .dispatch(Envelope::new(tools_id(), json!({"op": "call", "name": "bash", "args": {"command": "echo x"}})))
        .await
        .expect("sandbox missing");
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["error"]["code"], json!("SANDBOX_FAILED"), "{r}");

    let _ = std::fs::remove_dir_all(&ws);
    kernel.stop();
    kernel.destroy().await;
}

#[tokio::test]
async fn web_search_shape_is_always_valid() {
    let Some(_) = find_interpreter() else {
        skip("python not found");
        return;
    };
    if !kernel_repo().join("bindings/python").join("agent_kernel").exists() {
        skip("kernel bindings/python not found");
        return;
    }
    let kernel = spawn_tools(&[]).await;
    let r = kernel
        .dispatch(Envelope::new(
            tools_id(),
            json!({"op": "call", "name": "web_search", "args": {"query": "rust programming", "max_results": 2}}),
        ))
        .await
        .expect("web_search");
    if r["ok"] == json!(true) {
        let results = r["result"]["results"].as_array().unwrap();
        assert!(!results.is_empty() && results.len() <= 2, "{r}");
        assert!(results.iter().all(|x| x["url"].as_str().is_some()), "结果必带来源 URL: {r}");
    } else {
        // 离线环境：全链失败时错误必带已试引擎列表（05 §7 P）
        assert_eq!(r["error"]["code"], json!("SEARCH_ALL_ENGINES_FAILED"), "{r}");
        assert!(r["error"]["message"].as_str().unwrap().contains("已试"));
    }
    kernel.stop();
    kernel.destroy().await;
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
