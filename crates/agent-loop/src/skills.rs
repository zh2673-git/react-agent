//! 技能装配与提示词组装（R12 拆分自 lib.rs）。
//!
//! 职责：`resolve_system_prompt`（提示词组装链：env > WORKSPACE_ROOT/SYSTEM.md >
//! PROMPT 具名模板 > 内置缺省）、`skills_appendix` / `self_extension_section`
//! （技能附录 + L1 自扩展授权段 + 产物输出约定）、`trace_loaded_skills` /
//! `session_skill_tools`（R9 会话技能集重放推导与技能工具清单装配）、
//! `skill_install`（R9 技能安装编排，保留名 skill_install 路由终点）与
//! `install_skill_tools`（R9b 装载编排，load_skill / skill_install 共用）。

use super::*;

impl AgentLoopPlugin {
    /// 提示词组装链（07 §2.1）：env > WORKSPACE_ROOT/SYSTEM.md > PROMPT 具名模板（assets）> 内置缺省。
    pub(super) async fn resolve_system_prompt(&self, src: &Envelope) -> String {
        if let Ok(s) = std::env::var("AGENT_SYSTEM_PROMPT") {
            if !s.trim().is_empty() {
                return s;
            }
        }
        if let Ok(ws) = std::env::var("WORKSPACE_ROOT") {
            if let Ok(s) = std::fs::read_to_string(std::path::Path::new(&ws).join("SYSTEM.md")) {
                if !s.trim().is_empty() {
                    return s;
                }
            }
        }
        if let Ok(name) = std::env::var("PROMPT") {
            if !name.trim().is_empty() {
                if let Ok(v) = self
                    .call(src, CAP_ASSETS, json!({"op": "prompts.get", "name": name}), ASSETS_DEADLINE)
                    .await
                {
                    if let Some(c) = v.get("content").and_then(Value::as_str) {
                        if !c.trim().is_empty() {
                            return c.to_string();
                        }
                    }
                }
            }
        }
        DEFAULT_SYSTEM_PROMPT.into()
    }

    /// 技能附录（Discovery，07 §2.1）：assets 不可用/空列表 → 省略（不花 token）。
    /// skills.list 附带 root（08 §L1）：root ⊆ WORKSPACE_ROOT 时追加「技能自扩展」授权段——
    /// 模型可用 write_file 创建新技能（文件即注册表，list 每次重扫，下轮对话自动可见）。
    pub(super) async fn skills_appendix(&self, src: &Envelope) -> String {
        let Ok(v) = self.call(src, CAP_ASSETS, json!({"op": "skills.list"}), ASSETS_DEADLINE).await else {
            return String::new();
        };
        let Some(skills) = v.get("skills").and_then(Value::as_array) else {
            return String::new();
        };
        let root = v.get("root").and_then(Value::as_str).unwrap_or("");
        let mut lines = vec![String::new()];
        if !skills.is_empty() {
            lines.push("## Available skills".into());
            lines.push(
                "To use a skill, call the reserved tool load_skill with {\"name\": \"...\"} to load its full instructions."
                    .into(),
            );
            for s in skills {
                let name = s.get("name").and_then(Value::as_str).unwrap_or("");
                let desc = s.get("description").and_then(Value::as_str).unwrap_or("");
                if !name.is_empty() {
                    lines.push(format!("- {name}: {desc}"));
                }
            }
        }
        if let Some(section) = Self::self_extension_section(root) {
            lines.push(section);
        }
        // 产物输出约定（方案 A）：给用户的成品文件统一落产物目录——模型不再把产物
        // 混写进技能目录/仓库根。提示词约束非硬边界，越界拦截仍在文件工具侧。
        lines.push(format!(
            "## Artifact output\n- Files produced for the user (word/excel/ppt/html/markdown/images/…) must be written into `{}` (workspace-relative path). Do not scatter them into skill directories or the repo root. Mention the relative path of each artifact in your answer.",
            Self::output_dir()
        ));
        lines.join("\n")
    }

    /// L1 技能自扩展授权段（08 §三）：仅当 skills 根目录落在 WORKSPACE_ROOT 内（模型经
    /// write_file 可物理写入）时注入。这是授权声明而非新边界——真正的硬边界仍是
    /// 文件工具的越界拦截（提示词约束≠执行边界）。
    /// R9 扩展：技能打包与安装引导——SKILL.md + tools.json（语言无关声明）+ 执行体后
    /// 调 skill_install；preset 装载即启用，用户技能待界面确认后 load_skill 生效；
    /// load_skill 自身亦完成装载（R9b），两条保留名路径殊途同归。
    pub(super) fn self_extension_section(skills_root: &str) -> Option<String> {
        if skills_root.is_empty() {
            return None;
        }
        let ws = std::env::var("WORKSPACE_ROOT").ok()?;
        if !path_within(skills_root, &ws) {
            return None;
        }
        Some(
            "\n## Skill self-extension\n\
             You can extend your own skills: use write_file to create `<skills-root>/<name>/SKILL.md` \
             (directory name must equal the frontmatter `name`; frontmatter requires `name`, \
             `description` and `origin: user` — the origin tag marks user-created skills, without \
             it the skill is treated as factory-built and cannot be deleted later; the body holds \
             execution guidance, optionally referencing `references/` files \
             you also write). New/changed skills become visible in this catalog on the NEXT chat round — \
             no reload call needed. Keep skills small and focused; invalid frontmatter is silently skipped.\n\n\
             ### Skill packaging with companion tools\n\
             A skill may ship its own tools in a language-agnostic way: write a `tools.json` into the \
             skill directory (a JSON array; each item {\"name\",\"description\",\"parameters\",\
             \"exec\":{\"cmd\":[...],\"cwd\"?}}), plus the executor programs in any language \
             (Python/Node/Rust binary/script — declare the exact command in exec.cmd). Executor protocol: \
             read one JSON {\"args\":{...}} from stdin, reply one JSON {\"ok\":true,\"result\":...} or \
             {\"ok\":false,\"error\":{\"code\",\"message\"}} on stdout. After writing the files, call the \
             reserved tool `skill_install` with {\"name\":\"<skill>\"}: preset (factory) skills' tools are \
             enabled automatically on install and appear in your tool list immediately; user-created skills' \
             tools load into the pool but stay disabled until the user approves them in the settings UI \
             (Skills tab) — tell the user honestly instead of probing the environment. Declare tools only \
             when the skill truly needs them.\n"
                .replace("<skills-root>", skills_root)
                + "\n",
        )
    }

    /// R9：会话已加载技能集——从 trace 重放推导（skill_loaded 事件），**不新增循环可变态**
    /// （守 A1）。同会话新对话经重放恢复技能作用域；子代理新会话不继承（独立 trace）。
    pub(super) async fn trace_loaded_skills(&self, env: &Envelope, session_id: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        if let Ok(v) = self
            .call(env, CAP_MEMORY, json!({"op": "trace.read", "session_id": session_id, "after": 0}), MEM_DEADLINE)
            .await
        {
            if let Some(events) = v.get("events").and_then(Value::as_array) {
                for e in events {
                    if e.get("type").and_then(Value::as_str) == Some("skill_loaded") {
                        if let Some(s) = e.get("skill").and_then(Value::as_str) {
                            if !s.is_empty() {
                                out.insert(s.to_string());
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// R9：会话级技能工具清单——**已加载技能**的**已启用**工具（装配进 LLM 工具清单）。
    /// 未装载/未启用对模型不可见（防误用 + 清单不被领域工具污染）。失败降级为空，
    /// 主流程不因技能工具失败中断。
    pub(super) async fn session_skill_tools(&self, env: &Envelope, skills: &HashSet<String>) -> Vec<ToolSpec> {
        if skills.is_empty() {
            return vec![];
        }
        let payload = json!({"op": "skill_tools", "skills": skills});
        match self.call(env, CAP_TOOLS, payload, TOOLS_DEADLINE).await {
            Ok(v) => serde_json::from_value(v.get("tools").cloned().unwrap_or(Value::Null)).unwrap_or_default(),
            Err(e) => {
                tracing::warn!(target: ID, "tools.skill_tools failed, proceeding without skill tools: {e}");
                vec![]
            }
        }
    }

    /// R9/R9b：技能安装编排（保留名 skill_install 路由终点）——
    /// ① assets skills.load：取 SKILL.md 全文 + tools 声明（tools_manifest）；
    /// ② install_skill_tools：fail-closed 装载（preset 装载即启用，用户技能待界面确认）；
    /// ③ trace `skill_installed`（无声明技能同样发出，tools_* 为空 → 前端统一呈现「已注册」）；
    ///   preset 技能额外发 `skill_loaded`——装载即视同加载，trace 重放使工具跨对话持续可见；
    /// ④ 观察回写（含失败明细，部分成功不算整体失败——技能注册本身总是成立）。
    pub(super) async fn skill_install(&self, env: &Envelope, session_id: &str, name: &str) -> Value {
        if name.trim().is_empty() {
            return json!({"ok": false, "error": {"code": "K400", "field": "name",
                "message": "skill_install 需非空参数 {\"name\": str}（技能名）"}});
        }
        // ① 注册表取声明（unknown skill → 错误 payload 原样回喂）
        let loaded = match self
            .call(env, CAP_ASSETS, json!({"op": "skills.load", "name": name}), ASSETS_DEADLINE)
            .await
        {
            Ok(v) if v.get("ok") == Some(&json!(true)) => v,
            Ok(v) => return v,
            Err(e) => return json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
        };
        let (tools_loaded, tools_pending, issues) = self.install_skill_tools(env, name, &loaded).await;
        // ③ 事件日志（SSE 实时可达 → 前端内联卡 + 一键启用）
        self.trace(
            env,
            session_id,
            json!({"type": "skill_installed", "skill": name, "tools_loaded": tools_loaded, "tools_pending": tools_pending}),
        )
        .await;
        let preset = loaded.get("origin").and_then(Value::as_str) == Some("preset");
        if preset {
            self.trace(env, session_id, json!({"type": "skill_loaded", "skill": name})).await;
        }
        // ④ 观察回写
        let mut out = json!({
            "ok": true,
            "skill": name,
            "registered": true,
            "tools_loaded": tools_loaded,
            "tools_pending": tools_pending,
            "note": if preset {
                "出厂技能：配套工具装载即自动启用，会话内即可调用"
            } else {
                "技能已注册；待启用工具需用户在界面确认（装载≠启用），启用后 load_skill 生效于会话"
            },
        });
        if !issues.is_empty() {
            out["issues"] = json!(issues);
        }
        out
    }

    /// R9b 工具装载编排（skill_install 与 load_skill 共用）：有 tools.json 声明 →
    /// tools.install fail-closed 装载进池（origin=preset 由 tools 侧装载即启用；
    /// 用户技能进池待界面确认）。声明文件缺失/装载失败逐条记入 issues（不阻断）。
    /// 返回 (loaded, pending, issues)。
    pub(super) async fn install_skill_tools(
        &self,
        env: &Envelope,
        name: &str,
        loaded: &Value,
    ) -> (Vec<String>, Vec<String>, Vec<Value>) {
        let mut tools_loaded: Vec<String> = vec![];
        let mut tools_pending: Vec<String> = vec![];
        let mut issues: Vec<Value> = vec![];
        if let Some(m) = loaded.get("tools_manifest") {
            // 声明存在但文件缺失（assets 回传 missing）→ 明细回写，模型可感知修复
            if let Some(missing) = m.get("missing").and_then(Value::as_array) {
                for f in missing {
                    issues.push(json!({"where": "tools.json", "error": format!("声明文件不存在: {f}")}));
                }
            }
            if let Some(path) = m.get("path").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                // fail-closed 装载（origin=preset 由 tools 侧装载即启用）
                let origin = loaded.get("origin").and_then(Value::as_str).unwrap_or("user");
                match self
                    .call(env, CAP_TOOLS, json!({"op": "install", "path": path, "skill": name, "origin": origin}), TOOLS_DEADLINE)
                    .await
                {
                    Ok(v) if v.get("ok") == Some(&json!(true)) => {
                        tools_loaded = v
                            .get("loaded")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default();
                        tools_pending = v
                            .get("pending")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                            .unwrap_or_default();
                        for s in v.get("skipped").and_then(Value::as_array).cloned().unwrap_or_default() {
                            issues.push(json!({"where": "tools.install", "error": s}));
                        }
                    }
                    Ok(v) => issues.push(json!({"where": "tools.install", "error": v.get("error").cloned().unwrap_or(v)})),
                    Err(e) => issues.push(json!({"where": "tools.install", "error": e.to_string()})),
                }
            }
        }
        (tools_loaded, tools_pending, issues)
    }
}
