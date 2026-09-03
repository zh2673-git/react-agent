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
