//! A3：agent-loop 通过内核 conformance 出生证明（第 3 个数据点，内核 1.0 收敛条件 #2）。
//!
//! 提供者用最小 mock（capability 寻址后，agent-loop 依赖 `memory.session` /
//! `llm.chat` / `tools.exec` 三个 capability 的提供者先行注册——K302 语义的正确使用）。

use agent_kernel_sdk::{
    ApiVersion, Capability, Domain, Envelope, KernelResult, Manifest, Plugin, PluginContext, PluginInstance,
    PluginId, PluginKind, Semantics, Version,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

type BoxFut = std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>;

/// 最小 mock：声明指定 capability，handler 按需应答。
struct MockPlugin {
    manifest: Manifest,
    on: Arc<dyn Fn(&Envelope) -> BoxFut + Send + Sync>,
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
            on: Arc::new(move |env| {
                let v = on(env);
                Box::pin(async move { v })
            }),
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

fn providers() -> Vec<PluginInstance> {
    vec![
        // memory：session 域全量操作（agent-loop 统一寻址 memory.session）
        MockPlugin::simple("memory", &["memory.session"], |env| {
            match env.payload.get("op").and_then(Value::as_str) {
                Some("get") => json!({"ok": true, "messages": []}),
                Some("append") | Some("summarize") | Some("trace.append") | Some("trace.read") => {
                    json!({"ok": true, "messages": [], "events": []})
                }
                _ => json!({"ok": false, "error": {"code": "K400", "message": "bad op"}}),
            }
        }),
        // llm：固定文本回复
        MockPlugin::simple("llm-adapter", &["llm.chat"], |_env| {
            json!({"ok": true, "content": "ok", "tool_calls": [], "model": "mock", "finish_reason": "stop"})
        }),
        // tools：空清单
        MockPlugin::simple("tools", &["tools.exec"], |env| {
            match env.payload.get("op").and_then(Value::as_str) {
                Some("list") => json!({"ok": true, "tools": []}),
                _ => json!({"ok": true}),
            }
        }),
    ]
}

/// 出生证明：agent-loop（有硬依赖的居民）在提供者前置下通过全套验收。
#[tokio::test]
async fn agent_loop_passes_conformance_suite() {
    let report = agent_kernel_conformance::certify_with_providers(
        Arc::new(|| react_agent_agent_loop::new(8)),
        providers(),
    )
    .await;
    assert!(
        report.is_ok(),
        "agent-loop 必须通过出生证明（7 项）：\n{}",
        report.summary()
    );
}
