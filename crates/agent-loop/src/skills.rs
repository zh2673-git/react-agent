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
        want_task_plan: bool,
    ) -> String {
        // `AGENT_SYSTEM_PROMPT` 文档写的是「整体覆盖」，但那只是 `resolve_system_prompt`
        // 的**第一环**——本附录在 chat.rs 里是**并列拼接**的，于是「覆盖」并没有真正覆盖：
        // 运维用该变量钉死提示词后，附录仍照旧追加，系统提示词变成「你以为的 + 你没要的」。
        //
        // 后果有两层：其一，语义与文档不符（用户设了覆盖却不生效）；其二，本附录随功能
        // 增长而变长，而按预算精确断言的用例（token 闸 P7/P8 那一组，预算 301）会把
        // 附录长度写进余量——任何新增段落都会让那组用例变红，红的原因却看不出与它有关。
        // 覆盖态下不注入附录，既兑现文档语义，也让「钉死提示词」的用例真正可预期。
        if cfg.system_prompt_override.is_some() {
            return String::new();
        }
        // G 段（I-20261114-05）：技能注册表拿不到 **≠** 整段提示词都别要了。
        //
        // 原先这两处 `return String::new()` 会把**后面所有段**一起扔掉——看板纪律、
        // `@T-/@I-` 自动装载说明、分解/并行/三档判据、产物约定。于是技能插件因为
        // 一个 SKILL.md 的编码坏掉而 K500 时，模型连「有看板这回事」都不知道：
        // 用户看到的症状是「新窗口的会话不把活登记到看板」，与本文件毫无可见关联
        // （实测 2026-11-14：project-dev/SKILL.md 尾部混入 GBK 字节 → skills.list
        // 整条 500 → 该窗口整轮提示词缩水，且没有任何报错冒到界面）。
        //
        // 判据：技能段的失败只该让**技能段**退化（不注入 Available skills），其余
        // 能力照旧。看板是用户唯一的进度视图，它的可用性不能挂在技能注册表上。
        let (skills_root, skills): (String, Vec<Value>) = match self
            .call(src, CAP_ASSETS, json!({"op": "skills.list"}), ASSETS_DEADLINE)
            .await
        {
            Ok(v) => {
                let list = v
                    .get("skills")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let root = v.get("root").and_then(Value::as_str).unwrap_or("").to_string();
                (root, list)
            }
            Err(e) => {
                tracing::warn!(
                    target: ID,
                    "skills.list 失败，技能段降级（看板/画像/记忆段照旧）: {e}"
                );
                (String::new(), Vec::new())
            }
        };
        let mut lines = vec![String::new()];
        if !skills.is_empty() {
            lines.push("## Available skills".into());
            lines.push(
                "To use a skill, call the reserved tool load_skill with {\"name\": \"...\"} to load its full instructions."
                    .into(),
            );
            for s in &skills {
                let name = s.get("name").and_then(Value::as_str).unwrap_or("");
                let desc = s.get("description").and_then(Value::as_str).unwrap_or("");
                if !name.is_empty() {
                    lines.push(format!("- {name}: {desc}"));
                }
            }
        }
        if let Some(section) = Self::self_extension_section(&skills_root, want_skill_build) {
            lines.push(section);
        }
        // 任务看板（单会话形态）：把「人的思维模式」落成可执行纪律——先想清怎么做（plan）、
        // 成立什么部门（skills）、用多少人力（model/effort）、需要什么物力（tools）、
        // 好坏标准（acceptance），再动手。按需注入（同 B2 理由），通道写死 bash 调 CLI。
        let board = Self::task_board_section(want_task_plan);
        if !board.is_empty() {
            lines.push(board);
        }
        // C 轮：看板通道升级（@引用自动装载 / resolve / digest / layout / checks 评价纪律）。
        // 与 task_board_section 分两段：B 轮那段是长字符串字面量，改它等于重写整块提示词；
        // 新增能力单独成段，锚点稳且可独立回退。
        let board_ops = super::board_ctx::board_ops_hint(want_task_plan, super::board_ctx::has_board_cli());
        if !board_ops.is_empty() {
            lines.push(board_ops);
        }
        // D 轮：习惯画像通道（同「新增能力单独成段」纪律）。按需注入：问「是不是交代
        // 一件成规模的事」时不谈画像——画像写的是跨会话结论，与当轮任务规划无关。
        let profile_ops = super::profile_ctx::profile_ops_hint(want_task_plan);
        if !profile_ops.is_empty() {
            lines.push(profile_ops);
        }
        // G 轮：记忆 L2 通道（同「新增能力单独成段」纪律）。三层分工必须讲清，否则
        // 模型会在「上周干了什么」这类问题上直接去读 trace 原文——单个会话实测
        // 17.6 MB / 29507 条事件，既塞不进上下文，也看不出重点。
        let memory_ops = super::memory_ctx::memory_ops_hint();
        if !memory_ops.is_empty() {
            lines.push(memory_ops);
        }
        // D 轮：习惯画像通道（同「新增能力单独成段」纪律）。按需注入：问「是不是交代
        // 一件成规模的事」时不谈画像——画像写的是跨会话结论，与当轮任务规划无关。
        // D 轮：任务分解与委派纪律（I-20261107-01 的落点）。B 轮已要求「先想清怎么做
        // 再动手」，但 plan 只是一串句子——看不出**哪一步该交给子代理**、**哪一步该现造
        // 技能**。这里把「复杂度判断」变成 plan 上的一个字段，模型必须对每步作答。
        let decompose = super::board_ctx::decompose_hint();
        if !decompose.is_empty() {
            lines.push(decompose);
        }
        // G4b（I-20261108-06）：并行判据。波次并发与子代理并发都是既有能力，缺的只是
        // 「哪几步可以同时干」这条判断——不写进提示词，模型就随机并行（撞同一文件/目录）。
        let parallel = super::board_ctx::parallel_hint();
        if !parallel.is_empty() {
            lines.push(parallel);
        }
        // S 轮：入口三档分流（S/M/L）。与 decompose/parallel 同一纪律——新增能力单独成段，
        // 锚点稳且可独立回退。门控与上面两段一致（无 board CLI 则整段不注入）。
        let routing = super::board_ctx::routing_hint();
        if !routing.is_empty() {
            lines.push(routing);
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

    /// 任务看板纪律段（单会话形态）：把「人的思维模式」写进提示词——从需求出发先想清
    /// 怎么做（plan）、成立什么部门/角色（skills）、用多少人力（model/effort）、需要什么
    /// 物力（tools）、好坏标准（acceptance），再动手。
    ///
    /// **按需注入**（与 B2 同一理由）：`skills_appendix` 每轮随 system prompt 重发，长规范
    /// 对「不建任务」的轮次是纯 token 开销。`full=false` 只给一行指针（有这个权力 + 通道在哪），
    /// `full=true` 才给完整写作规范——判定见 `user_wants_task_plan`。
    ///
    /// **通道写死 bash 调 CLI**：`python scripts/board.py`。不走技能 `tools.json`（装载后默认
    /// 未启用，闭环会断在用户点配置上）。**不要求每轮更新**：只在建任务/阶段迁移/阻塞/完成
    /// 四个时刻写，其余时间靠 plan 步态自然累积——每轮刷看板是形式主义，纯烧 token。
    pub(super) fn task_board_section(full: bool) -> String {
        // 包装层：正文一字不动，出口把相对命令重写成绝对命令（见 abs_cli_cmds）。
        super::board_ctx::abs_cli_cmds(&Self::task_board_section_body(full))
    }

    /// 门控锚定**代码根**（`config::rsi_home`），不是 WORKSPACE_ROOT：
    /// 后者是用户项目，「新窗口」把它换成别的目录后这里会探测失败 →
    /// 看板段整段消失 → 子窗口永远不登记任务。
    fn task_board_section_body(full: bool) -> String {
        if !super::board_ctx::has_board_cli() {
            return String::new();
        }
        let brief = "\n## Task board (single-conversation UI)\n\
             The left panel is a TASK BOARD, not a session list: the user talks to you in one \
             continuous conversation and never manages project categories — you do that. When a \
             request becomes real work (not a one-liner), register it with \
             `python scripts/board.py` (bash is enabled by default; skill tools are not). \
             Board ops: `task-upsert --file <json|->`, `task-status <ID> <status> [--note ..]`, \
             `task-progress <ID> <done>/<total>`, `list`, `show <ID>`, `stats`. \
             The board lives in `.rsi/board.json`; do not put its content into every reply.\n\
             \n\
             ### task-upsert JSON schema (the only write path — get the shape right the first time)\n\
             `task-upsert --file plan.json` reads ONE JSON object. Minimal valid shape:\n\
             ```json\n\
             {\"title\": \"一句话标题\", \"status\": \"planning\", \"priority\": \"P1\",\n\
              \"plan\": [{\"step\": \"第一步\", \"status\": \"pending\", \"owner\": \"self\"}],\n\
              \"acceptance\": [\"可核对的验收项\"], \"resourcing\": {\"model\": \"current\", \"effort\": \"medium\"}}\n\
             ```\n\
             Field rules (these are enforced — a wrong value is REJECTED, not coerced):\n\
             - `id`: **omit it** and the CLI allocates one. If you set it, it must match \
             `T-YYYYMMDD-NN` (NN = 2~3 digits). Inventing an id like `T-foo-bar` is rejected.\n\
             - `status` (task level): exactly one of \
             `planning|doing|verifying|done|blocked|cancelled`. **`in_progress` is NOT a task \
             status** — it belongs to a plan step. Using it gets the whole upsert rejected.\n\
             - `plan[].status`: exactly one of `pending|in_progress|completed|cancelled`.\n\
             - `plan[].owner`: exactly one of `self|subagent|skill`.\n\
             - `acceptance`: an **ARRAY** of strings. A bare string is rejected.\n\
             - `priority`: `P0|P1|P2|P3`. `resourcing.model`: `current|cheap|strong`. \
             `resourcing.effort`: `small|medium|large`.\n\
             - `checks`: array of `{id, kind, name, target, timeout_secs}`; `kind` is \
             `cmd|file|grep|manual`.\n\
             - `outcomes`: `{brief, pro}` — both short strings; write them when moving to `done`.\n\
             The same schema is printed by `board.py task-upsert --help`; run it if unsure.\n\
             A rejected upsert means the task is NOT on the board — the user sees nothing, so \
             fix the shape and retry immediately.\n";
        if !full {
            return brief.to_string();
        }
        brief.to_string()
            + "\n### How to register a task (human-shaped thinking: people / money / materials)\n\
             Before executing, answer four questions and write them into the task:\n\
             1. **How is it done** → `goal` (one line) + `plan` (ordered steps, each `pending`/\
             `in_progress`/`completed`). If you cannot state the plan, you do not understand the task yet.\n\
             2. **Which department / roles** → `resourcing.skills` (existing skill names; if none fits, \
             that is exactly when a new skill is worth building — see the skill self-extension section).\n\
             3. **How much manpower (model) is efficient** → `resourcing.model` = `current` (stay on this \
             model), `cheap` (mechanical, high-volume, low-judgement work), or `strong` (judgement-heavy: \
             architecture, review, root-cause). Add `resourcing.effort` = `small`/`medium`/`large`. \
             Rule of thumb: do NOT spend a strong model on file shuffling, and do NOT spend a cheap one \
             on a decision you cannot verify afterwards.\n\
             4. **Which materials (tools)** → `resourcing.tools` (e.g. `bash`, `write_file`, `grep`, \
             `image_gen`). A task with no tool for its core step is not executable — say so instead of faking it.\n\
             Plus **the standard of good** → `acceptance`: 1-3 checkable items. `verifying`/`done` with an \
             empty `acceptance` is a self-report, not a verification (the CLI warns you; do not ignore it).\n\
             \nExample (write to a temp file, then `task-upsert --file`):\n\
             ```json\n\
             {\"title\": \"左栏改为任务看板\", \"goal\": \"UI 不再分会话层级，左侧只看任务与灵感\",\n\
              \"status\": \"planning\", \"priority\": \"P1\",\n\
              \"plan\": [{\"step\": \"数据契约\"}, {\"step\": \"Rust 路由\"}, {\"step\": \"前端\"}],\n\
              \"resourcing\": {\"model\": \"current\", \"effort\": \"large\",\n\
                              \"skills\": [\"project-dev\"], \"tools\": [\"bash\", \"write_file\"]},\n\
              \"acceptance\": [\"GET /api/board 返回任务与灵感\", \"左栏无会话列表\"]}\n\
             ```\n\
             Lifecycle: `planning → doing → verifying → done` (`blocked`/`cancelled` on the side). \
             Update at four moments only: task created, status changed, blocked, done. Do not narrate \
             the board in replies — the UI already shows it.\n\
             \n### Finishing a task = two lines of outcome (not one)\n\
             When you move a task to `done`, it owes the user TWO conclusions - write both into \
             `outcomes` (the CLI warns you when a `done` task has neither):\n\
             \x20 - `outcomes.brief` - **one sentence for the human**, in plain words: what is true \
             now that was not true before, and does the user need to do anything? No jargon, no file \
             paths, no test counts.\n\
             \x20 - `outcomes.pro` - **the professional conclusion for other agents**: what changed, \
             how it was verified, what is left. A later task of the same kind reads this via \
             `board.py digest`, so write the reusable part, not a narrative.\n\
             Why two: the point of the board is not that it is finished but what came out of it. \
             A `done` with no outcome sends the user back into the conversation to find out what \
             happened - the very thing the board exists to avoid.\n\
             Write them via `task-upsert` under the `outcomes` key: brief (for the human) and \
             pro (for agents). Both are optional individually, but a `done` task with neither \
             gets a warning, because that is exactly the case where the user cannot tell what happened.\n\
             \n### A substantial task also owes a survey of what the field knows\n\
             When the task solved a real problem, the user wants a second artifact besides the work \
             itself: what the state of the art and industry already do about it, what is genuinely \
             hard, and what this task contributed. Put that in the conversation or an `outputs/*.md` - \
             it is the raw material for turning the project record into a paper.\n\
             \n### @-references from the user\n\
             The user may write `@T-...` (a task) or `@I-...` (an inspiration) in a message. Resolve it \
             before answering: `python scripts/board.py show <ID>`. An inspiration the user wants acted on \
             becomes work via `task-upsert`. The task records the link to the inspiration; insp-adopt is gone. Inspirations are the user own words: read them freely and @I-... loads one, but you cannot create, retitle, re-tag, archive or delete them, and you must never write summaries of them back onto the board`.\n"
    }

    /// 判定本轮用户文本是否在「交代一件成规模的事」——任务看板完整规范按需注入的触发条件。
    ///
    /// 保守取词：宁可只给一行指针（brief 已说明「有这个权力」），也不要每轮灌长规范。
    /// 命中条件 = 有「任务性动作词」或「显式看板/任务词」；"在吗/谢谢/这是什么" 之类不触发。
    pub(super) fn user_wants_task_plan(user_text: &str) -> bool {
        let t = user_text.to_lowercase();
        if t.trim().len() < 4 {
            return false;
        }
        // 显式任务/看板语义
        if t.contains("任务") || t.contains("看板") || t.contains("计划") || t.contains("@t-") || t.contains("@i-") {
            return true;
        }
        // 成规模的交付动作（中英）
        [
            "做", "实现", "开发", "改造", "重构", "优化", "升级", "设计", "搭建", "迁移", "修复",
            "迭代", "接入", "拆分", "补齐", "落地", "部署", "调研", "审计", "写一", "帮我",
            "build", "implement", "refactor", "optimi", "design", "migrate", "fix", "add ", "upgrade",
            "integrate", "review", "audit", "research", "write a",
        ]
        .iter()
        .any(|kw| t.contains(kw))
    }

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

    /// R9：会话已加载技能集——由 Memory 侧 `session.skills` 汇总（trace 的 `skill_loaded`
    /// 事件为唯一真相源），**不新增循环可变态**（守 A1）。同会话新对话据此恢复技能作用域；
    /// 子代理新会话不继承（独立 trace）。
    ///
    /// 为什么不再由本层 `trace.read(after=0)` 逐页重放（P1 修复，改前必读）：
    /// ① `trace.read` 是分页接口（单页 ≤800 条 / 约 1MB），只读第一页必然漏掉长 trace
    ///    里第 800 条之后的 `skill_loaded`——技能作用域在长会话里静默丢失；
    /// ② 「会话已加载哪些技能」是**会话级事实**，应由持有 trace 的 Memory 汇总后提供，
    ///    而不是把 trace 的分页内部细节泄漏给编排层（漏页 bug 的根源就是这层泄漏）。
    /// 失败（内核超时/能力缺失）降级为空集，不阻断主流程。
    pub(super) async fn trace_loaded_skills(&self, env: &Envelope, session_id: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        if let Ok(v) = self
            .call(env, CAP_MEMORY, json!({"op": "session.skills", "session_id": session_id}), MEM_DEADLINE)
            .await
        {
            if let Some(skills) = v.get("skills").and_then(Value::as_array) {
                for s in skills {
                    if let Some(s) = s.as_str() {
                        if !s.is_empty() {
                            out.insert(s.to_string());
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
