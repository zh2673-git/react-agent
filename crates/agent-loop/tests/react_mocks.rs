//! 纯 Rust 集成测试：用 InProcess mock 插件替代三个 guest，验证 ReAct 循环本身。
//! 不需要 python/node 环境。

use agent_kernel_kernel::Kernel;
use agent_kernel_sdk::*;
use async_trait::async_trait;
use react_agent_agent_loop::{new as agent_loop, ChatReq, MemoryMsg};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---- mock 插件骨架 ----------------------------------------------------------

type BoxFut = Pin<Box<dyn Future<Output = Value> + Send>>;

struct MockPlugin {
    manifest: Manifest,
    on: Arc<dyn Fn(&Envelope) -> BoxFut + Send + Sync>,
}

impl MockPlugin {
    fn simple(name: &str, caps: &[&str], on: impl Fn(&Envelope) -> Value + Send + Sync + 'static) -> PluginInstance {
        Self::plug(name, caps, Semantics::Serial, Arc::new(move |env| {
            let v = on(env);
            Box::pin(async move { v })
        }))
    }

    /// Concurrent 语义 + 异步处理器：验证并行行为的用例用（Serial 会被内核按插件串行化）。
    fn concurrent_async(
        name: &str,
        caps: &[&str],
        on: impl Fn(&Envelope) -> BoxFut + Send + Sync + 'static,
    ) -> PluginInstance {
        Self::plug(name, caps, Semantics::Concurrent, Arc::new(on))
    }

    fn plug(name: &str, caps: &[&str], semantics: Semantics, on: Arc<dyn Fn(&Envelope) -> BoxFut + Send + Sync>) -> PluginInstance {
        Arc::new(Self {
            manifest: Manifest {
                name: PluginId::new(name),
                kind: PluginKind::Capability,
                version: Version::new(0, 1, 0),
                api_version: ApiVersion::new(1, 0),
                capabilities: caps.iter().map(|c| Capability::new(*c)).collect(),
                dependencies: vec![],
                domain: Domain::InProcess,
                semantics,
                priority: 1,
                max_inflight: Some(8),
                fuel_limit: None,
                host_timeout_ms: None,
                epoch_interval_ms: None,
                subscriptions: vec![],
            },
            on,
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
        Ok((self.on)(&env).await)
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
            Some("summarize") => {
                // 与真实 memory 插件同构的水位效果：历史裁为最近 keep_last 条
                // （压缩标记消息由真实侧合成，mock 略去——测试只观测水位与触发与否）
                let keep = payload.get("keep_last").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut st = state.lock().unwrap();
                if keep > 0 && st.len() > keep {
                    let split = st.len() - keep;
                    *st = st.split_off(split);
                }
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

/// 操作进程级 env 的用例必须先拿这把锁（cargo test 默认多线程并行，
/// env 是进程全局的——COMPACT_*/HISTORY_LIMIT/AGENT_SYSTEM_PROMPT 互扰）。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
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
async fn concurrent_top_level_chats_both_can_delegate() {
    // P1 回归（PLAN R1）：修复前委派深度是插件级 AtomicU32 计数器，
    // 两个并发顶层 chat 使 depth=2 → task 被误判「嵌套委派」拒绝。
    // 修复后深度随链传播（ChatReq.depth），并发顶层会话互不挤占委派额度。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
    // LLM mock 按消息形态确定性应答（与并发交织顺序无关）：
    // 末条为 tool 观察 → 收敛；含任务标记 → 子代理收敛；否则 → 委派 task。
    let llm = MockPlugin::simple("llm-adapter", &["llm.chat"], move |env| {
        let msgs = env.payload.get("messages").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("");
        let joined: String = msgs.iter().filter_map(|m| m["content"].as_str()).collect::<Vec<_>>().join("\n");
        if last_role == "tool" {
            json!({"ok": true, "content": "got: SUB-DONE", "tool_calls": [], "model": "mock", "finish_reason": "stop"})
        } else if joined.contains("SUB-TASK-MARKER") {
            json!({"ok": true, "content": "SUB-DONE", "tool_calls": [], "model": "mock", "finish_reason": "stop"})
        } else {
            tool_call_resp(json!([{"id": "c1", "name": "task", "arguments": {"task": "do SUB-TASK-MARKER"}}]))
        }
    });
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        llm,
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;

    let fa = kernel.dispatch(Envelope::new(
        PluginId::new("agent-loop"),
        json!({"op": "chat", "session_id": "s-a", "user_text": "main-a"}),
    ));
    let fb = kernel.dispatch(Envelope::new(
        PluginId::new("agent-loop"),
        json!({"op": "chat", "session_id": "s-b", "user_text": "main-b"}),
    ));
    let (ra, rb) = tokio::join!(fa, fb);
    let ra = ra.expect("dispatch chat a");
    let rb = rb.expect("dispatch chat b");
    assert_eq!(ra["ok"], json!(true), "并发 chat A 应成功且能委派: {ra}");
    assert_eq!(rb["ok"], json!(true), "并发 chat B 应成功且能委派: {rb}");
    assert!(ra["answer"].as_str().unwrap().contains("SUB-DONE"), "A 的委派应拿到子代理答案: {ra}");
    assert!(rb["answer"].as_str().unwrap().contains("SUB-DONE"), "B 的委派应拿到子代理答案: {rb}");
}

#[tokio::test]
async fn task_denied_for_chain_depth_one() {
    // 深度随链传播的直接验证：depth=1 的 chat（即子代理链）内调 task → 字段级拒绝。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "task", "arguments": {"task": "嵌套尝试"}}])),
        json!({"ok": true, "content": "recovered after denial", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm(llm_script),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "chat", "session_id": "s", "user_text": "start", "depth": 1}),
        ))
        .await
        .expect("dispatch chat");
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("recovered after denial"), "{r}");
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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
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
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（AGENT_SYSTEM_PROMPT 缺省）
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
    let _env_guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("AGENT_SYSTEM_PROMPT", "CUSTOM PROMPT");
    std::env::set_var("HISTORY_LIMIT", "2");

    // 预置 4 条历史
    let mem_state = Arc::new(Mutex::new(vec![
        MemoryMsg { role: "user".into(), content: Some("old1".into()), tool_calls: None, tool_call_id: None, attachments: None },
        MemoryMsg { role: "assistant".into(), content: Some("old2".into()), tool_calls: None, tool_call_id: None, attachments: None },
        MemoryMsg { role: "user".into(), content: Some("old3".into()), tool_calls: None, tool_call_id: None, attachments: None },
        MemoryMsg { role: "assistant".into(), content: Some("old4".into()), tool_calls: None, tool_call_id: None, attachments: None },
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

#[tokio::test]
async fn oversized_tool_result_is_truncated_in_context() {
    // P3 回归（PLAN R2）：工具结果全文回喂会撑爆上下文。默认 TOOL_RESULT_LIMIT=8000，
    // 20K 字符的结果应被截断且模型能感知截断（带标记），截断发生在入 memory 之前。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（TOOL_RESULT_LIMIT=8000）
    let big = "x".repeat(20_000);
    let mem_state = Arc::new(Mutex::new(Vec::<MemoryMsg>::new()));
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "calculator", "arguments": {}}])),
        json!({"ok": true, "content": "done", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(mem_state.clone()),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true, "data": big})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "read big").await;
    assert_eq!(r["ok"], json!(true), "{r}");

    // 第二轮 LLM 上下文中的 tool 消息 = 截断后结果
    let cap = captured.lock().unwrap();
    let tool_msg = cap[1]
        .iter()
        .find(|m| m["role"] == json!("tool"))
        .expect("round 2 must contain tool observation");
    let content = tool_msg["content"].as_str().expect("tool content str");
    assert!(content.len() < 20_000, "tool result must be truncated: len={}", content.len());
    assert!(content.contains("truncated"), "model must be told about truncation: {content}");
    assert!(content.len() < 8_200, "should be near the 8000-char limit: len={}", content.len());

    // 截断发生在入 memory 之前（memory 即上下文来源，全文不入库）
    let mem = mem_state.lock().unwrap();
    let mem_tool = mem.iter().find(|m| m.role == "tool").expect("tool msg in memory");
    assert!(mem_tool.content.as_deref().unwrap_or("").contains("truncated"), "memory stores truncated result");
}

#[tokio::test]
async fn history_limit_does_not_block_compaction() {
    // P3 回归（PLAN R3）：修复前 HISTORY_LIMIT 截断发生在压缩判断之前，
    // LIMIT(3) < TRIGGER(4) 时压缩永不触发。修复后先按全量历史判断压缩，再裁窗口。
    // 本测试操作进程级 env——必须与其它 env 用例互斥（ENV_LOCK）。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = [
        ("COMPACT_TRIGGER", std::env::var("COMPACT_TRIGGER").ok()),
        ("COMPACT_KEEP", std::env::var("COMPACT_KEEP").ok()),
        ("HISTORY_LIMIT", std::env::var("HISTORY_LIMIT").ok()),
    ];
    std::env::set_var("COMPACT_TRIGGER", "4");
    std::env::set_var("COMPACT_KEEP", "2");
    std::env::set_var("HISTORY_LIMIT", "3");

    let mk = |role: &str, c: &str| MemoryMsg {
        role: role.into(),
        content: Some(c.into()),
        tool_calls: None,
        tool_call_id: None,
        attachments: None,
    };
    // 预置 6 条历史 > TRIGGER(4)：本轮 user 入库后共 7 条，压缩必应触发
    let mem_state = Arc::new(Mutex::new(vec![
        mk("user", "m1"),
        mk("assistant", "m2"),
        mk("user", "m3"),
        mk("assistant", "m4"),
        mk("user", "m5"),
        mk("assistant", "m6"),
    ]));
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        // ① 压缩摘要调用（无工具）
        json!({"ok": true, "content": "summary of old history", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
        // ② 正文调用收敛
        json!({"ok": true, "content": "final", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(mem_state.clone()),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "continue").await;
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("final"));

    // 压缩经 memory.summarize 落盘：水位 = keep_last(2)，随后 finish 追加最终 assistant 消息 → 3 条
    // （修复前不压缩：7 条历史 + final = 8 条）
    let mem = mem_state.lock().unwrap();
    assert_eq!(mem.len(), 3, "compaction must persist via memory.summarize: {mem:?}");
    assert_eq!(mem[0].content.as_deref(), Some("m6"), "kept last 2 before compaction");
    assert_eq!(mem[1].content.as_deref(), Some("continue"));
    assert_eq!(mem[2].content.as_deref(), Some("final"));
    drop(mem);

    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 2, "llm 调用 = 摘要 + 正文");
    // caps[0] 是压缩摘要调用
    assert!(
        cap[0][0]["content"].as_str().unwrap().contains("Summarize the conversation history"),
        "first llm call should be the summarizer: {}", cap[0][0]["content"]
    );
    // 正文上下文应含压缩标记（LIMIT=3 = marker 1 + keep 2，恰好保全标记）
    let body = &cap[1];
    assert!(
        body.iter().any(|m| m["content"].as_str().map(|c| c.contains("Context compaction")).unwrap_or(false)),
        "compaction marker must be in LLM context: {body:?}"
    );

    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}

#[tokio::test]
async fn batched_tool_calls_run_concurrently() {
    // P4 回归（PLAN T2）：同一轮的多个 tool_calls 应并发执行（串行实现下在途峰值恒为 1）。
    // tools mock 用 Concurrent 语义 + 异步睡眠：两个 150ms 睡眠若并行必然重叠 → 峰值 ≥ 2。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let (a, m) = (active.clone(), max_seen.clone());
    let tools = MockPlugin::concurrent_async("tools", &["tools.exec"], move |env| {
        let (a, m) = (a.clone(), m.clone());
        let payload = env.payload.clone(); // BoxFut 要求 'static：进 async 块前拿走所有权
        Box::pin(async move {
            match payload.get("op").and_then(|v| v.as_str()) {
                Some("list") => json!({"ok": true, "tools": [
                    {"name": "slow_a", "description": "slow", "parameters": {}},
                    {"name": "slow_b", "description": "slow", "parameters": {}}
                ]}),
                Some("call") => {
                    let cur = a.fetch_add(1, Ordering::SeqCst) + 1;
                    m.fetch_max(cur, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    a.fetch_sub(1, Ordering::SeqCst);
                    json!({"ok": true, "result": "slow-done"})
                }
                _ => json!({"ok": false}),
            }
        })
    });
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        tool_call_resp(json!([
            {"id": "c1", "name": "slow_a", "arguments": {}},
            {"id": "c2", "name": "slow_b", "arguments": {}}
        ])),
        json!({"ok": true, "content": "both done", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        tools,
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "run both").await;
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("both done"));

    // 并行证据：在途峰值 ≥ 2
    assert!(max_seen.load(Ordering::SeqCst) >= 2, "batch tools must run concurrently, max inflight={}", max_seen.load(Ordering::SeqCst));

    // 顺序与对应关系不变：steps 按声明顺序；回喂 tool 消息 tool_call_id 对应 c1, c2
    let steps = r["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["tool"], json!("slow_a"));
    assert_eq!(steps[1]["tool"], json!("slow_b"));
    let cap = captured.lock().unwrap();
    let tool_msgs: Vec<&Value> = cap[1].iter().filter(|x| x["role"] == json!("tool")).collect();
    assert_eq!(tool_msgs.len(), 2, "{:?}", cap[1]);
    assert_eq!(tool_msgs[0]["tool_call_id"], json!("c1"));
    assert_eq!(tool_msgs[1]["tool_call_id"], json!("c2"));
    assert!(tool_msgs[0]["content"].as_str().unwrap().contains("slow-done"));
    assert!(tool_msgs[1]["content"].as_str().unwrap().contains("slow-done"));
}

#[tokio::test]
async fn empty_answer_is_treated_as_failure() {
    // P6 回归（PLAN R4）：content None 且无 tool_calls 时 ok:true + answer:"" 是假收敛，
    // 应按错误 payload 收敛（修复前返回成功 + 空答案）。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
    let mem_state = Arc::new(Mutex::new(Vec::<MemoryMsg>::new()));
    let kernel = boot(vec![
        mock_memory(mem_state),
        mock_llm(vec![json!({"ok": true, "content": null, "tool_calls": [], "model": "mock", "finish_reason": "stop"})]),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "give me nothing").await;
    assert_eq!(r["ok"], json!(false), "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("空答案"), "{r}");
}

#[tokio::test]
async fn subagent_failure_error_message_is_slim() {
    // P6 回归（PLAN E2）：子代理失败消息只取 error.message，不再内嵌整个响应 JSON。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        // ① 父 r1：委派 task
        tool_call_resp(json!([{"id": "c1", "name": "task", "arguments": {"task": "会失败的任务"}}])),
        // ② 子 r1：LLM 直接失败
        json!({"ok": false, "error": {"code": "LLM_ERROR", "message": "provider down"}, "tool_calls": []}),
        // ③ 父 r2：收敛
        json!({"ok": true, "content": "I saw the failure", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "delegate").await;
    assert_eq!(r["ok"], json!(true), "{r}");

    // 父 r2 的 tool 观察应含瘦身后的人类可读原因，且无响应 JSON 残渣
    let cap = captured.lock().unwrap();
    let parent_r2_last = cap[2].last().unwrap();
    let content = parent_r2_last["content"].as_str().unwrap();
    assert!(content.contains("子代理失败: provider down"), "{content}");
    assert!(!content.contains("finish_reason"), "不得内嵌整个响应 JSON: {content}");
}

#[tokio::test]
async fn cancel_op_interrupts_loop_at_round_boundary() {
    // P2 回归（PLAN T1）：cancel op 置位标志 → 循环在轮次边界命中 → K499 收敛。
    // 确定性编排：首轮 plan 挂起等 release；驱动协程等「已进入 plan」→ dispatch cancel
    // （Concurrent 语义保证不被在途 chat 阻塞）→ 放行首轮 → 轮 2 开头检查命中。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
    use std::sync::atomic::AtomicBool;
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (en, rl) = (entered.clone(), release.clone());
    let llm = MockPlugin::concurrent_async("llm-adapter", &["llm.chat"], move |_env| {
        let (en, rl) = (en.clone(), rl.clone());
        Box::pin(async move {
            if !en.swap(true, Ordering::SeqCst) {
                // 首轮：挂起等放行（模拟慢 LLM）
                while !rl.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                tool_call_resp(json!([{"id": "c1", "name": "calculator", "arguments": {}}]))
            } else {
                tool_call_resp(json!([{"id": "c", "name": "calculator", "arguments": {}}]))
            }
        })
    });
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        llm,
        mock_tools(json!({"ok": true})),
        agent_loop(3),
    ])
    .await;

    let chat_fut = kernel.dispatch(Envelope::new(
        PluginId::new("agent-loop"),
        json!({"op": "chat", "session_id": "s1", "user_text": "long running"}),
    ));
    let k2 = kernel.clone();
    let driver = async move {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        k2.dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "cancel", "session_id": "s1"}),
        ))
        .await
        .expect("dispatch cancel");
        // 取消已被接受 → 放行首轮 plan
        release.store(true, Ordering::SeqCst);
    };
    let (chat_res, _) = tokio::join!(chat_fut, driver);
    let r = chat_res.expect("dispatch chat");
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["error"]["code"], json!("K499"), "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("取消"), "{r}");
}

#[tokio::test]
async fn stale_cancel_flag_does_not_kill_next_chat() {
    // P2 边界：cancel 到达于 chat 收敛之后 → 残留标志必须被开局清理，不误杀下一轮对话。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm(vec![json!({"ok": true, "content": "ok", "tool_calls": [], "model": "mock", "finish_reason": "stop"})]),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    // 无在途 chat 时先置位（模拟「取消晚到」）
    let r = kernel
        .dispatch(Envelope::new(
            PluginId::new("agent-loop"),
            json!({"op": "cancel", "session_id": "s1"}),
        ))
        .await
        .expect("dispatch cancel");
    assert_eq!(r["ok"], json!(true), "{r}");
    // 下一轮对话应正常收敛
    let r = chat(&kernel, "fresh start").await;
    assert_eq!(r["ok"], json!(true), "残留取消标志不得误杀新对话: {r}");
    assert_eq!(r["answer"], json!("ok"));
}

// ---- P5（PLAN T3/T4）--------------------------------------------------------

#[tokio::test]
async fn transient_llm_failure_is_retried_then_succeeds() {
    // T3 回归：限流类瞬态失败 → 指数退避重试后成功（修复前直接杀死整轮）。
    // base=0 免去真实退避等待；attempts=1 够用（一次瞬态）。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = [
        ("LLM_RETRY_BASE_MS", std::env::var("LLM_RETRY_BASE_MS").ok()),
        ("LLM_RETRY_ATTEMPTS", std::env::var("LLM_RETRY_ATTEMPTS").ok()),
    ];
    std::env::set_var("LLM_RETRY_BASE_MS", "0");
    std::env::set_var("LLM_RETRY_ATTEMPTS", "1");

    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        // ① 首次：瞬态失败（429 限流）
        json!({"ok": false, "error": {"code": "LLM_ERROR", "message": "HTTPStatusError: 429 Too Many Requests (rate limit)"}}),
        // ② 重试后成功
        json!({"ok": true, "content": "recovered after retry", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "flaky provider").await;
    assert_eq!(r["ok"], json!(true), "瞬态失败后重试应成功: {r}");
    assert_eq!(r["answer"], json!("recovered after retry"));
    assert_eq!(captured.lock().unwrap().len(), 2, "llm 应被调用 2 次（1 次失败 + 1 次重试）");

    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}

#[tokio::test]
async fn non_transient_llm_failure_not_retried() {
    // T3 反面：确定性失败（K400 参数/鉴权类）重试无益 → 立即失败，llm 只被调 1 次。
    let _env_guard = ENV_LOCK.lock().unwrap(); // 隐式依赖 env 缺省（token 闸禁用）
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![json!({"ok": false, "error": {"code": "K400", "message": "unknown provider: nope"}})];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "deterministic failure").await;
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["error"]["code"], json!("K400"), "{r}");
    assert_eq!(captured.lock().unwrap().len(), 1, "K400 不得重试");
}

#[tokio::test]
async fn token_budget_exhaustion_stops_loop() {
    // T4 回归：单次 chat 累计 token（input+output）超预算 → 轮次边界 K508 收敛。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var("CHAT_TOKEN_BUDGET").ok();
    std::env::set_var("CHAT_TOKEN_BUDGET", "10");

    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        // ① r1：要调工具，本调用即用掉 12 tokens（> 预算 10）
        json!({"ok": true, "content": null, "tool_calls": [{"id": "c1", "name": "calculator", "arguments": {}}], "model": "mock", "finish_reason": "tool_calls", "usage": {"input_tokens": 8, "output_tokens": 4}}),
        // ② r2（不应到达）
        json!({"ok": true, "content": "should not reach", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "burn tokens").await;
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["error"]["code"], json!("K508"), "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("token"), "{r}");
    assert_eq!(captured.lock().unwrap().len(), 1, "预算耗尽后不得再调 LLM");

    match saved {
        Some(v) => std::env::set_var("CHAT_TOKEN_BUDGET", v),
        None => std::env::remove_var("CHAT_TOKEN_BUDGET"),
    }
}

#[tokio::test]
async fn wall_clock_budget_expires_at_round_boundary() {
    // T4 回归：单次 chat 墙钟超预算 → 轮次边界 K508 收敛（时长超限）。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var("CHAT_BUDGET_SECS").ok();
    std::env::set_var("CHAT_BUDGET_SECS", "0.2"); // 200ms

    let calls = Arc::new(AtomicUsize::new(0));
    let (c1, c2) = (calls.clone(), calls.clone());
    let llm = MockPlugin::concurrent_async("llm-adapter", &["llm.chat"], move |_env| {
        let c = c1.clone();
        Box::pin(async move {
            let n = c.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // 首轮 LLM 慢于预算（400ms > 200ms）
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            tool_call_resp(json!([{"id": "c", "name": "calculator", "arguments": {}}]))
        })
    });
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        llm,
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "slow round").await;
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["error"]["code"], json!("K508"), "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("时长"), "{r}");
    assert_eq!(c2.load(Ordering::SeqCst), 1, "预算耗尽后不得再调 LLM");

    match saved {
        Some(v) => std::env::set_var("CHAT_BUDGET_SECS", v),
        None => std::env::remove_var("CHAT_BUDGET_SECS"),
    }
}

#[tokio::test]
async fn subagent_inherits_remaining_token_budget() {
    // T4 继承：子代理继承父链**衰减后**的剩余预算（修复前继承同预算无衰减）。
    // 父预算 100，r1 用掉 95 → 子代理只拿 5；子 r1 用 10 → 子链 K508 → 父观察失败原因。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var("CHAT_TOKEN_BUDGET").ok();
    std::env::set_var("CHAT_TOKEN_BUDGET", "100");

    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        // ① 父 r1：委派 task（本调用用掉 95）
        json!({"ok": true, "content": null, "tool_calls": [{"id": "c1", "name": "task", "arguments": {"task": "研究"}}], "model": "mock", "finish_reason": "tool_calls", "usage": {"input_tokens": 90, "output_tokens": 5}}),
        // ② 子 r1：要调工具（本调用用掉 10 > 继承的 5）
        json!({"ok": true, "content": null, "tool_calls": [{"id": "c2", "name": "calculator", "arguments": {}}], "model": "mock", "finish_reason": "tool_calls", "usage": {"input_tokens": 6, "output_tokens": 4}}),
        // ③ 父 r2：看到子代理预算耗尽，收敛
        json!({"ok": true, "content": "SEEN-BUDGET-FAIL", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "delegate with tiny budget").await;
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("SEEN-BUDGET-FAIL"));

    // 父 r2 的 tool 观察应含子代理的预算耗尽原因（说明子链确实被继承预算截停）
    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 3, "llm 调用 = 父r1 + 子r1 + 父r2: {}", cap.len());
    let parent_r2_last = cap[2].last().unwrap();
    let content = parent_r2_last["content"].as_str().unwrap();
    assert!(content.contains("子代理失败"), "{content}");
    assert!(content.contains("预算耗尽"), "子代理失败原因应说明是预算耗尽: {content}");

    match saved {
        Some(v) => std::env::set_var("CHAT_TOKEN_BUDGET", v),
        None => std::env::remove_var("CHAT_TOKEN_BUDGET"),
    }
}

// ---- P7/P8（PLAN R5-R8：上下文 token 闸 + 超限收紧/降级）----------------------

/// 预置 4 条 400 ASCII 字符的历史（估算 100 tokens/条）。
fn big_history() -> Arc<Mutex<Vec<MemoryMsg>>> {
    let mk = |role: &str, c: String| MemoryMsg { role: role.into(), content: Some(c), tool_calls: None, tool_call_id: None, attachments: None };
    let big = "x".repeat(400);
    Arc::new(Mutex::new(vec![
        mk("user", big.clone()),
        mk("assistant", big.clone()),
        mk("user", big.clone()),
        mk("assistant", big),
    ]))
}

#[tokio::test]
async fn token_gate_triggers_compaction() {
    // P7 回归（PLAN R6）：压缩双闸——token 闸（估算超 LLM_CONTEXT_TOKENS×0.7）独立于
    // 条数闸生效（修复前只有条数闸，单条大结果在 40 条之前就能撑爆窗口）。
    // 预算 301：压缩前历史估算 422 > 301 触发；压缩后工作集估算 ≈274 ≤ 301 不再收紧。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = [
        // L7：窗口值仅本地窗口型 provider 生效——闸类测试一律预置 LLM_PROVIDER=ollama
        ("LLM_PROVIDER", std::env::var("LLM_PROVIDER").ok()),
        ("LLM_CONTEXT_TOKENS", std::env::var("LLM_CONTEXT_TOKENS").ok()),
        ("COMPACT_TRIGGER", std::env::var("COMPACT_TRIGGER").ok()),
        ("COMPACT_KEEP", std::env::var("COMPACT_KEEP").ok()),
        ("HISTORY_LIMIT", std::env::var("HISTORY_LIMIT").ok()),
        ("AGENT_SYSTEM_PROMPT", std::env::var("AGENT_SYSTEM_PROMPT").ok()),
    ];
    std::env::set_var("LLM_PROVIDER", "ollama");
    std::env::set_var("LLM_CONTEXT_TOKENS", "430");
    std::env::set_var("COMPACT_TRIGGER", "100"); // 条数闸不触发（5 条历史 ≪ 100）
    std::env::set_var("COMPACT_KEEP", "2");
    std::env::remove_var("HISTORY_LIMIT");
    std::env::set_var("AGENT_SYSTEM_PROMPT", "SYS"); // 置短系统提示词以便精确估算

    let mem_state = big_history();
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        // ① 压缩摘要调用
        json!({"ok": true, "content": "summary of old history", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
        // ② 正文调用收敛
        json!({"ok": true, "content": "final", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(mem_state.clone()),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "continue").await;
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("final"));

    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 2, "llm 调用 = 摘要 + 正文: {}", cap.len());
    assert!(
        cap[0][0]["content"].as_str().unwrap().contains("Summarize the conversation history"),
        "首次调用应为压缩摘要: {}", cap[0][0]["content"]
    );
    assert!(
        cap[1].iter().any(|m| m["content"].as_str().map(|c| c.contains("Context compaction")).unwrap_or(false)),
        "token 闸触发的压缩标记应进入正文上下文: {:?}", cap[1]
    );
    let mem = mem_state.lock().unwrap();
    assert_eq!(mem.len(), 3, "压缩应经 memory.summarize 落盘: {mem:?}");

    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}

#[tokio::test]
async fn context_overflow_fails_before_request() {
    // P7 回归（PLAN R5）：发送前逐级收紧（窗口减半 → tool_result 限额减半）后仍超限
    // → CONTEXT_OVERFLOW，正文请求不发出（修复前原样发出 → provider 400 杀死整轮）。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = [
        // L7：窗口值仅本地窗口型 provider 生效——闸类测试一律预置 LLM_PROVIDER=ollama
        ("LLM_PROVIDER", std::env::var("LLM_PROVIDER").ok()),
        ("LLM_CONTEXT_TOKENS", std::env::var("LLM_CONTEXT_TOKENS").ok()),
        ("COMPACT_TRIGGER", std::env::var("COMPACT_TRIGGER").ok()),
        ("AGENT_SYSTEM_PROMPT", std::env::var("AGENT_SYSTEM_PROMPT").ok()),
        ("HISTORY_LIMIT", std::env::var("HISTORY_LIMIT").ok()),
        ("TOOL_RESULT_LIMIT", std::env::var("TOOL_RESULT_LIMIT").ok()),
    ];
    std::env::set_var("LLM_PROVIDER", "ollama");
    std::env::set_var("LLM_CONTEXT_TOKENS", "20"); // 预算 14：必然超限
    std::env::set_var("AGENT_SYSTEM_PROMPT", "SYS");
    std::env::remove_var("HISTORY_LIMIT");
    std::env::remove_var("TOOL_RESULT_LIMIT");

    let long = "y".repeat(2000); // 估算 504 tokens
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![json!({"ok": true, "content": "SHOULD-NOT-BE-USED", "tool_calls": [], "model": "mock", "finish_reason": "stop"})];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, &long).await;
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["error"]["code"], json!("CONTEXT_OVERFLOW"), "{r}");
    assert_eq!(r["error"]["ctx"]["budget"], json!(14), "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("LLM_CONTEXT_TOKENS"), "{r}");

    let cap = captured.lock().unwrap();
    // token 闸先触发压缩（摘要调用发生）；正文请求不得发出
    assert_eq!(cap.len(), 1, "仅压缩摘要调用，正文请求不得发出: {}", cap.len());
    assert!(cap[0][0]["content"].as_str().unwrap().contains("Summarize the conversation history"));

    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}

#[tokio::test]
async fn provider_overflow_degrades_window_and_recovers() {
    // P8 回归（PLAN R7/R8）：provider 侧 CONTEXT_OVERFLOW（估算闸漏网时的 provider 终审）
    // → 窗口/限额减半降级重试一次后成功（修复前直接杀死整轮）。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = [
        ("LLM_CONTEXT_TOKENS", std::env::var("LLM_CONTEXT_TOKENS").ok()),
        ("HISTORY_LIMIT", std::env::var("HISTORY_LIMIT").ok()),
        ("AGENT_SYSTEM_PROMPT", std::env::var("AGENT_SYSTEM_PROMPT").ok()),
        ("COMPACT_TRIGGER", std::env::var("COMPACT_TRIGGER").ok()),
    ];
    std::env::remove_var("LLM_CONTEXT_TOKENS"); // token 闸禁用：纯 provider 侧超限路径
    std::env::remove_var("HISTORY_LIMIT");
    std::env::set_var("AGENT_SYSTEM_PROMPT", "SYS");

    let mem_state = big_history();
    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        // ① 首次：provider 终审超限（归一化后的形状）
        json!({"ok": false, "error": {"code": "CONTEXT_OVERFLOW", "message": "ollama HTTP 400: request (4096 tokens) exceeds the available context size (4096 tokens)"}}),
        // ② 降级重试后收敛
        json!({"ok": true, "content": "recovered", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory(mem_state),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "go").await;
    assert_eq!(r["ok"], json!(true), "降级重试应成功: {r}");
    assert_eq!(r["answer"], json!("recovered"));

    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 2, "1 次超限 + 1 次降级重试: {}", cap.len());
    assert_eq!(cap[0].len(), 6, "首次发送 = system + 4 历史 + user: {}", cap[0].len());
    assert_eq!(cap[1].len(), 3, "降级后窗口减半（保 system 头）: {:?}", cap[1]);
    assert_eq!(cap[1][0]["role"], json!("system"), "system 头保留: {:?}", cap[1]);
    assert_eq!(cap[1][1]["content"], json!("x".repeat(400)), "保留最近一条历史: cap[1][1]");
    assert_eq!(cap[1][2]["content"], json!("go"), "user 消息保留: cap[1][2]");

    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}

#[tokio::test]
async fn provider_overflow_converges_after_one_degrade_retry() {
    // P8 收敛边界：降级重试一次后仍超限 → 原错误收敛，不进入重试风暴。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved = [
        ("LLM_CONTEXT_TOKENS", std::env::var("LLM_CONTEXT_TOKENS").ok()),
        ("AGENT_SYSTEM_PROMPT", std::env::var("AGENT_SYSTEM_PROMPT").ok()),
    ];
    std::env::remove_var("LLM_CONTEXT_TOKENS");
    std::env::set_var("AGENT_SYSTEM_PROMPT", "SYS");

    let captured = Arc::new(Mutex::new(Vec::<Vec<Value>>::new()));
    let llm_script = vec![
        json!({"ok": false, "error": {"code": "CONTEXT_OVERFLOW", "message": "HTTP 400: too long"}}),
        json!({"ok": false, "error": {"code": "CONTEXT_OVERFLOW", "message": "HTTP 400: still too long"}}),
    ];
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        mock_llm_capturing(llm_script, captured.clone()),
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "hopeless").await;
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["error"]["code"], json!("CONTEXT_OVERFLOW"), "{r}");
    assert_eq!(captured.lock().unwrap().len(), 2, "降级重试恰一次");

    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}

#[tokio::test]
async fn num_ctx_passthrough_follows_context_window() {
    // L2+L3 回归：`LLM_CONTEXT_TOKENS` 语义升级为「上下文窗口」——>0 时 llm.chat payload
    // 携带平级字段 num_ctx（ollama native 映射 options.num_ctx，本地估算闸与服务端窗口对齐）；
    // 0/缺省不携带（向后兼容）。L7：num_ctx 仅本地窗口型 provider 下发，故预置 ollama。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let saved_provider = std::env::var("LLM_PROVIDER").ok();
    let saved = std::env::var("LLM_CONTEXT_TOKENS").ok();
    std::env::set_var("LLM_PROVIDER", "ollama");

    let ok = json!({"ok": true, "content": "hi", "tool_calls": [], "model": "mock", "finish_reason": "stop"});
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    // 全 payload 捕获（num_ctx 与 messages 平级，mock_llm_capturing 只抓 messages）
    let cap2 = captured.clone();
    let llm = MockPlugin::simple("llm-adapter", &["llm.chat"], move |env| {
        cap2.lock().unwrap().push(env.payload.clone());
        ok.clone()
    });
    let kernel = boot(vec![
        mock_memory(Arc::new(Mutex::new(vec![]))),
        llm,
        mock_tools(json!({"ok": true})),
        agent_loop(8),
    ])
    .await;

    // 缺省（未设）→ 不携带
    std::env::remove_var("LLM_CONTEXT_TOKENS");
    let _ = chat(&kernel, "q1").await;
    assert!(
        captured.lock().unwrap().last().unwrap().get("num_ctx").is_none(),
        "缺省不下发 num_ctx: {:?}",
        captured.lock().unwrap().last().unwrap()
    );

    // 窗口 4096 → num_ctx=4096
    std::env::set_var("LLM_CONTEXT_TOKENS", "4096");
    let _ = chat(&kernel, "q2").await;
    assert_eq!(captured.lock().unwrap().last().unwrap()["num_ctx"], json!(4096));

    // 窗口 0（禁用）→ 不携带
    std::env::set_var("LLM_CONTEXT_TOKENS", "0");
    let _ = chat(&kernel, "q3").await;
    assert!(
        captured.lock().unwrap().last().unwrap().get("num_ctx").is_none(),
        "0=禁用不下发 num_ctx"
    );

    match saved {
        Some(v) => std::env::set_var("LLM_CONTEXT_TOKENS", v),
        None => std::env::remove_var("LLM_CONTEXT_TOKENS"),
    }
    match saved_provider {
        Some(v) => std::env::set_var("LLM_PROVIDER", v),
        None => std::env::remove_var("LLM_PROVIDER"),
    }
}

// ---- R9（agent 自造技能闭环：skill_install / skill_loaded / 会话技能工具）------

/// memory mock + trace 事件捕获/回放：断言 skill_installed/skill_loaded 事件，
/// 或预置 skill_loaded 事件验证会话技能集重放推导。
fn mock_memory_traced(state: Arc<Mutex<Vec<MemoryMsg>>>, events: Arc<Mutex<Vec<Value>>>) -> PluginInstance {
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
            Some("summarize") => {
                let keep = payload.get("keep_last").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut st = state.lock().unwrap();
                if keep > 0 && st.len() > keep {
                    let split = st.len() - keep;
                    *st = st.split_off(split);
                }
                json!({"ok": true})
            }
            Some("trace.append") => {
                let evs: Vec<Value> =
                    serde_json::from_value(payload.get("events").cloned().unwrap_or(Value::Null)).unwrap_or_default();
                let n = evs.len();
                events.lock().unwrap().extend(evs);
                json!({"ok": true, "count": n})
            }
            Some("trace.read") => json!({"ok": true, "events": events.lock().unwrap().clone()}),
            _ => json!({"ok": false, "error": {"code": "K400", "message": "bad op"}}),
        }
    })
}

/// tools mock：捕获 install payload（断言路由参数）+ 固定 skill_tools 应答 + 技能工具调用。
fn mock_tools_r9(install_log: Arc<Mutex<Vec<Value>>>, skill_tool: Value) -> PluginInstance {
    MockPlugin::simple("tools", &["tools.exec"], move |env| {
        let payload = env.payload.clone();
        match payload.get("op").and_then(|v| v.as_str()) {
            Some("list") => json!({"ok": true, "tools": [{"name": "calculator", "description": "math", "parameters": {}}]}),
            Some("install") => {
                install_log.lock().unwrap().push(payload.clone());
                json!({"ok": true, "skill": payload["skill"], "loaded": ["t1"], "skipped": [], "pending": ["t1"]})
            }
            Some("skill_tools") => json!({"ok": true, "tools": [skill_tool.clone()]}),
            Some("call") => {
                if payload["name"] == json!("skill_echo") {
                    json!({"ok": true, "result": "SKILL-ECHO-RESULT"})
                } else {
                    json!({"ok": true, "result": 1})
                }
            }
            _ => json!({"ok": false}),
        }
    })
}

#[tokio::test]
async fn skill_install_full_chain_traces_and_reports() {
    // Q：skill_install 全链路——assets.load 取声明 → tools.install（装载≠启用）→
    // skill_installed 事件 → 观察回写含 loaded/pending。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let mem_state = Arc::new(Mutex::new(Vec::<MemoryMsg>::new()));
    let install_log = Arc::new(Mutex::new(Vec::<Value>::new()));
    let assets = MockPlugin::simple("assets", &["assets.registry"], move |env| {
        match env.payload.get("op").and_then(|v| v.as_str()) {
            Some("skills.load") => json!({
                "ok": true, "content": "BODY",
                "tools_manifest": {"path": "C:/ws/skills/demo/tools.json"}
            }),
            _ => json!({"ok": false}),
        }
    });
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "skill_install", "arguments": {"name": "demo"}}])),
        json!({"ok": true, "content": "installed demo", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory_traced(mem_state.clone(), events.clone()),
        mock_llm(llm_script),
        mock_tools_r9(install_log.clone(), json!({"name": "skill_echo", "description": "d", "parameters": {}})),
        assets,
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "install my skill").await;
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("installed demo"));

    // tools.install 被定点调用：path 来自声明、skill 显式传注册名
    let installs = install_log.lock().unwrap();
    assert_eq!(installs.len(), 1, "install 恰调用一次: {installs:?}");
    assert_eq!(installs[0]["path"], json!("C:/ws/skills/demo/tools.json"));
    assert_eq!(installs[0]["skill"], json!("demo"));
    drop(installs);

    // skill_installed 事件落 trace（前端内联卡与一键启用的依据）
    let evs = events.lock().unwrap();
    assert!(
        evs.iter().any(|e| e["type"] == json!("skill_installed")
            && e["skill"] == json!("demo")
            && e["tools_loaded"] == json!(["t1"])
            && e["tools_pending"] == json!(["t1"])),
        "skill_installed 事件缺失或形状不对: {evs:?}"
    );
    drop(evs);

    // 观察回写：loaded/pending 明细 + 「装载≠启用」提示（部分成功不算整体失败）
    let mem = mem_state.lock().unwrap();
    assert!(
        mem.iter().any(|m| m.role == "tool"
            && m.content.as_deref().map(|c| c.contains("t1") && c.contains("注册")).unwrap_or(false)),
        "观察回写应含 loaded/pending 与注册提示: {mem:?}"
    );
}

#[tokio::test]
async fn skill_install_without_tools_declaration_still_registers() {
    // 无 tools 声明的技能包：仅注册（tools.install 不调用），事件仍发（tools_* 为空）。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let install_log = Arc::new(Mutex::new(Vec::<Value>::new()));
    let assets = MockPlugin::simple("assets", &["assets.registry"], move |env| {
        match env.payload.get("op").and_then(|v| v.as_str()) {
            Some("skills.load") => json!({"ok": true, "content": "BODY"}),
            _ => json!({"ok": false}),
        }
    });
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "skill_install", "arguments": {"name": "demo"}}])),
        json!({"ok": true, "content": "registered", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory_traced(Arc::new(Mutex::new(vec![])), events.clone()),
        mock_llm(llm_script),
        mock_tools_r9(install_log.clone(), json!({"name": "x", "parameters": {}})),
        assets,
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "register plain skill").await;
    assert_eq!(r["ok"], json!(true), "{r}");

    assert!(install_log.lock().unwrap().is_empty(), "无声明不得触发 tools.install");
    let evs = events.lock().unwrap();
    assert!(
        evs.iter()
            .any(|e| e["type"] == json!("skill_installed") && e["skill"] == json!("demo") && e["tools_loaded"] == json!([])),
        "无声明技能也须发 skill_installed（tools_* 空）: {evs:?}"
    );
}

#[tokio::test]
async fn skill_install_unknown_skill_is_observed_not_fatal() {
    // I：unknown skill 错误 payload 原样回喂观察（部分失败不算整体失败），循环不中断。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let mem_state = Arc::new(Mutex::new(Vec::<MemoryMsg>::new()));
    let install_log = Arc::new(Mutex::new(Vec::<Value>::new()));
    let assets = MockPlugin::simple("assets", &["assets.registry"], move |env| {
        match env.payload.get("op").and_then(|v| v.as_str()) {
            Some("skills.load") => json!({"ok": false, "error": {"code": "UNKNOWN_SKILL", "message": "unknown skill: nope"}}),
            _ => json!({"ok": false}),
        }
    });
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "skill_install", "arguments": {"name": "nope"}}])),
        json!({"ok": true, "content": "I saw the error", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let kernel = boot(vec![
        mock_memory_traced(mem_state.clone(), events.clone()),
        mock_llm(llm_script),
        mock_tools_r9(install_log.clone(), json!({"name": "x", "parameters": {}})),
        assets,
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "install missing").await;
    assert_eq!(r["ok"], json!(true), "安装失败不得中断循环: {r}");
    assert_eq!(r["answer"], json!("I saw the error"));

    assert!(install_log.lock().unwrap().is_empty(), "load 失败不得触发 tools.install");
    assert!(
        !events.lock().unwrap().iter().any(|e| e["type"] == json!("skill_installed")),
        "load 失败不得发 skill_installed 事件"
    );
    let mem = mem_state.lock().unwrap();
    assert!(
        mem.iter().any(|m| m.role == "tool" && m.content.as_deref().map(|c| c.contains("UNKNOWN_SKILL")).unwrap_or(false)),
        "错误明细应回喂观察: {mem:?}"
    );
}

#[tokio::test]
async fn load_skill_merges_enabled_skill_tools_into_next_rounds() {
    // 清单组装规则：r1 load_skill 成功 → skill_loaded 事件 + 该技能已启用工具并入 r2 清单
    //（r1 清单不含技能工具——会话可见性由 load_skill 驱动）。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let payload_caps = Arc::new(Mutex::new(Vec::<Value>::new()));
    let llm_script = vec![
        tool_call_resp(json!([{"id": "c1", "name": "load_skill", "arguments": {"name": "demo"}}])),
        tool_call_resp(json!([{"id": "c2", "name": "skill_echo", "arguments": {"x": 1}}])),
        json!({"ok": true, "content": "final", "tool_calls": [], "model": "mock", "finish_reason": "stop"}),
    ];
    let script = Arc::new(Mutex::new(llm_script));
    let cap2 = payload_caps.clone();
    let llm = MockPlugin::simple("llm-adapter", &["llm.chat"], move |env| {
        cap2.lock().unwrap().push(env.payload.clone());
        let mut s = script.lock().unwrap();
        if s.len() > 1 {
            s.remove(0)
        } else {
            s.first().cloned().unwrap()
        }
    });
    let kernel = boot(vec![
        mock_memory_traced(Arc::new(Mutex::new(vec![])), events.clone()),
        llm,
        mock_tools_r9(Arc::new(Mutex::new(vec![])), json!({"name": "skill_echo", "description": "d", "parameters": {}})),
        mock_assets("BODY"),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "load and use").await;
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["answer"], json!("final"));

    assert!(
        events.lock().unwrap().iter().any(|e| e["type"] == json!("skill_loaded") && e["skill"] == json!("demo")),
        "load_skill 成功须发 skill_loaded 事件"
    );

    let caps = payload_caps.lock().unwrap();
    assert_eq!(caps.len(), 3, "llm 调用 = r1+r2+r3: {}", caps.len());
    let has_echo = |i: usize| {
        caps[i]["tools"]
            .as_array()
            .map(|ts| ts.iter().any(|t| t["name"] == json!("skill_echo")))
            .unwrap_or(false)
    };
    assert!(!has_echo(0), "r1 清单不得含技能工具: {:?}", caps[0]["tools"]);
    assert!(has_echo(1), "load_skill 成功后 r2 清单应并入技能工具: {:?}", caps[1]["tools"]);
    // r3 上下文含技能工具执行结果（保留名外技能工具走 tools.call 正常分发）
    assert!(
        caps[2]["messages"].as_array().unwrap().iter().any(|m| m["role"] == json!("tool")
            && m["content"].as_str().map(|c| c.contains("SKILL-ECHO-RESULT")).unwrap_or(false)),
        "技能工具结果应回喂: {:?}",
        caps[2]["messages"]
    );
}

#[tokio::test]
async fn session_skill_tools_replayed_from_trace_at_chat_start() {
    // 会话技能集从 trace 重放推导：预置 skill_loaded 事件 → 新对话开局清单即含已启用技能工具
    //（同会话技能作用域经重放恢复；子代理新会话不继承）。
    let _env_guard = ENV_LOCK.lock().unwrap();
    let events = Arc::new(Mutex::new(vec![json!({"type": "skill_loaded", "skill": "demo", "ts": 1})]));
    let payload_caps = Arc::new(Mutex::new(Vec::<Value>::new()));
    let ok = json!({"ok": true, "content": "hi", "tool_calls": [], "model": "mock", "finish_reason": "stop"});
    let cap2 = payload_caps.clone();
    let llm = MockPlugin::simple("llm-adapter", &["llm.chat"], move |env| {
        cap2.lock().unwrap().push(env.payload.clone());
        ok.clone()
    });
    let kernel = boot(vec![
        mock_memory_traced(Arc::new(Mutex::new(vec![])), events),
        llm,
        mock_tools_r9(Arc::new(Mutex::new(vec![])), json!({"name": "skill_echo", "description": "d", "parameters": {}})),
        mock_assets("BODY"),
        agent_loop(8),
    ])
    .await;
    let r = chat(&kernel, "fresh turn").await;
    assert_eq!(r["ok"], json!(true), "{r}");

    let caps = payload_caps.lock().unwrap();
    let tools = caps[0]["tools"].as_array().expect("tools present");
    assert!(
        tools.iter().any(|t| t["name"] == json!("skill_echo")),
        "开局清单应含 trace 重放出的技能工具: {tools:?}"
    );
    assert!(
        tools.iter().any(|t| t["name"] == json!("calculator")),
        "内置工具仍在: {tools:?}"
    );
}
