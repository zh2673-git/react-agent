//! Manifest 构造。注册顺序（host/main）：memory → llm-adapter → tools → agent-loop。
//! agent-loop 硬依赖三个 provider 的 capability；`Kernel::register` 对 K302 静默失败，
//! 因此 provider 必须先注册且通过探测。（agent-loop 自身的 manifest 在其 crate 内构造。）

use agent_kernel_sdk::{ApiVersion, Capability, Domain, Manifest, PluginKind, Semantics, Version};

/// Process 域 guest 的 host 侧 manifest。
/// 注意：api_version 必须 (0,1)——握手要求 guest major == host major 且 guest minor >= host minor，
/// Python/TS guest 声明 "0.1"。
pub fn guest_manifest(id: &str, capabilities: &[&str]) -> Manifest {
    Manifest {
        name: agent_kernel_sdk::PluginId::new(id),
        kind: PluginKind::Capability,
        version: Version::new(0, 1, 0),
        api_version: ApiVersion::new(0, 1),
        capabilities: capabilities.iter().map(|c| Capability::new(*c)).collect(),
        dependencies: vec![],
        domain: Domain::Process,
        semantics: Semantics::Serial,
        priority: 1,
        max_inflight: Some(8),
        fuel_limit: None,
        host_timeout_ms: None,
        epoch_interval_ms: None,
        subscriptions: vec![],
    }
}
