//! 纯 Rust 集成测试：用 InProcess mock 插件替代三个 guest，验证 ReAct 循环本身。
//! 不需要 python/node 环境。

use agent_kernel_kernel::Kernel;
use agent_kernel_sdk::*;
use async_trait::async_trait;
use react_agent_agent_loop::{new as agent_loop, ChatReq, MemoryMsg};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

// ---- mock 插件骨架 ----------------------------------------------------------

struct MockPlugin {
    manifest: Manifest,
    on: Arc<dyn Fn(&Envelope) -> Value + Send + Sync>,
}

impl MockPlugin {
    fn simple(name: &str, caps: &[&str], on: impl Fn(&Envelope) -> Value + Send + Sync + 'static) -> PluginInstance {
        Arc::new(Self {
            manifest: Manifest {
                name: PluginId::new(name),
                kind: PluginKind::Capability,
                version: Version::new(0, 1, 0),
                api_version: ApiVersion::new(1, 0),
                capabilities: caps.iter().map(|c| Capability::new(*c)).collect(),
                dependencies: vec![],
                domain: Domain::InProcess,
                semantics: Semantics::Serial,
                priority: 1,
                max_inflight: Some(8),
                fuel_limit: None,
                host_timeout_ms: None,
                epoch_interval_ms: None,
                subscriptions: vec![],
            },
            on: Arc::new(on),
        })
    }
}

#[async_trait]
impl Plugin for MockPlugin {
    fn id(&self) -> PluginId {
        self.manifest.name.clone()
    }
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    async fn init(&self, _ctx: &PluginContext) -> KernelResult<()> {
        Ok(())
    }
    async fn on_event(&self, env: Envelope) -> KernelResult<Value> {
        Ok((self.on)(&env))
    }
    fn destroy(&self) -> KernelResult<()> {
        Ok(())
    }
}

// ---- mock 组件 --------------------------------------------------------------

fn mock_memory(state: Arc<Mutex<Vec<MemoryMsg>>>) -> PluginInstance {
    MockPlugin::simple("memory", &["memory.session"], move |env| {
        let payload = &env.payload;
        match payload.get("op").and_then(|v| v.as_str()) {
            Some("get") => json!({"ok": true, "messages": state.lock().unwrap().clone()}),
            Some("append") => {
                let msgs: Vec<MemoryMsg> =
                    serde_json::from_value(payload.get("messages").cloned().unwrap_or(Value::Null)).unwrap_or_default();
                state.lock().unwrap().extend(msgs);
                json!({"ok": true})
            }
            _ => json!({"ok": false, "error": {"code": "K400", "message": "bad op"}}),
        }
    })
}

/// 脚本化 LLM：按序弹出响应；耗尽后复用最后一个。
fn mock_llm(script: Vec<Value>) -> PluginInstance {
    let seq = Arc::new(Mutex::new(script));
    MockPlugin::simple("llm-adapter", &["llm.chat"], move |_env| {
        let mut s = seq.lock().unwrap();
        if s.len() > 1 {
            s.remove(0)
        } else {
            s.first().cloned().unwrap_or_else(|| json!({"ok": false}))
        }
    })
}

fn mock_tools(result: Value) -> PluginInstance {
    MockPlugin::simple("tools", &["tools.exec"], move |env| match env.payload.get("op").and_then(|v| v.as_str()) {
        Some("list") => json!({"ok": true, "tools": [{"name": "calculator", "description": "math", "parameters": {}}]}),
        Some("call") => result.clone(),
        _ => json!({"ok": false}),
    })
}

/// assets mock：skills.list/load + prompts.get。
fn mock_assets(skill_content: &'static str) -> PluginInstance {
    MockPlugin::simple("assets", &["assets.registry"], move |env| {
        match env.payload.get("op").and_then(|v| v.as_str()) {
            Some("skills.list") => json!({"ok": true, "skills": [{"name": "demo", "description": "A demo skill."}]}),
            Some("skills.load") => json!({"ok": true, "content": skill_content}),
            Some("prompts.get") => json!({"ok": true, "content": "PROMPT_TEMPLATE"}),
            _ => json!({"ok": false, "error": {"code": "K400", "message": "bad op"}}),
        }
    })
}

/// 捕获发给 LLM 的 messages，供提示词组装/历史水位断言。
fn mock_llm_capturing(script: Vec<Value>, captured: Arc<Mutex<Vec<Vec<Value>>>>) -> PluginInstance {
    let seq = Arc::new(Mutex::new(script));
    MockPlugin::simple("llm-adapter", &["llm.chat"], move |env| {
        if let Some(msgs) = env.payload.get("messages").and_then(|m| m.as_array()) {
            captured.lock().unwrap().push(msgs.clone());
        }
        let mut s = seq.lock().unwrap();
        if s.len() > 1 {
            s.remove(0)
        } else {
            s.first().cloned().unwrap_or_else(|| json!({"ok": false}))
        }
    })
}

// ---- 测试工具 ---------------------------------------------------------------

async fn boot(plugins: Vec<PluginInstance>) -> Arc<Kernel> {
    let kernel = Kernel::new(GlobalConfig { node_id: "test".into(), max_total_inflight: 32 });
    for p in plugins {
        kernel.register(p).await;
    }
    kernel
}

async fn chat(kernel: &Kernel, user_text: &str) -> Value {
    kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "s1", "user_text": user_text}),
        ))
        .await
        .expect("dispatch chat")
}

fn tool_call_resp(calls: Value) -> Value {
    json!({"ok": true, "content": null, "tool_calls": calls, "model": "mock", "finish_reason": "tool_calls"})
}

// ---- 用例 -------------------------------------------------------------------

#[tokio::test]
async fn react_answers_without_tool_calls() {
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm(vec![json!({"ok": true, "content": "hello!", "tool_calls": [], "model": "mock", "finish_reason": "stop"})]),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "hi").await;
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["answer"], json!("hello!"));
    assert_eq!(r["rounds"], json!(1));
}

#[tokio::test]
async fn react_executes_tool_call_and_feeds_observation() {
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "calculator", "arguments": {"expr": "2+2"}}])),
        json!({"ok": true, "content": "answer is 4", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm(llm_script),
        mock_tools(json!({"ok": true, "result": 4})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "what is 2+2?").await;
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["answer"], json!("answer is 4"));
    assert_eq!(r["rounds"], json!(2));
}

#[tokio::test]
async fn react_reaches_max_rounds_and_returns_error_payload() {
    // LLM 每轮都要求调工具 → 轮次耗尽 → 最后一轮无工具仍要工具 → 错误 payload
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm(vec![tool_call_resp(json!([{"id": "c", "name": "calculator", "arguments": {}}]))]),
        mock_tools(json!({"ok": true, "result": 1})),
        agent_loop(2),
    ])
    .await;
    let r = chat(&kernel, "loop forever").await;
    assert_eq!(r["ok"], json!(false));
    assert!(r["error"]["message"].as_str().unwrap().contains("max_rounds"));
}

#[tokio::test]
async fn react_appends_history_and_final_to_memory() {
    let mem_state = Arc::new(Mutex::new(Vec::<MemoryMsg>::new()));
    let kernel = boot(vec![
        mock_memory(mem_state.clone()),
        mock_llm(vec![json!({"ok": true, "content": "done", "tool_calls": [], "model": "mock", "finish_reason": "stop"})]),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    chat(&kernel, "remember this").await;
    let mem = mem_state.lock().unwrap();
    // user + assistant(final) 至少两条
    assert_eq!(mem.first().map(|m| m.role.as_str()), Some("user"));
    let last = mem.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content.as_deref(), Some("done"));
}

#[tokio::test]
async fn react_tool_error_is_observed_not_fatal() {
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "calculator", "arguments": {"expr": "bad"}}])),
        json!({"ok": true, "content": "the tool failed with boom, but I recover", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm(llm_script),
        mock_tools(json!({"ok": false, "error": {"message": "boom"}})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "use broken tool").await;
    assert_eq!(r["ok"], json!(true));
    assert!(r["answer"].as_str().unwrap().contains("boom"));
}

#[tokio::test]
async fn task_delegates_and_denies_nesting() {
    // 捕获每次 plan 的 messages：子代理第 2 轮应收到「嵌套拒绝」的观察
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        // ① 父 r1：委派 task
        tool_call_resp(json!([{"id": "c1", "name": "task", "arguments": {"task": "研究子任务"}}])),
        // ② 子 r1：再嵌套 task（应被拒绝）
        tool_call_resp(json!([{"id": "c2", "name": "task", "arguments": {"task": "再嵌套"}}])),
        // ③ 子 r2：收敛
        json!({"ok": true, "content": "SUB-ANSWER", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
        // ④ 父 r2：收敛
        json!({"ok": true, "content": "PARENT-FINAL", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "start").await;
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("PARENT-FINAL"), "{r}");

    let calls = captured.lock().unwrap();
    // 调用顺序：父 r1 → 子 r1 → 子 r2 → 父 r2
    assert_eq!(calls.len(), 4, "llm 调用次数: {}", calls.len());
    // 子 r2 的最后一条消息应是嵌套拒绝的观察
    let sub_r2_last = calls[2].last().unwrap();
    assert_eq!(sub_r2_last["role"], json!("tool"), "子 r2 观察: {sub_r2_last}");
    assert!(
        sub_r2_last["content"].as_str().unwrap().contains("不支持嵌套委派"),
        "嵌套拒绝应回喂子代理: {sub_r2_last}"
    );
    // 父 r2 的观察应含子代理最终答案
    let parent_r2_last = calls[3].last().unwrap();
    assert!(
        parent_r2_last["content"].as_str().unwrap().contains("SUB-ANSWER"),
        "task 结果应含子代理答案: {parent_r2_last}"
    );
}

#[tokio::test]
async fn agent_loop_without_providers_is_not_dispatchable() {
    // 硬依赖无 provider ⇒ K302 ⇒ register 静默失败 ⇒ dispatch 报 UnknownPlugin
    let kernel = boot(vec![agent_loop(8)]).await;
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "s", "user_text": "x"}),
        ))
        .await;
    assert!(r.is_err(), "未满足硬依赖的插件不应可被 dispatch");
}

#[tokio::test]
async fn chat_req_parses_with_default_session() {
    let req: ChatReq = serde_json::from_value(json!({"op": "chat", "user_text": "x"})).unwrap();
    assert_eq!(req.session_id, "default");
}

// ---- Phase 1 新能力 ----------------------------------------------------------

#[tokio::test]
async fn load_skill_routes_to_assets_and_records_steps() {
    let mem_state = Arc::new(Mutex::new(Vec::<MemoryMsg>::new()));
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "load_skill", "arguments": {"name": "demo"}}])),
        json!({"ok": true, "content": "skill loaded, done", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(mem_state.clone()),
        mock_llm(llm_script),
        mock_tools(json!({"ok": true})),
        mock_assets("FULL SKILL BODY"),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "use the demo skill").await;
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("skill loaded, done"));

    // steps：round 递增、含保留名调用、ms 为正
    let steps = r["steps"].as_array().expect("steps present");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["round"], json!(1));
    assert_eq!(steps[0]["tool"], json!("load_skill"));
    assert!(steps[0]["ms"].as_u64().unwrap() < 60_000);

    // skill 全文经 tool 消息进入 memory（Execution 阶段的模型可见性来源）
    let mem = mem_state.lock().unwrap();
    assert!(mem.iter().any(|m| m.role == "tool" && m.content.as_deref().unwrap_or("").contains("FULL SKILL BODY")));
}

#[tokio::test]
async fn load_skill_failure_is_observed_not_fatal() {
    // 无 assets 注册（软依赖缺失）→ load_skill 合成 ok:false 回喂，循环不中断
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "load_skill", "arguments": {"name": "demo"}}])),
        json!({"ok": true, "content": "assets missing but I recover", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm(llm_script),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "use missing skill").await;
    assert_eq!(r["ok"], json!(true), "assets 缺失不得中断循环: {r}");
    assert_eq!(r["answer"], json!("assets missing but I recover"));
}

#[tokio::test]
async fn skills_catalog_is_injected_into_system_prompt() {
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![json!({"ok": true, "content": "done", "tool_calls": [], "model": "mock", "finish_reason": "stop"})];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        mock_assets("BODY"),
        agent_loop(8),
    ])
    .await;
    chat(&kernel, "hi").await;
    let cap = captured.lock().unwrap();
    let system = cap[0][0]["content"].as_str().unwrap();
    assert!(system.contains("## Available skills"), "{system}");
    assert!(system.contains("- demo: A demo skill."), "{system}");
    assert!(system.contains("load_skill"), "{system}");
}

#[tokio::test]
async fn system_prompt_env_overrides_builtin_and_history_limit_truncates() {
    // 本测试操作进程级 env，断言对并行用例无影响的键（AGENT_SYSTEM_PROMPT/HISTORY_LIMIT 不被其他用例检查）
    std::env::set_var("AGENT_SYSTEM_PROMPT", "CUSTOM PROMPT");
    std::env::set_var("HISTORY_LIMIT", "2");

    // 预置 4 条历史
    let mem_state = Arc::new(Mutex::new(vec![
        MemoryMsg { role: "user".into(), content: Some("old1".into()), tool_calls: None, tool_call_id: None },
        MemoryMsg { role: "assistant".into(), content: Some("old2".into()), tool_calls: None, tool_call_id: None },
        MemoryMsg { role: "user".into(), content: Some("old3".into()), tool_calls: None, tool_call_id: None },
        MemoryMsg { role: "assistant".into(), content: Some("old4".into()), tool_calls: None, tool_call_id: None },
    ]));
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![json!({"ok": true, "content": "done", "tool_calls": [], "model": "mock", "finish_reason": "stop"})];
    let kernel = boot(vec![
        mock_memory(mem_state),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "new question").await;
    assert_eq!(r["ok"], json!(true));

    let cap = captured.lock().unwrap();
    let msgs = &cap[0];
    // system = env 覆盖（无 assets → 无附录，但环境可能残留其它键——只断言前缀）
    assert!(msgs[0]["content"].as_str().unwrap().starts_with("CUSTOM PROMPT"), "{}", msgs[0]["content"]);
    // 历史水位：append（本轮 user）先于截断 → 5 条截为最近 2 条 = [old4, new]
    let non_system = &msgs[1..];
    assert_eq!(non_system.len(), 2, "{:?}", non_system);
    assert_eq!(non_system[0]["content"], json!("old4"));
    assert_eq!(non_system[1]["content"], json!("new question"));

    std::env::remove_var("AGENT_SYSTEM_PROMPT");
    std::env::remove_var("HISTORY_LIMIT");
}
