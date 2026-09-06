//! REPL 前端（Phase 3-2，01 §Phase3）：`trait Frontend` 两实现之一（W10 起 web 侧
//! 拆分至 [`crate::web`]：gateway / api / files 三子模块）。
//!
//! - `ReplFrontend`：终端交互（REPL / 单轮），斜杠命令见 `repl_command`。
//!
//! host 是组合根：前端是**入口组件**而非 guest 能力——网关需调 `agent.chat` + `session.trace`，
//! 而 guest 不可互调（内核物理约束）；前端切换必伴随重启，热插拔无收益。
//! 选择：`REACT_FRONTEND=repl`（默认）/ `web`（`WEB_ADDR` 默认 127.0.0.1:8710）。
use agent_kernel_sdk::{Envelope, PluginId};
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait Frontend: Send + Sync {
    /// 阻塞运行前端直到退出（REPL EOF/exit；web 伺服器永不返回）。
    async fn run(&self, kernel: Arc<Kernel>, session: String) -> anyhow::Result<()>;
}

/// 按 `REACT_FRONTEND` env 装配前端（默认 repl）。
pub fn from_env() -> Box<dyn Frontend> {
    match std::env::var("REACT_FRONTEND")
        .unwrap_or_else(|_| "repl".into())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "web" => Box::new(crate::web::WebFrontend::from_env()),
        _ => Box::new(ReplFrontend),
    }
}

// ───────────────────────────── ReplFrontend ─────────────────────────────

pub struct ReplFrontend;

#[async_trait::async_trait]
impl Frontend for ReplFrontend {
    async fn run(&self, kernel: Arc<Kernel>, session: String) -> anyhow::Result<()> {
        println!(
            "react-agent ready（session={session}）。输入 exit/quit 退出；/help 查看命令（/prompt、/skill 等）。"
        );
        let mut line = String::new();
        loop {
            print!("react-agent> ");
            let _ = std::io::stdout().flush();
            line.clear();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => break, // EOF
                Ok(_) => {}
            }
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            if text == "exit" || text == "quit" {
                break;
            }
            if text.starts_with('/') {
                repl_command(&kernel, text).await;
                continue;
            }
            turn(&kernel, &session, text).await;
        }
        Ok(())
    }
}

/// 单轮对话：dispatch agent.chat，打印答案与 steps 汇总（过程已由 react_progress 实时回显）。
pub async fn turn(kernel: &Kernel, session: &str, text: &str) {
    let env = Envelope::new(
        PluginId::new("agent-loop"),
        json!({"op": "chat", "session_id": session, "user_text": text}),
    );
    match kernel.dispatch(env).await {
        Ok(v) if v.get("ok") == Some(&json!(true)) => {
            if let Some(ans) = v.get("answer").and_then(Value::as_str) {
                println!("{ans}");
            }
            if let Some(steps) = v.get("steps").and_then(Value::as_array) {
                if !steps.is_empty() {
                    let summary = steps
                        .iter()
                        .map(|s| {
                            format!(
                                "r{}:{}({}ms)",
                                s.get("round").and_then(Value::as_u64).unwrap_or(0),
                                s.get("tool").and_then(Value::as_str).unwrap_or("?"),
                                s.get("ms").and_then(Value::as_u64).unwrap_or(0)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("[steps] {summary}");
                }
            }
        }
        Ok(v) => eprintln!("[agent error] {}", v.get("error").cloned().unwrap_or(v)),
        Err(e) => eprintln!("[kernel error] {e}"),
    }
}

/// REPL 斜杠命令（Phase 2-3）：/prompt、/skill。assets 为软依赖——不可用时命令报错不崩溃。
async fn repl_command(kernel: &Kernel, text: &str) {
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match (cmd, arg) {
        ("/help", "") => {
            println!(
                "命令：\n  /prompt            列出可用提示词模板\n  /prompt <name>     切换系统提示词（后续会话生效）\n  /prompt off        恢复内置缺省提示词\n  /skill             列出可用技能\n  /skill <name>      查看技能全文（SKILL.md）\n  exit | quit        退出"
            );
        }
        ("/prompt", "") => match assets(kernel, json!({"op": "prompts.list"})).await {
            Some(v) => {
                let items = v["prompts"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("（无可用提示词模板；prompts/ 目录为空或 assets 不可用）");
                } else {
                    for p in &items {
                        println!(
                            "- {}: {}",
                            p["name"].as_str().unwrap_or("?"),
                            p["description"].as_str().unwrap_or("")
                        );
                    }
                    println!("当前 PROMPT={}", std::env::var("PROMPT").unwrap_or_else(|_| "（未设置，用内置缺省）".into()));
                }
            }
            None => println!("[assets 不可用]"),
        },
        ("/prompt", "off") => {
            std::env::remove_var("PROMPT");
            println!("已恢复内置缺省提示词");
        }
        ("/prompt", name) => match assets(kernel, json!({"op": "prompts.get", "name": name})).await {
            Some(v) if v.get("ok") == Some(&json!(true)) => {
                std::env::set_var("PROMPT", name);
                println!("系统提示词已切换为 {name}（下轮对话生效）");
            }
            _ => println!("未知提示词模板: {name}（/prompt 查看列表）"),
        },
        ("/skill", "") => match assets(kernel, json!({"op": "skills.list"})).await {
            Some(v) => {
                let items = v["skills"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("（无可用技能；skills/ 目录为空或 assets 不可用）");
                } else {
                    for s in items {
                        println!(
                            "- {}: {}",
                            s["name"].as_str().unwrap_or("?"),
                            s["description"].as_str().unwrap_or("")
                        );
                    }
                    println!("（技能由模型经 load_skill 按需激活，无需手动加载）");
                }
            }
            None => println!("[assets 不可用]"),
        },
        ("/skill", name) => match assets(kernel, json!({"op": "skills.load", "name": name})).await {
            Some(v) if v.get("ok") == Some(&json!(true)) => {
                println!("{}", v["content"].as_str().unwrap_or(""));
            }
            _ => println!("未知技能: {name}（/skill 查看列表）"),
        },
        (other, _) => println!("未知命令 {other}（/help 查看命令）"),
    }
}

/// assets 软依赖调用：失败返回 None（REPL 命令降级提示，不崩溃）。
async fn assets(kernel: &Kernel, payload: Value) -> Option<Value> {
    let r = kernel
        .dispatch(Envelope::new(PluginId::new("assets"), payload))
        .await
        .ok()?;
    if r.get("ok") == Some(&json!(true)) {
        Some(r)
    } else {
        None
    }
}
