//! 宿主装配接缝（Phase 1）：`HostSpec` 描述插件清单与开关，`assemble_with(spec)` 执行
//! spawn → 注册 → 探测 → agent-loop。默认 spec（`HostSpec::from_env`）与改造前行为逐项等价。
//!
//! 设计约束：
//! - 只把「装配数据」抽出来；spawn/探测的既有语义（硬依赖 bail、软依赖 warn、并行探测、
//!   bash 沙箱 fail-closed）原样保留；
//! - 不新增运行时概念：发行形态（rsi-agent）只需构造自己的 spec，不复制宿主。

use crate::{config, config::HostConfig, manifests, spawn};
use agent_kernel_kernel::Kernel;
use agent_kernel_sdk::{Envelope, Manifest, PluginId};
use futures::future::join_all;
use react_agent_agent_loop::LoopPolicy;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// guest 运行时类型（决定 spawn 方式与解释器）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRuntime {
    Python,
    NodeTs,
}

/// 启动探测声明：payload + 超时 + 软/硬依赖。
#[derive(Debug, Clone)]
pub struct ProbeSpec {
    pub payload: Value,
    pub timeout_secs: u64,
    pub label: String,
    /// 软依赖：探测失败仅 warn，不阻断启动（assets / llm 云端 ping）。
    pub soft: bool,
}

/// 单个 guest 插件装配声明。
#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub id: String,
    pub runtime: PluginRuntime,
    pub script: PathBuf,
    pub capabilities: Vec<String>,
    pub concurrent: bool,
    pub env: Vec<(String, String)>,
    /// 可选插件：spawn 失败仅 warn（assets 本就是软依赖）。
    pub optional: bool,
    pub probe: Option<ProbeSpec>,
    /// 是否在该插件 env 上执行 bash 沙箱 fail-closed 装配（tools）。
    pub bash_sandbox: bool,
}

/// 宿主装配规格：默认实现由环境变量构造，发行形态可替换。
#[derive(Debug, Clone)]
pub struct HostSpec {
    pub node_id: String,
    pub max_total_inflight: usize,
    pub plugins: Vec<PluginSpec>,
    pub max_rounds: usize,
    /// 启动后按注册表预热技能工具池（R18）。
    pub warm_skill_tools: bool,
    /// agent-loop 策略钩子（默认关闭）。
    pub loop_policy: LoopPolicy,
}

impl HostSpec {
    /// 默认发行形态：memory → llm-adapter → tools → assets，与改造前逐项一致。
    pub fn from_env(cfg: &HostConfig) -> Self {
        let tools_env = {
            let mut env = vec![("WORKSPACE_ROOT".into(), config::workspace_root())];
            env.extend(cfg.passthrough_env());
            env
        };
        let mut assets_env = cfg.passthrough_env();
        assets_env.push(("WORKSPACE_ROOT".into(), config::workspace_root()));
        Self {
            node_id: "react-agent".into(),
            max_total_inflight: 32,
            plugins: vec![
                PluginSpec {
                    id: "memory".into(),
                    runtime: PluginRuntime::NodeTs,
                    script: cfg.plugins_dir.join("memory").join("memory_plugin.ts"),
                    capabilities: vec!["memory.session".into(), "session.trace".into()],
                    concurrent: false,
                    env: vec![],
                    optional: false,
                    probe: Some(ProbeSpec {
                        payload: json!({"op": "get", "session_id": "__probe__"}),
                        timeout_secs: 30,
                        label: "memory(ts)".into(),
                        soft: false,
                    }),
                    bash_sandbox: false,
                },
                PluginSpec {
                    id: "llm-adapter".into(),
                    runtime: PluginRuntime::Python,
                    script: cfg.plugins_dir.join("llm_adapter").join("llm_plugin.py"),
                    capabilities: vec!["llm.chat".into()],
                    // R1：abort op 需在流式 chat 中并发受理。
                    concurrent: true,
                    env: cfg.llm_env(),
                    optional: false,
                    probe: Some(ProbeSpec {
                        payload: json!({"op": "chat", "messages": [{"role": "user", "content": "ping"}]}),
                        timeout_secs: 120,
                        label: format!("llm-adapter(py, provider={})", cfg.llm_provider),
                        soft: true,
                    }),
                    bash_sandbox: false,
                },
                PluginSpec {
                    id: "tools".into(),
                    runtime: PluginRuntime::Python,
                    script: cfg.plugins_dir.join("tools").join("tools_plugin.py"),
                    capabilities: vec!["tools.exec".into()],
                    // T6：插件并发安全已审查（call 仅读 _ENABLED/池；子进程独立）。
                    concurrent: true,
                    env: tools_env,
                    optional: false,
                    probe: Some(ProbeSpec {
                        payload: json!({"op": "list"}),
                        timeout_secs: 30,
                        label: "tools(py)".into(),
                        soft: false,
                    }),
                    bash_sandbox: true,
                },
                PluginSpec {
                    id: "assets".into(),
                    runtime: PluginRuntime::Python,
                    script: cfg.plugins_dir.join("assets").join("assets_plugin.py"),
                    capabilities: vec!["assets.registry".into()],
                    concurrent: false,
                    env: assets_env,
                    optional: true,
                    probe: Some(ProbeSpec {
                        payload: json!({"op": "skills.list"}),
                        timeout_secs: 30,
                        label: "assets(py)".into(),
                        soft: true,
                    }),
                    bash_sandbox: false,
                },
            ],
            max_rounds: cfg.max_rounds,
            warm_skill_tools: true,
            loop_policy: LoopPolicy::from_env(),
        }
    }
}

/// 按 spec 装配：按需探测解释器 → 并行 spawn → 注册（可选插件失败 warn）→ 并行探测
/// （硬依赖 bail / 软依赖 warn）→ 技能工具池预热 → 注册 agent-loop。
pub async fn assemble_with(kernel: &Kernel, spec: &HostSpec) -> anyhow::Result<()> {
    let needs_node = spec.plugins.iter().any(|p| p.runtime == PluginRuntime::NodeTs);
    let needs_py = spec.plugins.iter().any(|p| p.runtime == PluginRuntime::Python);
    let node = if needs_node { spawn::find_node() } else { None };
    let py = if needs_py { spawn::find_interpreter() } else { None };
    if needs_node && node.is_none() {
        anyhow::bail!("node 未找到（--experimental-strip-types 需 >= 22.6）");
    }
    if needs_py && py.is_none() {
        anyhow::bail!("python 未找到（guest 需要 grpcio；pip install grpcio httpx）");
    }

    // bash 沙箱装配在建 env 时完成（文件系统级探测；15s 超时）
    let mut envs: Vec<Vec<(String, String)>> = spec.plugins.iter().map(|p| p.env.clone()).collect();
    for (i, p) in spec.plugins.iter().enumerate() {
        if p.bash_sandbox {
            apply_bash_sandbox(&mut envs[i]).await;
        }
    }

    let spawns = spec.plugins.iter().zip(envs.iter()).map(|(p, env)| async move {
        let caps: Vec<&str> = p.capabilities.iter().map(String::as_str).collect();
        let manifest: Manifest = manifests::guest_manifest(&p.id, &caps, p.concurrent);
        match p.runtime {
            PluginRuntime::Python => spawn::spawn_python(py.expect("python checked"), &p.script, manifest, env).await,
            PluginRuntime::NodeTs => spawn::spawn_node_ts(node.expect("node checked"), &p.script, manifest, env).await,
        }
    });
    let results = join_all(spawns).await;

    // 注册（顺序与 spec 一致；可选插件失败降级）
    let mut registered: Vec<bool> = Vec::with_capacity(results.len());
    for (p, r) in spec.plugins.iter().zip(results) {
        match r {
            Ok(plugin) => {
                kernel.register(Arc::new(plugin)).await;
                registered.push(true);
            }
            Err(e) if p.optional => {
                tracing::warn!("spawn {}({}) 失败（软依赖，降级）: {e}", p.id, runtime_name(p.runtime));
                registered.push(false);
            }
            Err(e) => return Err(anyhow::anyhow!("spawn {}({}) 失败: {e}", p.id, runtime_name(p.runtime))),
        }
    }

    // 并行探测：只探测已注册插件（未注册视为探测失败，软依赖仅 warn）
    let probes = spec.plugins.iter().zip(registered.iter()).filter_map(|(p, ok)| {
        let probe_spec = p.probe.clone()?;
        Some(async move {
            let id = p.id.clone();
            let out = if *ok {
                probe(kernel, &id, &probe_spec).await
            } else {
                Err(anyhow::anyhow!("插件未注册"))
            };
            (id, out)
        })
    });
    let probe_results = join_all(probes).await;
    let mut assets_ok = false;
    for (id, out) in probe_results {
        let soft = spec
            .plugins
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.probe.as_ref())
            .map(|p| p.soft)
            .unwrap_or(true);
        match out {
            Ok(()) if id == "assets" => assets_ok = true,
            Ok(()) => {}
            Err(e) if soft => tracing::warn!("{id} 探测失败（软依赖，启动继续）: {e}"),
            Err(e) => return Err(anyhow::anyhow!("{id} 探测失败: {e}")),
        }
    }

    if spec.warm_skill_tools && assets_ok {
        match warm_skill_tools(kernel).await {
            Ok(n) if n > 0 => tracing::info!("技能工具池预热完成（{n} 个技能装载）"),
            Ok(_) => {}
            Err(e) => tracing::warn!("技能工具池预热失败（不阻断启动，chat 侧开局会重试）: {e}"),
        }
    }

    kernel
        .register(react_agent_agent_loop::new_with_policy(spec.max_rounds, spec.loop_policy))
        .await;
    tracing::info!(
        "agent-loop registered (max_rounds={}, contract_first={}, verify_before_final={})",
        spec.max_rounds,
        spec.loop_policy.contract_first,
        spec.loop_policy.verify_before_final
    );
    Ok(())
}

fn runtime_name(runtime: PluginRuntime) -> &'static str {
    match runtime {
        PluginRuntime::Python => "py",
        PluginRuntime::NodeTs => "ts",
    }
}

/// R18：技能工具池启动预热——逐技能 assets.load → tools.install（幂等）。
async fn warm_skill_tools(kernel: &Kernel) -> anyhow::Result<usize> {
    let list = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        kernel.dispatch(Envelope::new(PluginId::new("assets"), json!({"op": "skills.list"}))),
    )
    .await
    .map_err(|_| anyhow::anyhow!("skills.list 探测超时"))??;
    let names: Vec<String> = list
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut warmed = 0usize;
    for name in names {
        let loaded = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            kernel.dispatch(Envelope::new(PluginId::new("assets"), json!({"op": "skills.load", "name": name}))),
        )
        .await
        .map_err(|_| anyhow::anyhow!("skills.load({name}) 超时"))??;
        if loaded.get("ok") != Some(&json!(true)) {
            continue;
        }
        let Some(path) = loaded
            .get("tools_manifest")
            .and_then(|m| m.get("path"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let origin = loaded.get("origin").and_then(Value::as_str).unwrap_or("user");
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            kernel.dispatch(Envelope::new(PluginId::new("tools"), json!({"op": "install", "path": path, "skill": name, "origin": origin}))),
        )
        .await
        .map_err(|_| anyhow::anyhow!("tools.install({name}) 超时"))??;
        match r.get("ok") {
            Some(&json!(true)) => warmed += 1,
            other => tracing::warn!("技能工具预热装载失败（skill={name}）: {other:?}"),
        }
    }
    Ok(warmed)
}

/// bash 沙箱装配（05 §2.1 宿主层 fail-closed）：
/// 默认 BASH_SANDBOX=on —— 探测同目录 sandbox-run 助手，通过则向 tools 传 SANDBOX_HELPER；
/// 探测失败/助手缺失/取值非法 → 把 bash 移出 TOOLS_ENABLED（拒绝执行，绝不静默降级为无沙箱直跑）；
/// 仅 BASH_SANDBOX=off 显式豁免时无沙箱直跑（bash 描述如实声明）。
async fn apply_bash_sandbox(tools_env: &mut Vec<(String, String)>) {
    let mode = std::env::var("BASH_SANDBOX").unwrap_or_else(|_| "on".into());
    let helper = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sandbox-run.exe")));
    match config::resolve_bash_sandbox(&mode, helper) {
        config::BashSandbox::ExplicitOff => {
            tracing::info!("bash 沙箱已显式关闭（BASH_SANDBOX=off）：bash 将以完整用户权限直跑");
        }
        config::BashSandbox::Sandboxed { helper } => match probe_sandbox(&helper).await {
            Ok(()) => {
                tracing::info!("bash 沙箱就绪（sandbox-run 受限令牌助手探测通过）");
                tools_env.push(("SANDBOX_HELPER".into(), helper.to_string_lossy().into_owned()));
            }
            Err(e) => deny_bash(tools_env, &format!("沙箱助手探测失败: {e}")),
        },
        config::BashSandbox::Denied(reason) => deny_bash(tools_env, &reason),
    }
}

fn deny_bash(tools_env: &mut Vec<(String, String)>, reason: &str) {
    let idx = tools_env.iter().position(|(k, _)| k == "TOOLS_ENABLED");
    let list: Vec<String> = match idx.and_then(|i| tools_env.get(i)) {
        Some((_, v)) => v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        None => config::ALL_TOOL_NAMES.iter().map(|s| s.to_string()).collect(),
    };
    if list.iter().any(|n| n == "bash") {
        let filtered: Vec<String> = list.into_iter().filter(|n| n != "bash").collect();
        let v = filtered.join(",");
        match idx {
            Some(i) => tools_env[i].1 = v,
            None => tools_env.push(("TOOLS_ENABLED".into(), v)),
        }
        tracing::warn!(
            "bash 沙箱不可用，已 fail-closed 将 bash 移出 TOOLS_ENABLED（{reason}）。如接受无沙箱直跑请设 BASH_SANDBOX=off"
        );
    } else {
        tracing::warn!("bash 沙箱不可用（{reason}）；TOOLS_ENABLED 本就未含 bash，无需调整");
    }
}

/// 沙箱助手探测：真实走一次受限令牌执行（cmd /c exit 0），15s 超时。
async fn probe_sandbox(helper: &std::path::Path) -> anyhow::Result<()> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(helper).arg("probe").output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("探测超时（15s）"))??;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "exit={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

async fn probe(kernel: &Kernel, target: &str, spec: &ProbeSpec) -> anyhow::Result<()> {
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(spec.timeout_secs),
        kernel.dispatch(Envelope::new(PluginId::new(target), spec.payload.clone())),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{} 探测超时（>{}s）", spec.label, spec.timeout_secs))?
    .map_err(|e| anyhow::anyhow!("{} dispatch 失败: {e}", spec.label))?;
    if r.get("ok") != Some(&json!(true)) {
        anyhow::bail!("{} 探测失败: {r}", spec.label);
    }
    tracing::info!("{} 探测通过", spec.label);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spec_matches_legacy_assembly() {
        let cfg = HostConfig::from_env();
        let spec = HostSpec::from_env(&cfg);
        assert_eq!(spec.node_id, "react-agent");
        assert_eq!(spec.max_total_inflight, 32);
        assert_eq!(spec.max_rounds, cfg.max_rounds);
        // 注册顺序与改造前一致
        let ids: Vec<&str> = spec.plugins.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["memory", "llm-adapter", "tools", "assets"]);
        // 关键语义：llm/tools 并发；assets 可选；只有 tools 走 bash 沙箱
        assert!(spec.plugins[1].concurrent);
        assert!(spec.plugins[2].concurrent);
        assert!(spec.plugins[3].optional);
        assert!(spec.plugins[2].bash_sandbox);
        assert!(!spec.plugins[0].bash_sandbox);
        // 每个插件都有探测；llm/assets 为软依赖
        assert!(spec.plugins.iter().all(|p| p.probe.is_some()));
        assert!(spec.plugins[1].probe.as_ref().unwrap().soft);
        assert!(spec.plugins[3].probe.as_ref().unwrap().soft);
        assert!(spec.warm_skill_tools);
    }

    #[test]
    fn default_spec_loop_policy_is_off_without_env() {
        // 默认发行形态不得自行开启 RSI 钩子（除非显式 env/config 开启）。
        let keys = ["RSI_CONTRACT_FIRST", "RSI_VERIFY_BEFORE_FINAL"];
        let saved: Vec<(String, Option<String>)> = keys.iter().map(|k| (k.to_string(), std::env::var(k).ok())).collect();
        for k in keys {
            std::env::remove_var(k);
        }
        let cfg = HostConfig::from_env();
        let spec = HostSpec::from_env(&cfg);
        assert_eq!(spec.loop_policy, LoopPolicy::default());
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}
