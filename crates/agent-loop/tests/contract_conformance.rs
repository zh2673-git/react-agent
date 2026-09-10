//! R14：居民特有契约的机械化验收（第三轮审计 Q1）。
//!
//! 冻结 agent-loop 对外/对内的"暗契约"，任何重构/替换跑本文件即知是否破坏契约：
//! ① 事件 schema：type 全集 + 必填字段（前端按 type 渲染，未知类型静默忽略——漏发即瞎）；
//! ② sid 形状 `{session}-r{digits}`（llm-adapter `_SID_RE` 反解 / 前端 doneSids 去重依赖）；
//! ③ 旁路路径规则：`.stream/{session}.jsonl`（与 host `config.rs::stream_file` 必须一致）；
//! ④ 保留名路由：`task` 下发模型可见，`load_skill`/`skill_install` 不进 tools 清单；
//! ⑤ undo 透传：tools 结果中的 `undo` 引用原样进 `file_change` 事件（回滚链路依赖）；
//! ⑥ 观测登记：write_file→artifact（产物卡）、web_search→sources（溯源卡）、bash changes[]→file_change；
//! ⑦ memory op 面：agent-loop 只会调用 {get, append, summarize, trace.append, trace.read}；
//! ⑧ llm.chat payload 面：messages + tools + stream_path + sid；
//! ⑨ tools.exec payload 面：{op, name, args}。
//!
//! 密钥脱敏经 `secrets.rs` 单测覆盖（静态缓存与 env 测试互斥，不在本文件重复）。
//! env 键面（⑩）将由 E3 AgentLoopConfig 单点化后自然收敛，此处不再冻结。

use agent_kernel_sdk::{
    ApiVersion, Capability, Domain, Envelope, KernelResult, Manifest, Plugin, PluginContext, PluginInstance,
    PluginId, PluginKind, Semantics, Version,
};
use async_trait::async_trait;
use react_agent_agent_loop::new as agent_loop_new;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

// ---- mock 骨架（捕获式） -----------------------------------------------------

type BoxFut = std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>;

struct MockPlugin {
    manifest: Manifest,
    on: Arc<dyn Fn(&Envelope) -> BoxFut + Send + Sync>,
}

impl MockPlugin {
    fn plug(name: &str, caps: &[&str], on: Arc<dyn Fn(&Envelope) -> BoxFut + Send + Sync>) -> PluginInstance {
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

// ---- 场景 -------------------------------------------------------------------

/// 冻结的 trace 事件 type 全集（新增类型必须先改这里 + 前端 render()，漏发即前端瞎）。
const FROZEN_EVENT_TYPES: &[&str] = &[
    "user", "assistant", "tool_call", "tool_result", "artifact", "sources", "file_change", "compaction",
    "subagent", "skill_loaded", "skill_installed", "error", "retry",
];

#[tokio::test]
async fn scripted_chat_fulfills_all_wire_contracts() {
    // ③ 旁路路径契约依赖 AGENT_STREAM_DIR（host 启动时下发；测试自设临时目录）
    let stream_dir = std::env::temp_dir().join("agent-loop-contract-stream");
    std::fs::create_dir_all(&stream_dir).unwrap();
    std::env::set_var("AGENT_STREAM_DIR", &stream_dir);

    let ops_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let trace_events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let llm_payloads: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let tool_calls_in: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let llm_traces: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));

    // memory：记录 op 面 + 捕获 trace 事件 + 维护消息存储
    let memory = {
        let ops = ops_log.clone();
        let events = trace_events.clone();
        let store: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
        let store2 = store.clone();
        MockPlugin::plug(
            "memory",
            &["memory.session"],
            Arc::new(move |env: &Envelope| {
                let ops = ops.clone();
                let events = events.clone();
                let store = store2.clone();
                let payload = env.payload.clone();
                Box::pin(async move {
                    ops.lock().unwrap().push(payload.get("op").and_then(Value::as_str).unwrap_or("").to_string());
                    match payload.get("op").and_then(Value::as_str) {
                        Some("get") => json!({"ok": true, "messages": store.lock().unwrap().clone()}),
                        Some("append") => {
                            if let Some(msgs) = payload.get("messages").and_then(Value::as_array) {
                                store.lock().unwrap().extend(msgs.iter().cloned());
                            }
                            json!({"ok": true})
                        }
                        Some("trace.append") => {
                            if let Some(evs) = payload.get("events").and_then(Value::as_array) {
                                events.lock().unwrap().extend(evs.iter().cloned());
                            }
                            json!({"ok": true})
                        }
                        Some("trace.read") => json!({"ok": true, "events": events.lock().unwrap().clone()}),
                        _ => json!({"ok": false, "error": {"code": "K400", "message": "bad op"}}),
                    }
                })
            }),
        )
    };

    // llm：两轮脚本（① 要两个工具 ② 最终答案），捕获完整 payload + 调用链 trace
    let llm = {
        let payloads = llm_payloads.clone();
        let traces = llm_traces.clone();
        let seq = Arc::new(Mutex::new(vec![
            json!({"ok": true, "content": null, "tool_calls": [
                {"id": "c1", "name": "write_file", "arguments": {"path": "outputs/report.md", "content": "hi"}},
                {"id": "c2", "name": "web_search", "arguments": {"query": "q"}}
            ], "model": "mock", "finish_reason": "tool_calls"}),
            json!({"ok": true, "content": "final answer", "tool_calls": [], "model": "mock", "finish_reason": "stop",
                   "usage": {"input_tokens": 10, "output_tokens": 5}}),
        ]));
        MockPlugin::plug(
            "llm-adapter",
            &["llm.chat"],
            Arc::new(move |env: &Envelope| {
                let payloads = payloads.clone();
                let traces = traces.clone();
                let seq = seq.clone();
                let payload = env.payload.clone();
                let tid = env.trace_id.to_string();
                Box::pin(async move {
                    traces.lock().unwrap().push(tid);
                    payloads.lock().unwrap().push(payload);
                    let mut s = seq.lock().unwrap();
                    if s.len() > 1 {
                        s.remove(0)
                    } else {
                        s[0].clone()
                    }
                })
            }),
        )
    };

    // tools：write_file（带 undo）/ web_search（带结果）
    let tools = {
        let calls_in = tool_calls_in.clone();
        MockPlugin::plug(
            "tools",
            &["tools.exec"],
            Arc::new(move |env: &Envelope| {
                let calls_in = calls_in.clone();
                let payload = env.payload.clone();
                Box::pin(async move {
                    calls_in.lock().unwrap().push(payload.clone());
                    match (payload.get("op").and_then(Value::as_str), payload.get("name").and_then(Value::as_str)) {
                        (Some("list"), _) => json!({"ok": true, "tools": [{"name": "calculator", "description": "math", "parameters": {}}]}),
                        (Some("call"), Some("write_file")) => json!({"ok": true, "result": {
                            "path": "outputs/report.md", "bytes": 10,
                            "undo": {"id": "u1", "created": [], "deleted": []}}}),
                        (Some("call"), Some("web_search")) => json!({"ok": true, "result": {
                            "results": [{"title": "T", "url": "https://e.example/1"}]}}),
                        _ => json!({"ok": false, "error": {"code": "K400", "message": "bad call"}}),
                    }
                })
            }),
        )
    };

    // 装配（assets 不注册 = 软依赖降级路径也在契约覆盖内）
    let kernel = agent_kernel_kernel::Kernel::new(agent_kernel_sdk::GlobalConfig {
        node_id: "contract-test".into(),
        max_total_inflight: 16,
    });
    kernel.register(memory).await;
    kernel.register(llm).await;
    kernel.register(tools).await;
    kernel.register(agent_loop_new(8)).await;

    let resp = kernel
        .dispatch(Envelope::new(PluginId::new("agent-loop"), json!({"op": "chat", "session_id": "s1", "user_text": "do it"})))
        .await
        .expect("dispatch chat");
    assert_eq!(resp["ok"], json!(true), "{resp:?}");

    let events = trace_events.lock().unwrap().clone();
    let payloads = llm_payloads.lock().unwrap().clone();
    let calls_in = tool_calls_in.lock().unwrap().clone();

    // ① 事件 schema：type 全集 + 必备 ts
    for e in &events {
        let ty = e.get("type").and_then(Value::as_str).unwrap_or("");
        assert!(
            FROZEN_EVENT_TYPES.contains(&ty),
            "事件类型越界：{ty}（新增类型必须先冻结进 FROZEN_EVENT_TYPES + 前端 render）"
        );
        assert!(e.get("ts").is_some(), "事件缺 ts：{e}");
    }
    let types_of = |t: &str| -> Vec<Value> {
        events.iter().filter(|e| e.get("type").and_then(Value::as_str) == Some(t)).cloned().collect()
    };
    assert_eq!(types_of("user").len(), 1);
    assert_eq!(types_of("tool_call").len(), 2);
    assert_eq!(types_of("tool_result").len(), 2);
    assert_eq!(types_of("assistant").len(), 1);
    // ⑥ 观测登记：产物卡 / 溯源卡
    let artifacts = types_of("artifact");
    assert!(
        artifacts.iter().any(|e| e["path"] == json!("outputs/report.md") && e["tool"] == json!("write_file")),
        "write_file 成功必须登记 artifact: {artifacts:?}"
    );
    let sources = types_of("sources");
    assert!(
        sources.iter().any(|e| e["tool"] == json!("web_search")
            && e["items"].as_array().map_or(false, |a| !a.is_empty())),
        "web_search 成功必须登记 sources: {sources:?}"
    );
    // tool_call / tool_result 必填字段
    for e in types_of("tool_call") {
        for k in ["round", "name", "id", "args"] {
            assert!(e.get(k).is_some(), "tool_call 缺 {k}: {e}");
        }
    }
    for e in types_of("tool_result") {
        for k in ["round", "name", "id", "ms", "ok", "result_truncated", "memory_truncated"] {
            assert!(e.get(k).is_some(), "tool_result 缺 {k}: {e}");
        }
    }
    // ② sid 形状（llm-adapter _SID_RE / 前端 doneSids 依赖）
    let assistant = &types_of("assistant")[0];
    for k in ["answer", "rounds", "sid", "reasonings", "usage", "elapsed_ms", "metrics"] {
        assert!(assistant.get(k).is_some(), "assistant 缺 {k}: {assistant}");
    }
    // E4-minimal：指标随 assistant 事件外抛（脚本场景：2 轮 LLM、0 重试、0 工具失败）
    let metrics = &assistant["metrics"];
    assert_eq!(metrics["llm_calls"], json!(2), "{assistant:?}");
    assert_eq!(metrics["retries"], json!(0));
    assert_eq!(metrics["tool_failures"], json!(0));
    let sid = assistant["sid"].as_str().expect("sid");
    let digits = sid.strip_prefix("s1-r").expect("sid 前缀");
    assert!(!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()), "sid 形状 {sid}");
    // ⑤ undo 透传：file_change 原样携带 undo 引用
    let fcs = types_of("file_change");
    assert!(
        fcs.iter().any(|e| e["path"] == json!("outputs/report.md") && e["op"] == json!("write")
            && e["undo"] == json!({"id": "u1", "created": [], "deleted": []})),
        "file_change 必须携带 path/op/round/undo 原样透传: {fcs:?}"
    );
    for e in &fcs {
        assert!(e.get("round").is_some(), "file_change 缺 round: {e}");
    }

    // ③/⑧ llm.chat payload 面：messages + tools + stream_path + sid；旁路路径规则
    assert_eq!(payloads.len(), 2, "两轮各一次 llm.chat");
    // S1（v0.1.7 消费）：两轮调用链 trace 贯穿且同源
    let traces = llm_traces.lock().unwrap().clone();
    assert_eq!(traces.len(), 2);
    assert!(!traces[0].is_empty());
    assert_eq!(traces[0], traces[1], "两轮 llm.chat 必须同属一条调用链 trace");
    for p in &payloads {
        assert!(p.get("messages").and_then(Value::as_array).map_or(false, |a| !a.is_empty()), "缺 messages");
        assert!(p.get("tools").and_then(Value::as_array).is_some(), "缺 tools");
        let sp = p.get("stream_path").and_then(Value::as_str).expect("缺 stream_path");
        assert!(sp.ends_with("s1.jsonl"), "旁路路径规则 .stream/{{session}}.jsonl，实际 {sp}");
        assert!(std::path::Path::new(sp).is_absolute(), "旁路路径须绝对（host tail 依赖）");
        let psid = p.get("sid").and_then(Value::as_str).expect("缺 sid");
        assert!(psid.starts_with("s1-r"), "payload sid 形状 {psid}");
    }
    // ④ 保留名路由：task 下发可见；load_skill / skill_install 绝不下发
    let tool_names: Vec<String> = payloads[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert!(tool_names.iter().any(|n| n == "task"), "task 必须随清单下发: {tool_names:?}");
    assert!(!tool_names.iter().any(|n| n == "load_skill" || n == "skill_install"),
        "保留名不得进 tools 清单: {tool_names:?}");
    assert!(tool_names.iter().any(|n| n == "calculator"), "tools.list 结果必须透传");

    // ⑦ memory op 面（agent-loop 只会调这 5 个 op）
    for op in ops_log.lock().unwrap().iter() {
        assert!(
            ["get", "append", "summarize", "trace.append", "trace.read"].contains(&op.as_str()),
            "memory op 越界：{op}"
        );
    }
    // ⑨ tools.exec payload 面
    for c in calls_in.iter().filter(|c| c["op"] == json!("call")) {
        for k in ["name", "args"] {
            assert!(c.get(k).is_some(), "tools.exec call 缺 {k}: {c}");
        }
    }
}
