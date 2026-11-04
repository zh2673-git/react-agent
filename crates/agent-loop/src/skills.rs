//! 技能装配与提示词组装（R12 拆分自 lib.rs）。
//!
//! 职责：`resolve_system_prompt`（提示词组装链：env > WORKSPACE_ROOT/SYSTEM.md >
//! PROMPT 具名模板 > 内置缺省）、`skills_appendix` / `self_extension_section`
//! （技能附录 + L1 自扩展授权段 + 产物输出约定）、`trace_loaded_skills` /
//! `session_skill_tools`（R9 会话技能集重放推导与技能工具清单装配）、
//! `skill_install`（R9 技能安装编排，保留名 skill_install 路由终点）与
//! `install_skill_tools`（R9b 装载编排，load_skill / skill_install 共用）。

use super::config::AgentLoopConfig;
use super::*;

impl AgentLoopPlugin {
    /// 提示词组装链（07 §2.1）：cfg 覆盖（env 单点解析，E3）> WORKSPACE_ROOT/SYSTEM.md > PROMPT 具名模板（assets）> 内置缺省。
    pub(super) async fn resolve_system_prompt(&self, src: &Envelope, cfg: &AgentLoopConfig) -> String {
        if let Some(s) = &cfg.system_prompt_override {
            return s.clone();
        }
        if let Ok(ws) = std::env::var("WORKSPACE_ROOT") {
            if let Ok(s) = std::fs::read_to_string(std::path::Path::new(&ws).join("SYSTEM.md")) {
                if !s.trim().is_empty() {
                    return s;
                }
            }
        }
        if let Some(name) = &cfg.prompt_template {
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
        DEFAULT_SYSTEM_PROMPT.into()
    }

    /// 技能附录（Discovery，07 §2.1）：assets 不可用/空列表 → 省略（不花 token）。
    /// skills.list 附带 root（08 §L1）：root ⊆ WORKSPACE_ROOT 时追加「技能自扩展」授权段——
    /// 模型可用 write_file 创建新技能（文件即注册表，list 每次重扫，下轮对话自动可见）。
    ///
    /// B2 按需注入：`want_skill_build` 由调用方按**本轮用户文本**判定（见
    /// `user_wants_skill_build`）。为假时只注入极短授权段；为真时才注入完整打包教程。
    pub(super) async fn skills_appendix(
        &self,
        src: &Envelope,
        cfg: &AgentLoopConfig,
        want_skill_build: bool,
    ) -> String {
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
        if let Some(section) = Self::self_extension_section(root, want_skill_build) {
            lines.push(section);
        }
        // 产物输出约定（方案 A）：给用户的成品文件统一落产物目录——模型不再把产物
        // 混写进技能目录/仓库根。提示词约束非硬边界，越界拦截仍在文件工具侧。
        lines.push(format!(
            "## Artifact output\n- Files produced for the user (word/excel/ppt/html/markdown/images/…) must be written into `{}` (workspace-relative path). Do not scatter them into skill directories or the repo root. Mention the relative path of each artifact in your answer.",
            cfg.output_dir
        ));
        // 反向约定（与上面那条成对出现才有约束力）：只讲「成品去哪」时，模型会把
        // 探针/一次性脚本也算作「产物」写进产物目录。实测成因：那类脚本占 file_change
        // 的 46%，且产物目录在 bash 快照排除区（写了连 undo 留痕都没有）。
        lines.push(format!(
            "- Intermediate files (probe / scratch / one-off analysis scripts) are NOT artifacts: do not write them into `{}`. Prefer reading files with the dedicated tools (grep / read_file / symbols_search); if a script is unavoidable, put it under `.scratch/` and delete it when done.",
            cfg.output_dir
        ));
        lines.join("\n")
    }

    /// 判定本轮用户文本是否明确要求「造技能/打包工具」——B2 按需注入的触发条件。
    ///
    /// 保守取词：宁可少注入（brief 已说明「有这个权力」），也不要每轮灌长教程。
    /// 覆盖中英文常见表述；`skill`/`技能` 与「造/做/建/打包/新建/写一个」共现才算。
    pub(super) fn user_wants_skill_build(user_text: &str) -> bool {
        let t = user_text.to_lowercase();
        let has_noun = t.contains("技能") || t.contains("skill");
        if !has_noun {
            return false;
        }
        ["造", "做", "建", "打包", "新建", "写一个", "生成", "创建", "封装", "沉淀", "整理成",
         "create", "build", "make", "package", "author", "write a", "new"]
            .iter()
            .any(|kw| t.contains(kw))
    }

    /// L1 技能自扩展授权段（08 §三）：仅当 skills 根目录落在 WORKSPACE_ROOT 内（模型经
    /// write_file 可物理写入）时注入。这是授权声明而非新边界——真正的硬边界仍是
    /// 文件工具的越界拦截（提示词约束≠执行边界）。
    ///
    /// **按需注入（B2）**：默认只给一条极短的授权声明；完整的打包/安装教程
    /// （`### Skill packaging with companion tools`，约 1.2 KB）仅在 `full=true` 时注入。
    /// 依据：实测 `skills_appendix` 每轮都随 system prompt 重发，长教程对「不造技能」的
    /// 绝大多数轮次是纯 token 开销，且会抬高模型的工具调用倾向（效率放大器 E2）。
    /// `full=true` 由调用方在「用户本轮明确要求造技能/打包工具」时决定。
    pub(super) fn self_extension_section(skills_root: &str, full: bool) -> Option<String> {
        if skills_root.is_empty() {
            return None;
        }
        let ws = std::env::var("WORKSPACE_ROOT").ok()?;
        if !path_within(skills_root, &ws) {
            return None;
        }
        // 常驻段：只讲「你有这个权力」和最小落点，不讲打包协议
        let brief = format!(
            "\n## Skill self-extension\n\
             You can extend your own skills: use write_file to create `<root>/<name>/SKILL.md` \
             (directory name must equal the frontmatter `name`; frontmatter requires `name`, \
             `description` and `origin: user` — without the origin tag the skill is treated as \
             factory-built and cannot be deleted later). New/changed skills become visible in this \
             catalog on the NEXT chat round — no reload call needed. Keep skills small and focused; \
             invalid frontmatter is silently skipped.\n\
             When the user asks you to build or package a skill, a skill may also ship its own tools \
             (`tools.json` + an executor in any language, installed via the reserved `skill_install` tool).\n"
        )
        .replace("<root>", skills_root);
        if !full {
            return Some(brief);
        }
        // 按需段：完整打包/安装协议（仅当用户明确要造技能时注入）
        Some(
            brief
                + "\n### Skill packaging with companion tools\n\
             Write a `tools.json` into the skill directory (a JSON array; each item {\"name\",\"description\",\
             \"parameters\",\"exec\":{\"cmd\":[...],\"cwd\"?}}), plus the executor programs in any language \
             (Python/Node/Rust binary/script — declare the exact command in exec.cmd). Executor protocol: \
             read one JSON {\"args\":{...}} from stdin, reply one JSON {\"ok\":true,\"result\":...} or \
             {\"ok\":false,\"error\":{\"code\",\"message\"}} on stdout. After writing the files, call the \
             reserved tool `skill_install` with {\"name\":\"<skill>\"}: preset (factory) skills' tools are \
             enabled automatically on install and appear in your tool list immediately; user-created skills' \
             tools load into the pool but stay disabled until the user approves them in the settings UI \
             (Skills tab) — tell the user honestly instead of probing the environment. Declare tools only \
             when the skill truly needs them.\n",
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
        match self.call(env, CAP_TOOLS, payload, tools_deadline()).await {
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
        // ② Phase 1 质量门：安装前跑 assets `skills.validate`（mode=install 只拦结构错误，
        //    存量技能不被误伤）；不通过即拒绝安装，错误逐条回喂（code/path/message）。
        //    门禁只对「结构化拒绝」生效：`errors` 非空才拦；空错误（旧版 assets 未实现
        //    该 op / 内部异常）按「校验不可用」放行，避免误伤存量与第三方资产插件。
        if let Ok(v) = self
            .call(env, CAP_ASSETS, json!({"op": "skills.validate", "name": name, "mode": "install"}), ASSETS_DEADLINE)
            .await
        {
            let errors = v.get("errors").and_then(Value::as_array).cloned().unwrap_or_default();
            if v.get("ok") == Some(&json!(false)) && !errors.is_empty() {
                let errors = Value::Array(errors);
                self.trace(
                    env,
                    session_id,
                    json!({"type": "skill_rejected", "skill": name, "errors": errors}),
                )
                .await;
                return json!({
                    "ok": false,
                    "skill": name,
                    "error": {
                        "code": "SKILL_INVALID",
                        "message": "技能未通过安装校验（结构错误），已在注册前拒绝",
                        "details": errors,
                    },
                });
            }
            if v.get("ok") == Some(&json!(false)) {
                tracing::warn!(target: ID, "skills.validate 未返回错误明细，按校验不可用放行: {v}");
            }
        }
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
                    .call(env, CAP_TOOLS, json!({"op": "install", "path": path, "skill": name, "origin": origin}), tools_deadline())
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
