//! 看板上下文装载（C 轮）：把 `@T-…` / `@I-…` 引用与在办任务摘要注入系统提示词。
//!
//! 为什么在 agent-loop 里直接读文件、而不是让模型先发一次 `board.py resolve`：
//! ① 引用是**输入的一部分**，不是待办事项——要模型先花一次工具往返才看见自己刚被
//! 引用的任务，多引用时就是 N 次往返；② 读一份本机 JSON 是纯 I/O，无插件、无沙箱、
//! 无网络，代价远小于一次工具波次；③ 失败必须静默（文件缺失/坏 JSON → 空串），
//! 绝不能因为看板坏了让 chat 起不来。
//!
//! 空间契约：只读 `WORKSPACE_ROOT/.rsi/board.json`（`RSI_BOARD_PATH` 可覆盖），
//! **不写**——写通道唯一是 `scripts/board.py`（agent）与 host 的 `/api/board`（UI）。
//! 时间契约：每次 chat 现读现渲染，不缓存（看板是分钟级变化，缓存只会让引用读到旧态）。
//!
//! 可测性契约（本轮踩过的坑）：**渲染与 I/O 分离**。`render_refs` / `render_open_tasks`
//! 是纯函数（输入 board Value），`refs_context` 才做路径解析与读文件。初版把读文件
//! 与渲染揉在一起，测试只能 `set_var("WORKSPACE_ROOT")`——而 cargo 多测共享进程环境，
//! 与既有 `self_extension_section_gated_by_workspace_reachability` 直接串扰（实测红）。
//!
//! 有界：整块 ≤ `MAX_BLOCK_CHARS`，在办摘要 ≤ `MAX_OPEN_TASKS` 条——防止 200 条任务的
//! 看板把每轮 prompt 撑爆（这是「看板不进上下文」纪律被有条件放宽的代价，必须显式设限）。

use serde_json::Value;
use std::path::PathBuf;

/// 注入块字符上限；超出即截断（宁可少给，不可撑爆上下文）。
const MAX_BLOCK_CHARS: usize = 4000;
/// 无引用时的在办任务摘要条数上限。
const MAX_OPEN_TASKS: usize = 12;
/// 单条引用里计划/验收/检查的展示条数上限。
const MAX_ITEMS: usize = 6;

/// 看板数据的根目录（`RSI_BOARD_PATH` 相对路径的锚点）。
///
/// v12（I-20261108-14 的漏改一侧）：v11 把 UI/CLI 的数据根统一到 `RSI_HOME`，
/// 但**本文件漏了**——原来这里独占 `WORKSPACE_ROOT`。主窗口里两者恰好同源
/// （都是代码根），所以看不出来；子窗口（`instances.rs` 把 `WORKSPACE_ROOT` 设成
/// 用户项目、`RSI_HOME` 设成代码根）里两者必然分叉，而用户项目下没有
/// `.rsi/board.json` → `load_board()` 返回 `None` → `refs_context` 静默给空串。
///
/// 后果不是「少一段提示」，而是**提示词在说谎**：`board_ops_hint` 已锚代码根
/// （`has_board_cli`），所以子窗口里「@引用会自动装载、不要再 show」照样注入，
/// 而装载实际什么都没发生——用户 @ 了任务，模型却看不到。
///
/// 判定顺序与 `host/src/web/api/board.rs::data_root`、`scripts/board.py::data_root`
/// 逐字一致：`RSI_HOME` → `WORKSPACE_ROOT` → cwd。三处必须同源，否则会出现
/// 「UI 写 A 文件、agent 读 B 文件」——最难查的一类不一致。
fn data_root() -> Option<PathBuf> {
    for key in ["RSI_HOME", "WORKSPACE_ROOT"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    std::env::current_dir().ok()
}

fn board_path() -> Option<PathBuf> {
    let raw = std::env::var("RSI_BOARD_PATH").unwrap_or_default();
    let raw = raw.trim();
    let p = if raw.is_empty() {
        PathBuf::from(".rsi").join("board.json")
    } else {
        PathBuf::from(raw)
    };
    if p.is_absolute() {
        Some(p)
    } else {
        data_root().map(|root| root.join(p))
    }
}

fn load_board() -> Option<Value> {
    let p = board_path()?;
    let text = std::fs::read_to_string(p).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.is_object().then_some(v)
}

/// 从任意文本里抽出 `@T-…` / `@I-…` 引用（去重保序）。
///
/// 手写扫描而非正则：形状固定为 `@` + 单字母前缀 + `-` + 8 位日期 + `-` + 2~3 位序号，
/// 且本 crate 无 regex 依赖（不为一个 token 抽取加 crate）。松匹配会把 `@Tomorrow`
/// 当引用，紧匹配会漏真引用——两个方向都有代价，故按 ID 形状严格判定。
pub(super) fn extract_refs(text: &str) -> Vec<String> {
    let cs: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 1 < cs.len() {
        if cs[i] == '@' && (cs[i + 1] == 'T' || cs[i + 1] == 'I') {
            let start = i + 1;
            let mut j = start + 1;
            while j < cs.len() && (cs[j].is_ascii_digit() || cs[j] == '-') {
                j += 1;
            }
            let cand: String = cs[start..j].iter().collect();
            let parts: Vec<&str> = cand.split('-').collect();
            let ok = parts.len() == 3
                && parts[0].len() == 1
                && parts[1].len() == 8
                && parts[1].chars().all(|c| c.is_ascii_digit())
                && (parts[2].len() == 2 || parts[2].len() == 3)
                && parts[2].chars().all(|c| c.is_ascii_digit());
            if ok && !out.contains(&cand) {
                out.push(cand);
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn arr<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn strs(v: &Value, key: &str) -> Vec<String> {
    arr(v, key)
        .iter()
        .filter_map(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn or_default<'a>(v: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    let got = s(v, key);
    if got.is_empty() {
        fallback
    } else {
        got
    }
}

/// 单个任务的引用块（含目标/计划/验收/检查结论/人财物/产物）。
fn render_task(t: &Value) -> String {
    let prog = t.get("progress").cloned().unwrap_or(Value::Null);
    let ev = t.get("evaluation").cloned().unwrap_or(Value::Null);
    let res = t.get("resourcing").cloned().unwrap_or(Value::Null);
    let done = u(&prog, "done");
    let total = u(&prog, "total");
    let note = s(&prog, "note");
    let mut out = format!(
        "- @{} 「{}」 status={} priority={} progress={}{} verdict={}\n",
        s(t, "id"),
        truncate(s(t, "title"), 80),
        s(t, "status"),
        s(t, "priority"),
        if total > 0 { format!("{done}/{total}") } else { "-".into() },
        if note.is_empty() { String::new() } else { format!(" ({})", truncate(note, 60)) },
        or_default(&ev, "verdict", "none"),
    );
    let goal = s(t, "goal");
    if !goal.is_empty() {
        out.push_str(&format!("  goal: {}\n", truncate(goal, 160)));
    }
    let plan = arr(t, "plan");
    if !plan.is_empty() {
        let items: Vec<String> = plan
            .iter()
            .take(MAX_ITEMS)
            .map(|p| {
                let mark = match s(p, "status") {
                    "completed" => "[x]",
                    "in_progress" => "[~]",
                    "cancelled" => "[-]",
                    _ => "[ ]",
                };
                format!("{mark} {}", truncate(s(p, "step"), 48))
            })
            .collect();
        out.push_str(&format!("  plan: {}\n", items.join(" / ")));
    }
    let acc = arr(t, "acceptance");
    if !acc.is_empty() {
        let list: Vec<String> = acc
            .iter()
            .take(MAX_ITEMS)
            .map(|a| truncate(a.as_str().unwrap_or(""), 60))
            .collect();
        out.push_str(&format!("  acceptance: {}\n", list.join("；")));
    }
    let checks = arr(t, "checks");
    if !checks.is_empty() {
        let list: Vec<String> = checks
            .iter()
            .take(MAX_ITEMS)
            .map(|c| format!("{}={}", s(c, "id"), or_default(c, "status", "pending")))
            .collect();
        out.push_str(&format!(
            "  checks: {} (pass={} fail={} skip={})\n",
            list.join(" "),
            u(&ev, "pass"),
            u(&ev, "fail"),
            u(&ev, "skip"),
        ));
    }
    let skills = strs(&res, "skills");
    if !skills.is_empty() {
        out.push_str(&format!(
            "  resourcing: model={} effort={} skills={}\n",
            or_default(&res, "model", "current"),
            or_default(&res, "effort", "medium"),
            skills.join(",")
        ));
    }
    let arts = strs(t, "artifacts");
    if !arts.is_empty() {
        out.push_str(&format!("  artifacts: {}\n", arts.join(" ")));
    }
    // G5（I-20261108-04 第 3 条）：任务被 @ 引用时也要带上**结果**，不只有过程。
    // 此前 render_task 给的是 goal/plan/acceptance/checks——全是「打算怎么做」，
    // 缺「最后做成了什么」。后果是引用一件已完成任务时 agent 仍要翻对话才知道结论，
    // 跨任务归纳（digest）同理。两条线都进上下文：
    //   brief = 用户视角的一句话结论（复述它即可，不必自己组织话术）
    //   pro   = 专业结论（判断「能不能复用、还差什么」的依据）
    let outs = t.get("outcomes").cloned().unwrap_or(Value::Null);
    let brief = s(&outs, "brief");
    if !brief.is_empty() {
        out.push_str(&format!("  outcome: {}\n", truncate(brief, 200)));
    }
    let pro = s(&outs, "pro");
    if !pro.is_empty() {
        out.push_str(&format!("  outcome_detail: {}\n", truncate(pro, 300)));
    }
    out
}

/// 单条灵感的引用块（原文 + 标签 + 已采纳的任务）。
fn render_inspiration(i: &Value) -> String {
    let tags = strs(i, "tags");
    let mut out = format!(
        "- @{} 「{}」 status={}{}\n",
        s(i, "id"),
        truncate(s(i, "text"), 200),
        s(i, "status"),
        if tags.is_empty() { String::new() } else { format!(" tags={}", tags.join(",")) }
    );
    let tasks = strs(i, "tasks");
    if !tasks.is_empty() {
        out.push_str(&format!("  adopted_by: {}\n", tasks.join(",")));
    }
    out
}

/// 纯函数：把用户文本里的引用渲染成上下文块（不读文件、不改环境）。
///
/// 三种结局都要**明确**，不能静默：命中 → 装载；全部未命中 → 只回报缺失（空壳 header
/// 比不注入更误导）；无引用 → 空串（调用方决定是否退化为在办摘要）。
pub(super) fn render_refs(user_text: &str, board: &Value) -> String {
    let tasks = arr(board, "tasks");
    let insps = arr(board, "inspirations");
    let refs = extract_refs(user_text);
    if refs.is_empty() {
        return String::new();
    }
    let mut body = String::new();
    let mut missing: Vec<String> = Vec::new();
    for r in &refs {
        if let Some(t) = tasks.iter().find(|t| s(t, "id") == r.as_str()) {
            body.push_str(&render_task(t));
        } else if let Some(i) = insps.iter().find(|i| s(i, "id") == r.as_str()) {
            body.push_str(&render_inspiration(i));
        } else {
            missing.push(r.clone());
        }
    }
    if body.is_empty() {
        return format!(
            "\n## Board references\n- {} not on the board (stale @-reference); ignore it or ask the user.\n",
            missing.join(", ")
        );
    }
    let mut block = String::from(
        "\n## Board references (auto-loaded from .rsi/board.json)\n\
         The user just @-referenced these board items; treat them as already-read context \
         (do not call `board.py show` again for them).\n",
    );
    block.push_str(&body);
    if !missing.is_empty() {
        block.push_str(&format!("- missing (not on the board): {}\n", missing.join(", ")));
    }
    truncate(&block, MAX_BLOCK_CHARS)
}

/// 纯函数：在办任务摘要（done/cancelled 排除，按看板顺序取前 N 条）。
pub(super) fn render_open_tasks(board: &Value) -> String {
    let open: Vec<&Value> = arr(board, "tasks")
        .into_iter()
        .filter(|t| !matches!(s(t, "status"), "done" | "cancelled"))
        .collect();
    if open.is_empty() {
        return String::new();
    }
    let mut block = String::from(
        "\n## Open tasks on the board (digest; `python scripts/board.py digest --skill <name>` \
         for same-kind history)\n",
    );
    for t in open.iter().take(MAX_OPEN_TASKS) {
        let prog = t.get("progress").cloned().unwrap_or(Value::Null);
        let done = u(&prog, "done");
        let total = u(&prog, "total");
        block.push_str(&format!(
            "- {} 「{}」 {} {} {}\n",
            s(t, "id"),
            truncate(s(t, "title"), 60),
            s(t, "status"),
            s(t, "priority"),
            if total > 0 { format!("{done}/{total}") } else { String::new() }
        ));
    }
    if open.len() > MAX_OPEN_TASKS {
        block.push_str(&format!(
            "- …共 {} 个未完成（`board.py list --status doing` 看全部）\n",
            open.len()
        ));
    }
    truncate(&block, MAX_BLOCK_CHARS)
}

/// I/O 入口：读板 → 引用优先；无引用且本轮像「交代一件成规模的事」→ 在办摘要。
/// 读不到板（无工作区 / 无文件 / 坏 JSON）一律空串——看板坏了不能拖垮 chat。
pub(super) fn refs_context(user_text: &str, want_plan: bool) -> String {
    let Some(board) = load_board() else {
        return String::new();
    };
    let refs = render_refs(user_text, &board);
    if !refs.is_empty() {
        return refs;
    }
    if want_plan {
        render_open_tasks(&board)
    } else {
        String::new()
    }
}

/// 看板通道补充说明（C 轮新增的五个 op）。
///
/// 为什么追加在独立段而不是改写 `skills.rs::task_board_section`：那段是 B 轮落地的长
/// 字符串字面量（含续行转义），改它等于重写整块提示词，锚点脆弱；这里新增一段，
/// 并纠正一处被本轮改变的事实——@引用现在自动装载，不再需要模型自己去 `show`。
pub(super) fn board_ops_hint(full: bool, has_cli: bool) -> String {
    if !has_cli {
        return String::new();
    }
    let mut out = String::from(
        "\n### Board ops added in this build\n\
         - `@T-...` / `@I-...` are **auto-loaded** into this prompt (see \"Board references\"); \
         do NOT run `board.py show` again for them.\n\
         - `board.py resolve \"<text>\"` — same resolution for text you are handed indirectly \
         (e.g. a queued message, a file).\n\
         - `board.py digest [--skill <name>] [--brief]` — same-kind history across tasks: use it \
         when the user asks \"how did we do this before\" or before re-inventing a workflow.\n\
         - `board.py layout` / `layout --file <json>` — the left panel is assembled from \
         `layout.sections` (kinds: tasks | metrics | inspirations, order fixed by kind).\n\
         \n### Registering is part of doing the work, not paperwork after it\n\
         The board is the user's only view of what you are doing. If you never register a task, \
         the work is invisible to them even after it is finished — they will ask \"I gave you a job \
         and the board shows nothing\". That is a delivery failure, not a UI problem.\n\
         - **Register BEFORE the first substantive edit**, not after. One `task-upsert` call at \
         the moment the request turns into real work; then update it at the four moments the \
         board cares about (created / status changed / blocked / done).\n\
         - **Do not wait to be asked.** \"The user did not ask for a plan\" is not a reason to skip \
         the entry — they asked for the work, and the entry is how they watch it happen.\n\
         - The workspace tag is filled in automatically from `WORKSPACE_ROOT`; do NOT pass \
         `workspace` unless you are deliberately recording work for a different project.\n\
         - One-line answers, pure lookups and single trivial edits do NOT get a task. The line is \
         \"would the user want to see progress on this?\", not \"did I touch a file?\".\n",
    );
    if full {
        out.push_str(
            "\n### Verification = evidence, not self-report (checks)\n\
             `acceptance` is the human-readable standard; `checks` is the machine-runnable proof. \
             Add 1-3 checks when the task has a runnable criterion, then run them:\n\
             ```json\n\
             \"checks\": [\n\
               {\"id\": \"c1\", \"kind\": \"cmd\",  \"name\": \"pytest 全绿\", \"target\": \"python -m pytest tests -q\", \"timeout_secs\": 120},\n\
               {\"id\": \"c2\", \"kind\": \"file\", \"name\": \"产物落盘\", \"target\": \"outputs/report.md\"},\n\
               {\"id\": \"c3\", \"kind\": \"grep\", \"name\": \"旧入口已删\", \"target\": \"postChat\", \"scope\": \"runtime/crates/host/web-dist/vendor\"},\n\
               {\"id\": \"c4\", \"kind\": \"manual\", \"name\": \"用户确认文案\"}\n\
             ]\n\
             ```\n\
             `cmd` exits 0 = pass (cwd = workspace); `file` = existence; `grep` = substring hit \
             under `scope`; `manual` always `skip` (a human judges it — you must not grade yourself).\n\
             Run `python scripts/board.py eval <ID>`; it writes `evaluation.verdict` and **downgrades \
             verifying/done back to doing when a check fails**. `done` without a passing evaluation \
             on a task that has checks is a self-report (the CLI warns you).\n\
             \n### Same-kind history (context management)\n\
             The board is where repeated work converges. Before starting something that smells \
             familiar, `board.py digest --skill <name> --brief` and reuse what was learned (plan \
             shape, artifacts, what failed). After finishing, keep the task's `plan`, `resourcing` \
             and `artifacts` accurate — a later digest is only as good as what you wrote now. \
             Keep digests out of the conversation: query when needed, do not paste them.\n",
        );
    }
    abs_cli_cmds(&out)
}

/// 看板 CLI 的绝对路径（锚定**代码根**，见 `crate::config::rsi_home`）。
///
/// 为什么返回路径而不是 bool：提示词里必须给出**可直接执行**的命令。
/// 子窗口的 cwd 是用户项目，`python scripts/board.py` 在那儿并不存在，
/// 只给相对命令等于让模型撞一次「文件不存在」后放弃登记任务。
pub(super) fn board_cli_path() -> Option<PathBuf> {
    let p = super::config::rsi_home()?.join("scripts").join("board.py");
    p.is_file().then_some(p)
}

/// 是否有看板 CLI——决定是否注入看板通道段。
///
/// 探测点是**代码根**而非工作区，理由见 `config::rsi_home` 的文档：能力属于这套工具，
/// 不该因为用户换了个项目目录就消失。
pub(super) fn has_board_cli() -> bool {
    board_cli_path().is_some()
}

/// 把提示词里的相对命令 `python scripts/board.py` 重写成**绝对命令**。
///
/// 为什么在末端统一替换，而不是逐处改文案：
/// ① 该相对命令在提示词里出现 8 次，跨 3 个函数与数段长字符串字面量，逐处改要
///    同时碰 `\` 续行转义与 JSON 示例里的 `{}`（`format!` 会直接 panic），
///    每一处都可能差一个字符；
/// ② 「CLI 在哪」应当只有**一个来源**（`board_cli_path`），而不是散落在文案里的事实。
/// 反斜杠统一成 `/`（cmd 与 bash 都认）；含空格时加引号——本机工作区路径实测带空格
/// （`D:/ZH/vibe code/...`），不加引号会被 shell 拆成两段直接报错。
/// 找不到 CLI 时原样返回（退化场景，此时调用方本就不注入这一段）。
pub(super) fn abs_cli_cmds(text: &str) -> String {
    let Some(p) = board_cli_path() else {
        return text.to_string();
    };
    let abs = p.to_string_lossy().replace('\\', "/");
    let cmd = if abs.contains(' ') {
        format!("python \"{abs}\"")
    } else {
        format!("python {abs}")
    };
    text.replace("python scripts/board.py", &cmd)
}

/// D 轮：任务分解与委派纪律（I-20261107-01 的落点）。
///
/// 用户原话：「具体计划怎么执行，需要哪些部门哪些人，是否需要进一步的进行细分。
/// 这些 agent 需要自行在计划中写清楚，全盘考虑，是否需要调用子代理等。考虑好任务的
/// 复杂度。」
///
/// 为什么做成 plan 步上的 `owner` 字段而不是一段「请考虑复杂度」的软提示：
/// 软提示的产物是模型的自述（"这个任务比较复杂，我会分解"），无法核对；字段化之后
/// 每一步都必须回答「谁干」，验收时能数出来（几步委派、几步要现造技能）。
///
/// 三种 owner 的判据（写进提示词，避免模型凭感觉选）：
/// - `self`：需要本轮上下文才能做的步（改刚读过的那个函数、按上一次输出继续）；
/// - `subagent`：可自包含且**产物是结论而非改动**的步（大范围调研、跨文件阅读后总结、
///   多方案对比）——把它们的原文阅读留在子会话里，主上下文只收结论；
/// - `skill`：仓库里没有现成能力、要现造技能/工具才能做的步（这是「物力自造」的
///   入口：先写 SKILL.md 或 tools.json，再按 plan 走）。
///
/// 什么时候**才**该把一件事拆成多个任务（parent）：拆开的价值是「各自可独立验收」，
/// 若子件验收条件与父任务相同，拆了只是把进度条切成两段——那种情况就别拆。
pub(super) fn decompose_hint() -> String {
    if !has_board_cli() {
        return String::new();
    }
    String::from(
        "\n### Decomposition & delegation (`plan[].owner`, `parent`)\n\
         Each plan step records **who executes it** — this is how \"how complex is this, do I need \
         a sub-agent\" becomes checkable instead of self-reported:\n\
         - `owner: \"self\"` — needs this turn's context (edit the function you just read, continue \
         from the last output).\n\
         - `owner: \"subagent\"` — self-contained, and the deliverable is a *conclusion*, not an edit: \
         broad research, reading many files to summarise, comparing options. The raw reading stays \
         in the sub-session; this context only receives the answer.\n\
         - `owner: \"skill\"` — no existing capability in this workspace: author a skill/tool first \
         (skills self-extension), then run the step.\n\
         Default is `self` when omitted; do not mark everything `subagent` — delegation costs a \
         fresh context that knows nothing about this conversation.\n\
         \n\
         Split a request into child tasks (each with its own `parent: \"T-...\"`) **only when the \
         children are independently verifiable**. If a child's acceptance criteria are the same as \
         the parent's, splitting just cuts the progress bar in two — don't.\n\
         ```json\n\
         \"plan\": [\n\
           {\"step\": \"读现有看板实现，定位写通道\", \"status\": \"in_progress\", \"owner\": \"self\"},\n\
           {\"step\": \"调研同类工具怎么表达『人财物』\", \"owner\": \"subagent\"},\n\
           {\"step\": \"现造一个导出 CSV 的工具\", \"owner\": \"skill\"}\n\
         ]\n\
         ```\n\
         Sub-tasks: `task-upsert` with `\"parent\": \"T-...\"`; deleting a parent never deletes \
         children — `board.py task-rm` detaches them (`parent` cleared).\n",
    )
}

/// 并行判据段（G4b，I-20261108-06 的正身）。
///
/// 为什么单列一段而不是塞进 `decompose_hint`：`decompose_hint` 讲的是「一步步交给谁」，
/// 这段讲的是「几步能不能同时干」——两件事，锚点分开才能各自回退。
///
/// 为什么必须写进提示词：工具波次本来就是并发的（`chat.rs` 的 `join_all`），子代理也在
/// 同一波次里跑——**技术底座早就有了，缺的一直是「什么时候该并行」这条判断**。
/// 不写判据的后果不是「不能并行」，而是模型随机并行：撞同一个文件、同一个
/// `target/` 目录，把工作区搅成互相覆盖的烂摊子，再花一轮去修。
pub(super) fn parallel_hint() -> String {
    String::from(
        "\n### Parallel vs serial: you decide, the user only wants the result\n\
         Tool calls in one wave already run concurrently, and `task` sub-agents run in that same \
         wave. So \"can this be parallel\" is a judgement you make — not a switch the user turns on. \
         The user should never have to know how many agents ran; they get one merged answer.\n\
         Run steps **in parallel** when they are mutually independent:\n\
         \x20 - different files: nothing one writes is read by another;\n\
         \x20 - different verification: no shared build dir, no shared port, no shared lock file;\n\
         \x20 - no ordering: B does not need A's output before it can start.\n\
         Keep them **serial** when they collide:\n\
         \x20 - the same file, or the same `target/` / `node_modules/` / build cache;\n\
         \x20 - one step's input is another step's output — then the order *is* the work;\n\
         \x20 - both steps write the board: writes are now mutually exclusive (a second writer gets \
         a lock error, not a lost entry), but serial is still cheaper than retrying;\n\
         \x20 - it is one edit in one file: splitting it just makes two agents fight over it.\n\
         **When unsure, go serial.** A wrong parallel attempt costs a corrupted tree and a \
         confusing answer; a serial run costs only wall time — and wall time is the cheaper of the two.\n\
         Parallel work still owes **one** combined result: merge the sub-results into a single \
         answer, do not paste N sub-agent reports and call it done.\n",
    )
}

/// S 轮：入口分流（三档）。**只判档位，不改门控**——`user_wants_task_plan` 那个布尔门
/// 决定「要不要注入看板规范全文」，本条决定「注入了之后按多重的流程走」。
///
/// 为什么必须有（trace s-mw5uq7sz 实测）：15 轮 chat 共 1922 次工具调用、单轮最多
/// 245 rounds，`Get-Content README.md` 这种活也走完「探索 → 计划 → 看板 → 验证」。
/// 既有提示词只有「像任务 / 不像任务」两态，缺的正是「像任务，但小到不值得开全套流程」。
///
/// 判据为什么不写行数/token：模型无法可靠自估这两个量，写了等于没写。
/// 改用**可观测的对象数**（几个文件、几个模块、要不要先出方案）——动手前看目录就能数出来。
pub(super) fn routing_hint() -> String {
    if !has_board_cli() {
        return String::new();
    }
    String::from(
        "\n### Pick the cost tier BEFORE you act (S / M / L)\n\
         Most turns are small. Running the full explore-plan-verify pipeline on a one-line question \
         is the single biggest source of wasted tool calls in this workspace, so pick a tier first \
         and let the tier decide how much process you owe.\n\
         \x20 - **S — answer directly.** One question, one command, a one-line fix in one file, or a \
         pure lookup. Do it and reply. No todo list, no board entry, no plan, no repo exploration. \
         If you can name the single file you will touch, it is S.\n\
         \x20 - **M — small change.** One module or a couple of files, and the acceptance is one \
         sentence you could hand to a colleague. Read the target, change it, verify it (compile it, \
         run the one relevant test, or smoke it). Update the plan steps of an EXISTING task if one \
         covers this; do not open a new task for it.\n\
         \x20 - **L — project-level.** Several modules, a contract change that crosses files, a new \
         project, a reverse-engineering request, or the user asked for a plan. Load `project-dev` \
         FIRST and follow its mode decision; do not start editing before the plan exists.\n\
         Deciding in one breath: **how many objects will change?** one → S, a few in one module → M, \
         many or across modules → L. Unsure between two tiers: take the CHEAPER one and escalate \
         only when a fact forces it (a shared build dir appears, a contract breaks, the user asks \
         for a plan). Cheap-and-wrong costs one extra round; expensive-and-unneeded costs twenty.\n\
         Two invariants regardless of tier: verification is never skipped for M/L, and a registered \
         task is never opened just to look busy.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 进程级 env 是共享可变状态：改 env 的用例必须串行，否则彼此串扰而红。
    /// v12：用 crate 根那把锁（`crate::ENV_LOCK`）——本模块与 `lib.rs` 测试模块
    /// 改的是同一组 key，各持一把等于没加锁。
    use crate::ENV_LOCK;

    /// 用例结束自动还原 env——断言中途 panic 也要还原，否则后续用例读到脏值。
    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            Self { saved: keys.iter().map(|k| (*k, std::env::var_os(k))).collect() }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// 子窗口形态：WORKSPACE_ROOT 是用户项目、RSI_HOME 是代码根。
    ///
    /// 这是 v12 修的 bug 本身。v11 把 UI/CLI 的数据根搬到代码根，本文件漏改，
    /// 于是子窗口里 board_path 指向「用户项目/.rsi/board.json」——那个文件不存在，
    /// load_board() 返回 None，@引用与在办摘要全部静默为空，而提示词里「@引用会自动
    /// 装载」那段照样注入：**提示词在说谎**。
    ///
    /// 断言的是「读到的是代码根那份」，而非路径形状：路径可随判据顺序调整，
    /// 「读谁的数据」才是这一处的不变量。
    #[test]
    fn board_follows_rsi_home_not_workspace() {
        use std::io::Write;
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::new(&["RSI_HOME", "WORKSPACE_ROOT", "RSI_BOARD_PATH"]);
        std::env::remove_var("RSI_BOARD_PATH");

        let tmp = std::env::temp_dir().join(format!("ra-bctx-{}", std::process::id()));
        let home = tmp.join("code-root");
        let proj = tmp.join("user-project");
        for d in [home.join(".rsi"), proj.join(".rsi")] {
            std::fs::create_dir_all(&d).expect("mkdir");
        }
        // 代码根那份**有**任务；用户项目那份先**故意为空**（子窗口真实形态）。
        let mut f = std::fs::File::create(home.join(".rsi").join("board.json")).expect("create");
        f.write_all(br#"{"tasks":[{"id":"T-20260101-01","title":"from code root","status":"doing"}]}"#)
            .expect("write");
        drop(f);

        std::env::set_var("RSI_HOME", &home);
        std::env::set_var("WORKSPACE_ROOT", &proj);

        let loaded = load_board().expect("必须读到代码根那份；读空即 v12 bug 复发");
        assert_eq!(
            loaded["tasks"][0]["title"].as_str().unwrap_or(""),
            "from code root",
            "读到了错误的工作区账本: {loaded}"
        );
        // 引用渲染是这条链的最终消费者：修好之前这里恒为空串。
        let ctx = render_refs("@T-20260101-01 看下这个", &loaded);
        assert!(ctx.contains("from code root"), "@引用装载必须命中: {ctx}");

        // 反过来：用户项目那份写了也别读——数据根只有一处，两处都读等于又拆成两本账。
        std::fs::write(
            proj.join(".rsi").join("board.json"),
            br#"{"tasks":[{"id":"T-20260101-02","title":"from workspace","status":"doing"}]}"#,
        )
        .expect("write proj");
        assert_eq!(
            load_board().expect("board")["tasks"][0]["title"].as_str().unwrap_or(""),
            "from code root",
            "工作区有板也不该被读——那正是拆板的形态"
        );

        // 只有 WORKSPACE_ROOT（主窗口 / 老部署）时仍要能读：修子窗口不能弄坏主窗口。
        std::env::remove_var("RSI_HOME");
        assert_eq!(
            load_board().expect("退化形态必须继续可用")["tasks"][0]["title"].as_str().unwrap_or(""),
            "from workspace",
            "无 RSI_HOME 时应退化到工作区"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 空白 env 必须被当成「未设置」，而不是「数据根是空路径」。
    ///
    /// 若把空串当有效值，`PathBuf::from("").join(".rsi/board.json")` 得到相对路径，
    /// 数据落点就由 cwd 决定——静默漂移到「启动目录里的 .rsi」。
    #[test]
    fn blank_env_falls_through_to_next_key() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::new(&["RSI_HOME", "WORKSPACE_ROOT", "RSI_BOARD_PATH"]);
        std::env::remove_var("RSI_BOARD_PATH");
        std::env::set_var("RSI_HOME", "   ");
        std::env::set_var("WORKSPACE_ROOT", "D:/fake-ws");
        let p = board_path().expect("path");
        assert!(
            p.to_string_lossy().replace('\\', "/").starts_with("D:/fake-ws/"),
            "空白 RSI_HOME 应被跳过: {p:?}"
        );
    }

    /// 显式 RSI_BOARD_PATH 覆盖仍优先（测试隔离与自定义账本位置靠它）。
    #[test]
    fn explicit_board_path_overrides_data_root() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::new(&["RSI_HOME", "WORKSPACE_ROOT", "RSI_BOARD_PATH"]);
        std::env::set_var("RSI_HOME", "D:/code-root");
        std::env::set_var("RSI_BOARD_PATH", "D:/custom/board.json");
        assert_eq!(board_path().expect("path"), PathBuf::from("D:/custom/board.json"));
    }

    /// 渲染层纯函数不碰 env：给什么 board 就渲染什么（可测性契约）。
    #[test]
    fn render_refs_is_pure_over_given_board() {
        let board = json!({"tasks": [{"id": "T-20260101-01", "title": "t", "status": "doing"}],
                           "inspirations": [{"id": "I-20260101-01", "text": "idea", "status": "open"}]});
        let out = render_refs("@T-20260101-01 @I-20260101-01", &board);
        assert!(out.contains("T-20260101-01"));
        assert!(out.contains("idea"));
        // 未命中的引用要**明说**未命中，而不是静默丢掉（空壳 header 更误导）。
        let miss = render_refs("@T-20260101-99", &board);
        assert!(miss.contains("not on the board"), "缺失引用必须显式回报: {miss}");
    }
}
