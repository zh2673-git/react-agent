//! e2e：SYSTEM.md 提示词覆盖链 + 预置代码技能注册（assets 五）。
//!
//! 独立测试二进制：本测试会修改测试进程的 WORKSPACE_ROOT（agent-loop 为 InProcess，
//! 从进程 env 读 WORKSPACE_ROOT 解析 SYSTEM.md）——与同二进制内其他依赖「进程 env 无
//! WORKSPACE_ROOT」假设的测试存在竞态，故单列文件（cargo 串行运行各测试二进制）。

mod common;

use agent_kernel_sdk::{Envelope, PluginId};
use common::*;
use react_agent_agent_loop::new as agent_loop;
use serde_json::json;

/// assets 五（代码 profile）：WORKSPACE_ROOT/SYSTEM.md 覆盖链全局生效 + 新预置技能注册。
/// mock provider 经 MOCK_CAPTURE 落盘请求 → 断言 system 消息含 SYSTEM.md 内容；
/// assets 直查 skills.list / skills.load 验证 project-dev（编排）/ repo-explorer / tdd（含 tools 声明）。
#[tokio::test]
async fn system_md_overrides_prompt_and_preset_skills_register() {
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

    const MARKER: &str = "代码工程师覆盖链冒烟 MARKER-CODER-77";
    let ws_dir = std::env::temp_dir().join(format!("react-agent-sysmd-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&ws_dir).expect("create workspace");
    std::fs::write(ws_dir.join("SYSTEM.md"), format!("# {MARKER}\n探索优先，最小改动。\n")).expect("write SYSTEM.md");
    let mem_dir = std::env::temp_dir().join(format!("react-agent-sysmd-mem-{}-{}", std::process::id(), nanos()));
    let capture = std::env::temp_dir().join(format!("react-agent-sysmd-cap-{}-{}.jsonl", std::process::id(), nanos()));
    let _ = std::fs::remove_file(&capture);

    // agent-loop（InProcess）从测试进程 env 读 WORKSPACE_ROOT → SYSTEM.md 生效
    std::env::set_var("WORKSPACE_ROOT", &ws_dir);

    let kernel = fresh_kernel();
    let script = json!([
        {"ok": true, "content": "收敛", "tool_calls": [], "model": "mock", "finish_reason": "stop"},
    ]);

    let mem = spawn_node_ts(
        node,
        &plugins_dir().join("memory").join("memory_plugin.ts"),
        guest_manifest("memory", &["memory.session"]),
        &[("MEMORY_DATA_DIR", mem_dir.to_string_lossy().into())],
    )
    .await
    .expect("spawn memory");
    register(&kernel, mem).await;

    let llm = spawn_python(
        py,
        &plugins_dir().join("llm_adapter").join("llm_plugin.py"),
        guest_manifest("llm-adapter", &["llm.chat"]),
        &[
            ("LLM_PROVIDER", "mock".into()),
            ("MOCK_SCRIPT", script.to_string()),
            ("MOCK_CAPTURE", capture.to_string_lossy().into_owned()),
        ],
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

    let assets = spawn_python(
        py,
        &plugins_dir().join("assets").join("assets_plugin.py"),
        guest_manifest("assets", &["assets.registry"]),
        &[],
    )
    .await
    .expect("spawn assets");
    register(&kernel, assets).await;

    kernel.register(agent_loop(4)).await;

    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "e2e-sysmd", "user_text": "你好"}),
        ))
        .await
        .expect("dispatch chat");
    assert_eq!(r["ok"], json!(true), "sysmd loop: {r}");
    assert_eq!(r["answer"], json!("收敛"));

    // ① SYSTEM.md 覆盖链：捕获的请求中 system 消息含标记与正文（env > SYSTEM.md）
    let cap = std::fs::read_to_string(&capture).expect("capture written");
    let first_line = cap.lines().next().expect("one captured request");
    let req: serde_json::Value = serde_json::from_str(first_line).expect("capture is json");
    let sys = req["messages"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["role"] == json!("system")))
        .and_then(|m| m["content"].as_str())
        .expect("system message present")
        .to_string();
    assert!(sys.contains(MARKER), "system 未命中 SYSTEM.md 标记: {sys}");
    assert!(sys.contains("探索优先"), "system 含 SYSTEM.md 正文: {sys}");

    // ② 预置技能注册：repo-explorer（无工具）+ tdd（带 tools 声明）
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("assets"), json!({"op": "skills.list"})))
        .await
        .expect("dispatch skills.list");
    let skills = expect_ok(&r, "skills.list")["skills"].as_array().unwrap().clone();
    let find = |n: &str| skills.iter().find(|s| s["name"] == json!(n)).cloned();
    let explorer = find("repo-explorer").expect("repo-explorer registered");
    assert!(
        explorer["description"].as_str().map(|d| !d.is_empty()).unwrap_or(false),
        "repo-explorer description 非空: {explorer}"
    );
    let tdd = find("tdd").expect("tdd registered");
    assert_eq!(tdd["tools"], json!(true), "tdd 带 tools 声明标记: {tdd}");
    let proj = find("project-dev").expect("project-dev registered");
    assert!(
        proj["description"].as_str().map(|d| !d.is_empty()).unwrap_or(false),
        "project-dev description 非空: {proj}"
    );
    // Phase A：出厂件 origin=preset（git 管理预置件，删除保护的数据源）
    assert_eq!(proj["origin"], json!("preset"), "project-dev 为出厂件: {proj}");
    assert_eq!(tdd["origin"], json!("preset"), "tdd 为出厂件: {tdd}");
    assert_eq!(explorer["origin"], json!("preset"), "repo-explorer 为出厂件: {explorer}");

    // ③' skills.load(project-dev)：编排层全文回传，阶段路由须引用 repo-explorer / tdd
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("assets"),
            json!({"op": "skills.load", "name": "project-dev"}),
        ))
        .await
        .expect("dispatch skills.load project-dev");
    let loaded = expect_ok(&r, "skills.load project-dev");
    let content = loaded["content"].as_str().expect("project-dev SKILL.md content");
    assert!(
        content.contains("repo-explorer") && content.contains("tdd"),
        "project-dev 编排路由引用两技能: {content}"
    );

    // ③ skills.load(tdd)：回传 tools.json 绝对路径（无 missing），供 install 定点装载
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("assets"), json!({"op": "skills.load", "name": "tdd"})))
        .await
        .expect("dispatch skills.load tdd");
    let loaded = expect_ok(&r, "skills.load tdd");
    let manifest = loaded["tools_manifest"].as_object().expect("tools_manifest");
    let mpath = manifest["path"].as_str().expect("manifest path");
    assert!(mpath.ends_with("tdd\\tools.json") || mpath.ends_with("tdd/tools.json"), "manifest 指向 tdd: {mpath}");
    assert!(manifest.get("missing").is_none(), "声明文件齐全无 missing: {manifest:?}");
    let content = loaded["content"].as_str().expect("SKILL.md content");
    assert!(content.contains("run_tests"), "tdd SKILL.md 全文回传: {content}");

    std::env::remove_var("WORKSPACE_ROOT");
    let _ = std::fs::remove_dir_all(&ws_dir);
    let _ = std::fs::remove_dir_all(&mem_dir);
    let _ = std::fs::remove_file(&capture);
    kernel.stop();
    kernel.destroy().await;
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
