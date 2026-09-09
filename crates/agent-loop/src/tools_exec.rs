//! 工具波次与产物/来源登记（R12 拆分自 lib.rs）。
//!
//! 职责：`act_begin` / `act_exec` / `act_end`（行动三段：观测 / 执行 / 回喂观测，
//! P4 并行波次的执行单元）、产物检测（`detect_artifacts` / `scan_artifact_paths` /
//! `output_dir` / `fresh_artifact`）、来源登记（`detect_sources` / `extract_sources`）、
//! 字符截断（`truncate_chars`）。

use super::*;
use std::time::Instant;

/// 按字符截断（UTF-8 安全）。超限时追加省略标记——模型必须能感知结果被裁剪，
/// 而非把残缺内容当作完整事实。
pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if max == 0 || total <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("\n…[truncated, {total} chars total, showing first {max}]"));
    out
}

impl AgentLoopPlugin {
    /// 行动前奏：登记 tool_call 观测（trace + tracing 日志）。
    /// 并行波次开始前由 chat_body 按**声明顺序**统一发出（trace 顺序稳定）。
    pub(super) async fn act_begin(&self, src: &Envelope, session_id: &str, round: u32, tc: &ToolCall) {
        tracing::info!(target: "react_progress", round, tool = %tc.name, "▶ round {round}: {}", tc.name);
        self.trace(src, session_id, json!({"type": "tool_call", "round": round, "name": tc.name, "id": tc.id, "args": tc.arguments}))
            .await;
    }

    /// 行动执行：保留名 `task` 路由子代理（07 §2.2），`load_skill` 路由 assets（07 §2.2），
    /// 其余走 tools.exec。失败合成 ok:false 结果回喂（不中断循环）。
    /// 不含事件发射——并行执行时事件顺序由 chat_body 统一编排（声明顺序，稳定）。
    /// 返回 (工具结果, 耗时ms)。`depth` 为当前链上的委派深度（0=顶层，随链传播）；
    /// `budget_snap` 为传给子代理的剩余预算快照 (剩余ms, 剩余token)（T4，随链衰减）。
    pub(super) async fn act_exec(
        &self,
        src: &Envelope,
        session_id: &str,
        depth: u32,
        tc: &ToolCall,
        budget_snap: Option<(Option<u64>, Option<u64>)>,
    ) -> (Value, u64) {
        let started = Instant::now();
        let result = if tc.name == RESERVED_TASK {
            // 子代理委派（Phase 3-3）：全新会话复用 agent.chat 全链路，仅回传最终答案
            let task_text = tc.arguments.get("task").and_then(Value::as_str).unwrap_or("");
            if task_text.trim().is_empty() {
                json!({"ok": false, "error": {"code": "K400", "field": "task", "message": "task 工具需非空参数 {\"task\": str}（子任务自包含描述）"}})
            } else {
                self.run_subagent(src, session_id, task_text, depth, budget_snap).await
            }
        } else if tc.name == RESERVED_SKILL_INSTALL {
            // R9 技能安装编排（不进 tools 分发）
            let name = tc.arguments.get("name").and_then(Value::as_str).unwrap_or("");
            self.skill_install(src, session_id, name).await
        } else if tc.name == RESERVED_LOAD_SKILL {
            // assets 路由；成功时发 skill_loaded 事件（会话技能集重放推导依据，R9）。
            // R9b：load_skill 即完成「读正文 + 装配套工具」——装载编排原只挂 skill_install，
            // 模型按习惯只调 load_skill 时工具永不进池（媒体生成不可见的根因），故在此合并；
            // preset 装载即启用，下一轮起并入会话清单。
            let name = tc.arguments.get("name").and_then(Value::as_str).unwrap_or("");
            let mut v = match self
                .call(src, ID_ASSETS, json!({"op": "skills.load", "name": name}), ASSETS_DEADLINE)
                .await
            {
                Ok(v) => v,
                Err(e) => json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
            };
            if v.get("ok") == Some(&json!(true)) {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    self.trace(src, session_id, json!({"type": "skill_loaded", "skill": trimmed})).await;
                    let (tools_loaded, tools_pending, issues) =
                        self.install_skill_tools(src, trimmed, &v).await;
                    if v.get("tools_manifest").is_some() {
                        // 有声明才发安装事件（前端内联卡依据；纯文档技能不刷卡）
                        self.trace(
                            src,
                            session_id,
                            json!({"type": "skill_installed", "skill": trimmed, "tools_loaded": tools_loaded, "tools_pending": tools_pending}),
                        )
                        .await;
                    }
                    // 装载结果并入回喂：模型可感知工具就绪（loaded）/待启用（pending）
                    v["tools_loaded"] = json!(tools_loaded);
                    v["tools_pending"] = json!(tools_pending);
                    if !issues.is_empty() {
                        v["tool_issues"] = json!(issues);
                    }
                }
            }
            v
        } else {
            match self
                .call(src, ID_TOOLS, json!({"op": "call", "name": tc.name, "args": tc.arguments}), TOOLS_DEADLINE)
                .await
            {
                Ok(v) => v,
                Err(e) => json!({"ok": false, "error": {"code": e.code(), "message": e.to_string()}}),
            }
        };
        (result, started.elapsed().as_millis() as u64)
    }

    /// 行动收尾：tool_result 观测。`ms` 为该工具自身耗时（并行下与墙钟无关）；
    /// `memory_truncated` 表明回喂进 memory 的内容是否被截断（PLAN R2 后全文不再入库）。
    pub(super) async fn act_end(&self, src: &Envelope, session_id: &str, round: u32, tc: &ToolCall, result: &Value, ms: u64) {
        tracing::info!(target: "react_progress", round, tool = %tc.name, ms, "✓ round {round}: {} ({}ms)", tc.name, ms);
        // 事件日志：结果截断（防大输出撑爆审计文件），memory 侧另有 8000 字符预算
        let result_str = result.to_string();
        let full_chars = result_str.chars().count();
        let limit = tool_result_limit();
        let truncated: String = result_str.chars().take(2000).collect();
        self.trace(
            src,
            session_id,
            json!({
                "type": "tool_result", "round": round, "name": tc.name, "id": tc.id, "ms": ms,
                "ok": result.get("ok") == Some(&json!(true)),
                "result_truncated": truncated,
                "memory_truncated": limit > 0 && full_chars > limit,
            }),
        )
        .await;
        // 产物登记：给用户的成品文件在事件流里结构化落账，前端渲染可点击文件卡片
        self.detect_artifacts(src, session_id, tc, result).await;
        // 来源登记：web 检索/阅读的引用链接结构化落账，前端渲染溯源卡（信息溯源）
        self.detect_sources(src, session_id, tc, result).await;
        // 变更登记（V2 执行可见性）：write/edit 成功 → file_change 事件结构化落账，
        // 前端聚合为头部「📝 文件变更」chip + 抽屉（随时可答「agent 动了哪些文件」）
        self.detect_file_changes(src, session_id, round, tc, result).await;
    }

    /// 产物检测：write_file/edit_file 是结构化结果直接取 path/bytes；bash 从输出文本
    /// 扩展名启发式扫描（python-docx 等子进程产物）。误报无害——前端点击由 /files
    /// 服务兜底 404；漏报仅少一张卡片。
    pub(super) async fn detect_artifacts(&self, src: &Envelope, session_id: &str, tc: &ToolCall, result: &Value) {
        if result.get("ok") != Some(&json!(true)) {
            return;
        }
        let inner = result.get("result").cloned().unwrap_or(json!({}));
        match tc.name.as_str() {
            "write_file" | "edit_file" => {
                if let Some(p) = inner.get("path").and_then(Value::as_str) {
                    // 路径归一为正斜杠：Windows 下 _display 产出反斜杠，卡片展示/URL/跨平台一致性统一
                    let p = p.replace('\\', "/");
                    // 只登记「给用户的成品」：脚本/数据/自扩展文件是中间产物，不上卡片
                    let name = p.rsplit('/').next().unwrap_or("");
                    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                    let is_product = Self::PRODUCT_EXTS.contains(&ext.as_str());
                    let is_meta = name == "SKILL.md" || name == "tools.json";
                    if is_product && !is_meta {
                        self.trace(
                            src,
                            session_id,
                            json!({
                                "type": "artifact", "path": p, "tool": tc.name,
                                "bytes": inner.get("bytes").and_then(Value::as_u64),
                            }),
                        )
                        .await;
                    }
                }
            }
            "bash" => {
                let out = inner.get("output").and_then(Value::as_str).unwrap_or("");
                for p in Self::scan_artifact_paths(out) {
                    if !self.fresh_artifact(session_id, &p) {
                        continue; // 历史文件：ls/git 输出误报，不上卡
                    }
                    self.trace(src, session_id, json!({"type": "artifact", "path": p, "tool": tc.name}))
                        .await;
                }
            }
            _ => {}
        }
    }

    /// 变更登记（执行可见性 V2 + W16）：write/edit 成功 → `file_change` 事件 {path, op,
    /// round}；bash（W16 追溯半）结果带 `changes[]`（bash.py 快照区前后比对）→ 逐条转发
    /// 为 op=bash 事件（新增/修改/删除 + undo 引用）。事件进既有 trace（只追加）、SSE 透传、
    /// schema 只增不改。区外改动（噪音/流/产物/工作区外）不产生事件。子代理变更经 trace
    /// 镜像自动汇入父会话（打 sub 标签）。
    pub(super) async fn detect_file_changes(
        &self,
        src: &Envelope,
        session_id: &str,
        round: u32,
        tc: &ToolCall,
        result: &Value,
    ) {
        for mut ev in Self::file_change_events_for(tc, result) {
            ev["round"] = json!(round);
            self.trace(src, session_id, ev).await;
        }
    }

    /// 多事件版（W16）：write/edit 单事件；bash 从 `result.changes[]` 展开多事件（op=bash，
    /// 带 undo 引用——回滚撤销与 diff 视图与 write/edit 同协议）。
    /// 失败的写/编辑不登记；path 归一为正斜杠（与产物登记同款）；
    /// 结果内带 undo 引用（files.py 变更快照，R15 回滚撤销 + 双侧 diff 数据源）→ 原样透传。
    pub(super) fn file_change_events_for(tc: &ToolCall, result: &Value) -> Vec<Value> {
        if result.get("ok") != Some(&json!(true)) {
            return Vec::new();
        }
        match tc.name.as_str() {
            "write_file" | "edit_file" => {
                let op = if tc.name == "write_file" { "write" } else { "edit" };
                let inner = result.get("result").cloned().unwrap_or(json!({}));
                let Some(path) = inner.get("path").and_then(Value::as_str) else {
                    return Vec::new();
                };
                let mut ev = json!({"type": "file_change", "path": path.replace('\\', "/"), "op": op});
                if let Some(u) = inner.get("undo") {
                    ev["undo"] = u.clone();
                }
                vec![ev]
            }
            "bash" => result
                .get("result")
                .and_then(|r| r.get("changes"))
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|c| {
                            let path = c.get("path").and_then(Value::as_str)?.replace('\\', "/");
                            let mut ev = json!({"type": "file_change", "path": path, "op": "bash"});
                            if let Some(u) = c.get("undo") {
                                ev["undo"] = u.clone();
                            }
                            Some(ev)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// 产物输出目录（方案 A）：AGENT_OUTPUT_DIR，缺省 `outputs`（工作区内相对路径）。
    /// 安全边界不变——文件工具越界拦截照旧，本约定只是给模型一个统一去处。
    pub(super) fn output_dir() -> String {
        std::env::var("AGENT_OUTPUT_DIR")
            .ok()
            .map(|s| s.trim().trim_matches(['/', '\\']).to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "outputs".into())
    }

    /// 兜底扫描路径的「新鲜度」闸：文件真实存在且 mtime ≥ 回合开始（容差 2s，吸收
    /// 文件系统时间精度）才登记——ls/git 列出的历史文件不再刷成产物卡。
    /// WORKSPACE_ROOT 未设置（e2e host 进程）或回合起点缺失 → 跳过校验保持旧行为。
    pub(super) fn fresh_artifact(&self, session_id: &str, path: &str) -> bool {
        let Some(ws) = std::env::var_os("WORKSPACE_ROOT").map(std::path::PathBuf::from) else {
            return true;
        };
        let started = self
            .turn_starts
            .lock()
            .unwrap()
            .get(session_id)
            .copied();
        let Some(started) = started else { return true };
        let p = std::path::Path::new(path);
        let full = if p.is_absolute() { p.to_path_buf() } else { ws.join(p) };
        match std::fs::metadata(&full).and_then(|m| m.modified()) {
            Ok(mt) => mt + std::time::Duration::from_secs(2) >= started,
            Err(_) => false,
        }
    }

    /// 来源登记（信息溯源）：web_search/web_read 的引用链接以 `sources` 事件结构化
    /// 落账，前端在最终答案之后渲染「来源」卡。与产物登记同款纪律：误报无害（仅多
    /// 一条链接），漏报仅少一卡。
    pub(super) async fn detect_sources(&self, src: &Envelope, session_id: &str, tc: &ToolCall, result: &Value) {
        if result.get("ok") != Some(&json!(true)) {
            return;
        }
        let inner = result.get("result").cloned().unwrap_or(json!({}));
        let items = Self::extract_sources(&tc.name, &inner);
        if items.is_empty() {
            return;
        }
        let items: Vec<Value> = items
            .into_iter()
            .map(|(title, url)| json!({"title": title, "url": url}))
            .collect();
        self.trace(src, session_id, json!({"type": "sources", "tool": tc.name, "items": items}))
            .await;
    }

    /// 从 web 工具结果提取 (title, url) 列表：http/https 白名单、URL 去重、上限 10 条
    /// （溯源卡是索引不是快照）。无 title 时用 URL 自身充作显示文本。
    pub(super) fn extract_sources(tool: &str, inner: &Value) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut push = |title: &str, url: &str| {
            let url = url.trim();
            if !url.starts_with("http") || out.iter().any(|(_, u)| u == url) || out.len() >= 10 {
                return;
            }
            let title = title.trim();
            out.push((
                if title.is_empty() { url.to_string() } else { title.to_string() },
                url.to_string(),
            ));
        };
        match tool {
            "web_search" => {
                for r in inner.get("results").and_then(Value::as_array).into_iter().flatten() {
                    push(
                        r.get("title").and_then(Value::as_str).unwrap_or(""),
                        r.get("url").and_then(Value::as_str).unwrap_or(""),
                    );
                }
            }
            "web_read" => push("", inner.get("url").and_then(Value::as_str).unwrap_or("")),
            _ => {}
        }
        out
    }

    /// 自由文本（bash 输出 / 最终答案）扫描「疑似产物路径」：产物扩展名 + 路径字符，
    /// 去重保序。启发式：误报无害（/files 404 兜底）；含空格的绝对路径会被空白截断——
    /// 借 WORKSPACE_ROOT 的目录名把「…/工作区目录名/xxx」截成工作区相对路径。
    /// 双闸收紧（2026-09-06 实测回归：一次 bash 列目录把整仓 .md 刷成 30+ 张卡）：
    /// 二进制成品扩展名放行；文本类扩展（md/html/txt…）仅产物目录内放行——bash 输出
    /// 里 rg/ls/git 列出的源码与文档路径高频出现，不设此闸必刷屏。文本类成品仍可经
    /// write_file 结构化登记（PRODUCT_EXTS 全集，不受此限）。
    pub(super) fn scan_artifact_paths(text: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        // 工作区目录名（如 react-agent）：用于截掉绝对路径前缀（含空格路径被空白切断的场景）
        let root_name = std::env::var("WORKSPACE_ROOT")
            .ok()
            .and_then(|ws| std::path::Path::new(&ws).file_name().map(|n| n.to_string_lossy().into_owned()));
        let out_dir_prefix = format!("{}/", Self::output_dir());
        for tok in text.split_whitespace() {
            let t = tok.trim_matches(|c: char| "()[]{}<>\"'`，。；：！？）【】、".contains(c));
            // 截前缀：token 中含「<sep>工作区目录名<sep>」→ 取其后的相对路径
            let mut t = t.to_string();
            if let Some(rn) = &root_name {
                for sep in ['\\', '/'] {
                    let marker = format!("{sep}{rn}{sep}");
                    if let Some(idx) = t.find(&marker) {
                        t = t[idx + marker.len()..].to_string();
                        break;
                    }
                }
            }
            let Some(dot) = t.rfind('.') else { continue };
            let (stem, ext) = t.split_at(dot);
            let ext = &ext[1..];
            if !Self::PRODUCT_EXTS.contains(&ext.to_ascii_lowercase().as_str())
                || stem.is_empty()
                || !stem.chars().all(|c| c.is_alphanumeric() || "\\/_-.~".contains(c))
            {
                continue;
            }
            let ext_l = ext.to_ascii_lowercase();
            let norm = t.replace('\\', "/");
            if !Self::SCAN_DOC_EXTS.contains(&ext_l.as_str()) && !norm.starts_with(&out_dir_prefix) {
                continue;
            }
            if !out.iter().any(|s| s.eq_ignore_ascii_case(&t)) {
                out.push(t);
            }
        }
        out
    }

    /// 成品扩展名白名单：卡片只收「给用户的交付物」，json/py 等中间脚本与数据不上卡。
    const PRODUCT_EXTS: &[&str] = &[
        "docx", "xlsx", "pptx", "pdf", "html", "htm", "md", "txt", "csv", "zip",
        "png", "jpg", "jpeg", "gif", "webp", "svg",
    ];

    /// 文本兜底扫描（bash 输出 / 最终答案）的扩展名闸：只认「二进制成品」。文本类
    /// 成品（md/html/txt…）仍可经 write_file 结构化登记、或写入产物目录（output_dir
    /// 前缀放行），避免把 rg/ls/git 输出里的整仓文档刷成产物卡。
    const SCAN_DOC_EXTS: &[&str] = &[
        "docx", "xlsx", "pptx", "pdf", "zip", "png", "jpg", "jpeg", "gif", "webp",
        "mp4", "webm", "mov",
    ];
}
