//! P2：Magpie 路由组管理（Host 安全 API）。
//!
//! 为什么必须走 CLI：Magpie 没有组的机器可读出口——`groups --json` / `-o json` /
//! `--output json` 都被忽略（仍吐人类文本），`group show --json` 直接失败，网关侧
//! `/v1/magpie/groups`、`/api/groups`、`/groups`、`/routing/groups` 全 404。组只存在于
//! Magpie 自己的 providers.json，**不直接改那个文件**：那是 Magpie 的运行时状态，
//! 改了会被它下一次写盘覆盖，且绕过它自己的校验。
//!
//! 安全约定（本文件内强制，前端不得绕过）：
//! - 一切 CLI 调用走 `std::process::Command::args(...)`，**不拼 shell 字符串**。
//! - stdin 一律 null：无 method 的交互菜单会读 stdin → EOF。
//! - stdout 与 stderr 分开取：stderr 常有 Trae 登录过期等日志，**不能**因此判失败；
//!   成败只看退出码。
//! - 内部 id、成员、routing、stays 在**进 CLI 之前**先自校验，错误文案给中文原因。
//!
//! 语义（全部来自实测，见 .scratch/probe_groups_semantics*.txt、.scratch/probe_p2_*.txt）：
//! - public id 分两类：`auto/<model>`（Magpie 自己发现的组）与 `group/<内部id>`（用户组）。
//! - `auto/<model>` 的判定（实测 p8–p13，见 .scratch/p2-auto-rule.md）：
//!   目标模型名 = **某个成员模型 id 的最后一段**（不是 provider 之后的完整路径）；
//!   内部 id 必须精确等于 `auto-<slug(目标名)>`（小写，非 `[a-z0-9]` 变 `-`）。
//!   成员可混合：只要有一个成员算出目标名即可。
//!   effort 后缀（`:high` 等）不计入目标名；字面 `:free` 是模型 id 的一部分，要计入。
//! - `group add` 不接受 `id=`（未声明参数被静默忽略），内部 id 必须后续用
//!   `group set <ref> id=...` 设置；该步在 id 已被占用时报 "there is a group ... already"。
//! - `group add` 同名会**替换**原组（退出码仍是 0），因此创建前必须预检同名/同 id。
//! - 删除内置 found 组是「隐藏」（写 `{"hidden":true}` 记录），`group restore` 可复原；
//!   删除用户组是真正删除。两者 stdout 文案不同，据此区分。
//! - `magpie groups` 列表不含 stays，必须逐组 `group show`。
//! - 成员 id 里可以带字面 `:free` / `:7b` / `:0` 后缀（那是模型 id 的一部分），
//!   只有已知 effort 值才按 reasoning effort 解释。

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::net::TcpStream;

use super::super::{bad_request, dispatch_or_err, json_resp};
use agent_kernel_kernel::Kernel;

// ───────────────────────── 常量 ─────────────────────────

/// routing 枚举（CLI 原文：`routing "x" is not one ... has: smart, order, rotate,
/// usage, pace, weight, free, fast, cheap, drain, manual, or <plugin>/<name>`）。
/// `<plugin>/<name>` 形态放行（网关插件的自定义路由）。
pub(crate) const ROUTING_KINDS: &[&str] = &[
    "smart", "order", "rotate", "usage", "pace", "weight", "free", "fast", "cheap", "drain", "manual",
];

/// stays 枚举（CLI 原文：`stays "x" is not one ... has: auto, session, turn, off, strict`）。
pub(crate) const STAYS_KINDS: &[&str] = &["auto", "session", "turn", "off", "strict"];

/// reasoning effort 枚举（CLI 原文：`provider/model:<effort>` 的 effort 取值）。
pub(crate) const EFFORT_KINDS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `group show` 并发上限（逐组读；读多写少，限量避免进程风暴）。
const SHOW_CONCURRENCY: usize = 4;

// ───────────────────────── 校验 ─────────────────────────

/// 内部 id 规则：小写 ASCII 字母/数字/点/短横线；不许 `/`、空格、下划线、
/// 首尾短横线、连续双短横线。
///
/// CLI 自己**没有**强制小写（实测 `RSI-T5` 被接受），所以这里必须自己收严。
pub(crate) fn valid_internal_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    if id.starts_with('-') || id.ends_with('-') {
        return false;
    }
    if id.contains("--") {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
}

/// 成员 id 形状：`provider/model`（可多段路径，如 `kilo/reflection/beam-free`），
/// 或嵌套组引用 `group/<id>`。
///
/// 不许空白（`models=` 是逗号分隔的等值参数）、逗号、等号、引号——它们会破坏
/// CLI 的参数解析。
pub(crate) fn valid_member_shape(member: &str) -> bool {
    if member.is_empty() || member.len() > 200 {
        return false;
    }
    if member.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    if member.contains([',', '=', '"', '\'', ';', '&', '|', '>', '<']) {
        return false;
    }
    if !member.contains('/') {
        return false;
    }
    if member.starts_with('/') || member.ends_with('/') {
        return false;
    }
    if member.contains("//") {
        return false;
    }
    true
}

/// routing 值校验：枚举 + `<plugin>/<name>`。
pub(crate) fn valid_routing(routing: &str) -> bool {
    if ROUTING_KINDS.contains(&routing) {
        return true;
    }
    // <plugin>/<name>：两段非空，段内只许字母数字/短横线/点/下划线
    let mut parts = routing.split('/');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !a.is_empty()
        && !b.is_empty()
        && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && b.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// stays 值校验：枚举。
pub(crate) fn valid_stays(stays: &str) -> bool {
    STAYS_KINDS.contains(&stays)
}

/// auto 目标模型名校验：只允许字母数字与 `.` `_` `-` `:`。
///
/// 允许字面 `:`（实测：`auto/nemotron-3.5-lightning:free` 是真实存在的
/// public auto id，`:free` 是模型 id 的一部分，不是不合法的 effort）。
pub(crate) fn valid_auto_model_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 120 {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
}

/// 组显示名校验：非空、长度上限、不含控制字符。
pub(crate) fn valid_group_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 120 {
        return false;
    }
    !trimmed.chars().any(|c| c.is_control())
}

// ───────────────────────── id 推导 ─────────────────────────

/// 把一段文本折叠成单短横线连接的 slug。
///
/// `keep_dot=true` 时保留点号（CLI 用名字派生内部 id 的规则：
/// `Auto.GLM 5.3` → `auto.glm-5.3`）；`keep_dot=false` 时点号也变短横线
/// （public `auto/<model>` 的规范内部 id 规则：`auto/minimax-m3.1-flash` →
/// `auto-minimax-m3-1-flash`）。
fn slugify(s: &str, keep_dot: bool) -> String {
    let mut out = String::new();
    for ch in s.trim().chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '-' || (keep_dot && c == '.') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    // 折叠连续短横线
    let mut folded = String::with_capacity(out.len());
    let mut prev_dash = false;
    for c in out.chars() {
        if c == '-' {
            if !prev_dash {
                folded.push(c);
            }
            prev_dash = true;
        } else {
            folded.push(c);
            prev_dash = false;
        }
    }
    folded.trim_matches('-').to_string()
}

/// 名字 → CLI 派生的内部 id（实测：小写、空格/下划线/斜杠/非 ASCII → 短横线，
/// 连续短横线折叠，首尾短横线去掉，点号保留）。
pub(crate) fn name_to_id(name: &str) -> String {
    slugify(name, true)
}

/// 裸模型名 → public `auto/<model>` 的规范内部 id 后缀（点号也变短横线）。
pub(crate) fn auto_slug(model: &str) -> String {
    slugify(model, false)
}

/// public id → 内部 id。
///
/// - `auto/<model>` → `auto-<dash-slug(model)>`（`auto/glm-5.3` → `auto-glm-5-3`）
/// - `group/<id>` → `<id>`
/// - 裸 id → 原样（当作内部 id）
pub(crate) fn internal_id_of(public_id: &str) -> String {
    let s = public_id.trim();
    match s.split_once('/') {
        Some(("auto", rest)) => format!("auto-{}", auto_slug(rest)),
        Some(("group", rest)) => rest.to_string(),
        _ => s.to_string(),
    }
}

/// 内部 id → CLI 可用的精确引用 `group/<内部id>`。
///
/// 不用裸 id：`group rm <裸id>` 可能命中内置 found 组并把它藏掉；`group/<id>`
/// 才是「按内部 id 精确指向」的写法（实测 `group show group/auto-glm-5-3` 与
/// `group show auto/glm-5.3` 指向同一个内置组）。
pub(crate) fn cli_ref(internal_id: &str) -> String {
    format!("group/{}", internal_id)
}

/// 名字归一（比较用）：去首尾空白 + 折叠内部空白 + 小写。
pub(crate) fn norm_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// 成员 id 的「基础部分」：最后一段里第一个冒号之前的部分。
///
/// `kilo/nvidia/nemotron-3.5-super-vl:free` → `kilo/nvidia/nemotron-3.5-super-vl`
/// （`:free` 是模型 id 的一部分）；`codex/gpt-6.1-sol:high` → `codex/gpt-6.1-sol`。
pub(crate) fn member_base(member: &str) -> String {
    let tail_start = member.rfind('/').map(|i| i + 1).unwrap_or(0);
    match member[tail_start..].find(':') {
        Some(rel) => member[..tail_start + rel].to_string(),
        None => member.to_string(),
    }
}

/// 成员 id 的后缀段（`:effort` / `:fast` / `:low+high`）。
pub(crate) fn member_suffixes(member: &str) -> Vec<String> {
    let base = member_base(member);
    if base.len() == member.len() {
        return Vec::new();
    }
    member[base.len() + 1..].split(':').map(|s| s.to_string()).collect()
}

/// 后缀段是否是合法的 reasoning effort（`none`/`low`/…/`max`，可用 `+` 组合）。
///
/// **不含** `fast`：`fast` 是「厂商的快速模式」修饰符，不是 effort，
/// 单独判定（见 `is_fast_suffix`）。
pub(crate) fn valid_effort_suffix(suffix: &str) -> bool {
    if suffix.is_empty() {
        return false;
    }
    suffix.split('+').all(|p| EFFORT_KINDS.contains(&p))
}

/// 后缀段是否是 `fast`（厂商快速模式修饰符）。
///
/// 实测（.scratch/probe_p14.txt、probe_p15.txt）：
/// 成员 `trae-cn/glm-5.3-flash-max:fast` 被 CLI 拒绝，报
/// `trae-cn/glm-5.3-flash-max has no fast mode ... can ask for`
/// ——CLI 把它当成「给这个模型开快速模式」的修饰符并去找 fast 能力，
/// 而不是当成一个叫 `glm-5.3-flash-max:fast` 的模型。
/// 顺带确认：**当前网关目录里没有任何模型支持 fast 模式**
/// （p17 穷举了网关目录的全部 83 个真实模型——90 个里另外 7 个是 `auto/*` 组，
/// 不是模型——CLI 统一报 `has no fast mode`），
/// 所以这条路径在本机暂时走不通。
pub(crate) fn is_fast_suffix(suffix: &str) -> bool {
    suffix == "fast"
}

/// 成员 id 的「模型 id 尾段」：模型 id 的**最后一段**。
///
/// 这是 auto 规则的核心（实测 p8–p13）：
/// public `auto/<model>` 的 `<model>` 就是成员模型 id 的最后一段，
/// **不是** provider 之后的完整剩余路径。
///
/// `workbuddy/minimax-m3.1-flash` → `minimax-m3.1-flash`
/// `kilo/reflection/beam-free` → `beam-free`（不是 `reflection/beam-free`）
/// `kilo/kilo-auto/free` → `free`
/// `kilo/nvidia/nemotron-3.5-super-vl:free` → `nemotron-3.5-super-vl:free`
///   （字面 `:free` 是模型 id 的一部分，不能在这里被剥掉）
pub(crate) fn model_tail(member: &str) -> String {
    match member.rsplit_once('/') {
        Some((_head, tail)) => tail.to_string(),
        None => member.to_string(),
    }
}

/// auto 规范内部 id：成员 → `auto-<slug(模型尾段)>`。
///
/// 这是**唯一**能拿到 public `auto/<model>` 的内部 id 形态。实测：
/// - `kilo/kilo-auto/free` → `auto-free` ✅（`auto-reflection-beam-free` ❌）
/// - `kilo/recursive/flint-v1.0:free` → `auto-flint-v1-0-free` ✅
/// - `trae-cn/glm-5.3-flash-max:high` → `auto-glm-5-3-flash-max` ✅
///   （effort 后缀不计入目标名）
pub(crate) fn canonical_auto_id(member: &str, models: &HashSet<String>) -> Option<String> {
    auto_target(member, models).map(|t| format!("auto-{}", auto_slug(&t)))
}

/// 成员的 auto 目标模型名（public `auto/<model>` 里的 `<model>`）。
///
/// 判定顺序（实测 p12/p13）：
/// 1. 完整 id 在网关目录里 → 目标 = 完整 id 的最后一段
///    （`kilo/nvidia/nemotron-3.5-super-vl:free` → `nemotron-3.5-super-vl:free`）
/// 2. 去掉 `:后缀` 后在目录里、且后缀是已知 effort → 目标 = 基础部分的最后一段
///    （`trae-cn/glm-5.3-flash-max:high` → `glm-5.3-flash-max`）
/// 3. 都不中 → `None`（该成员不参与 auto 判定）
pub(crate) fn auto_target(member: &str, models: &HashSet<String>) -> Option<String> {
    if models.contains(member) {
        let t = model_tail(member);
        if !t.is_empty() {
            return Some(t);
        }
    }
    let base = member_base(member);
    if base != member {
        let suffixes = member_suffixes(member);
        // 全部后缀都是已知 effort 时，effort 不计入目标名（实测 p13）。
        // `fast` 不在这里：它是厂商快速模式修饰符，且本机没有任何模型支持
        // （实测 p14/p15），这种成员根本进不了组，不会走到 auto 判定。
        let all_effort = !suffixes.is_empty() && suffixes.iter().all(|s| valid_effort_suffix(s));
        if all_effort && models.contains(&base) {
            let t = model_tail(&base);
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// 组里所有成员算出的 auto 目标名集合（去重）。
///
/// public `auto/<model>` 的 `<model>` 来自**某一个**成员（实测：混入别的模型
/// 也能成 auto，只要有一个成员匹配）。
pub(crate) fn auto_targets(members: &[String], models: &HashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in members {
        if let Some(t) = auto_target(m, models) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// CLI 回读的 public id 是否就是我们要的 `auto/<model>`。
///
/// **不能**由 internal id 反推：实测（.scratch/p2e_rule2_out.txt）CLI 保留成员尾段的
/// 下划线/点号/冒号，且对部分模型选择性小写——`trae-cn/Doubao_1_6` 回读
/// `auto/Doubao_1_6`（原样保留），而 `trae-cn/Doubao-Seed-2.1-Turbo` 回读
/// `auto/doubao-seed-2.1-turbo`（被小写）。public 尾段与 internal slug 不是同一
/// 信息空间，只能拿 CLI 的**实际回读结果**来判。
///
/// 判定两步，缺一不可：
/// 1. 必须以 `auto/` 开头——成员失配时 CLI 会把组降级成 `group/auto-<slug>`，
///    这一步直接判掉（实测 `group set id=<另一模型 slug>` 退出码仍是 0）；
/// 2. 尾段的 `auto_slug` 规范化后必须等于目标模型的规范化（吸收大小写/点号/
///    下划线差异，但不能替代第 1 步的前缀判断）。
pub(crate) fn public_auto_id_matches(public_id: &str, auto_model: &str) -> bool {
    let Some(tail) = public_id.trim().strip_prefix("auto/") else {
        return false;
    };
    let want = auto_slug(auto_model);
    !want.is_empty() && auto_slug(tail) == want
}

// ───────────────────────── CLI 执行 ─────────────────────────

/// CLI 运行结果：stdout / stderr 分开存，退出码单独存。
pub(crate) struct CliRun {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 定位 Magpie CLI：MAGPIE_EXE env → %USERPROFILE%\.local\bin\magpie.exe → PATH（where/which）。
pub(crate) fn find_magpie_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MAGPIE_EXE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(&profile).join(".local").join("bin").join("magpie.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    let (prog, arg) = if cfg!(target_os = "windows") { ("where.exe", "magpie") } else { ("which", "magpie") };
    std::process::Command::new(prog)
        .arg(arg)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| PathBuf::from(l.trim())))
        .filter(|p| p.is_file())
}

/// 跑一条 Magpie CLI 命令（同步；调用方需放进 spawn_blocking）。
///
/// stdin=null（无 method 的交互菜单会 EOF）；stdout/stderr 分离捕获。
///
/// 参数用 `Vec<String>` 收（owned）：调用方要把它 move 进 `spawn_blocking`，
/// `&[&str]` 会带出 `'static` 借用约束。
pub(crate) fn run_magpie(exe: &Path, args: &[String]) -> anyhow::Result<CliRun> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).stdin(std::process::Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output()?;
    Ok(CliRun {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// stderr 里与「Trae 登录过期」一类的插件日志噪声——只做诊断，不参与成败判定。
fn is_noise(stderr: &str) -> bool {
    stderr.contains("sign-in has expired") || stderr.contains("plugin [error]")
}

/// 取 stderr 的第一行「真错误」（过滤插件噪声），供错误文案用。
pub(crate) fn first_real_error(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !is_noise(l))
        .next()
        .map(str::to_string)
}

/// CLI 失败时的可读原因：优先 stderr 的真错误，其次 stdout 首行。
pub(crate) fn cli_failure_message(run: &CliRun) -> String {
    first_real_error(&run.stderr)
        .or_else(|| {
            run.stdout
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("CLI 退出码 {}", run.code))
}

// ───────────────────────── 输出解析 ─────────────────────────

/// `magpie groups` 的一行解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupRow {
    pub name: String,
    pub public_id: String,
    pub routing: String,
    pub members: Vec<String>,
    /// manual 路由下当前选中的成员（列表里 `●` 打在成员前面）。
    pub selected: Option<String>,
    /// 名称列尾部**看起来**像 `found` 标记（弱提示，**不可作为准**）。
    ///
    /// 真正的 found 状态以 `group show` 首行的 `found by ...` 为准。
    /// 列表里的这个标记只用于「先决定要不要去 `group show` 拿详情」。
    pub found_marker: bool,
}

/// `magpie group show` 的成员条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberInfo {
    pub id: String,
    pub provider: String,
    pub description: String,
    pub selected: bool,
}

/// `magpie group show` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupDetail {
    pub name: String,
    pub public_id: String,
    pub suffix: String,
    pub routing: String,
    pub routing_note: String,
    pub stays: String,
    pub members: Vec<MemberInfo>,
    /// 内置 found 组（`found by Magpie — changing it makes it yours`）。
    pub found: bool,
    /// 已被移除（`removed · Magpie group restore <id> brings it back`）。
    pub removed: bool,
}

/// token 是不是 public id 形态（`auto/...` 或 `group/...`）。
///
/// 注意：public id 的尾段允许字面 `:`。实测（p13 Q3c）真实存在的
/// public id 是 `auto/nemotron-3.5-lightning:free`——`:free` 是模型 id 的一部分，
/// 不是 effort 后缀。这里若不放行 `:`，就会把真实 auto 组误判成普通模型名。
fn is_public_id_token(tok: &str) -> bool {
    let Some((prefix, rest)) = tok.split_once('/') else {
        return false;
    };
    (prefix == "auto" || prefix == "group")
        && !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

/// token 是不是 routing 值形态（枚举或 `<plugin>/<name>`）。
fn is_routing_token(tok: &str) -> bool {
    valid_routing(tok)
}

/// 按「2 个及以上连续空格」切列（CLI 表格的列间距至少 2 空格；成员之间是单空格）。
fn split_columns(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            let mut j = i;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            if j - i >= 2 {
                out.push(&line[start..i]);
                start = j;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out.push(&line[start..]);
    out.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// 解析 `magpie groups` 的一行 → 组摘要。
///
/// 行形态（实测，列宽自适应，**不按固定列偏移切**，按列间距切）：
/// ```text
///   Beam Free found            auto/beam-free            smart  opencode-zen/beam-free · kilo/reflection/beam-free
///   rsi-probe                 group/rsi-probe            manual ● workbuddy/minimax-m3.1-flash · opencode-zen/beam-free
/// ```
/// - 第 1 列是展示名（Magpie 发现的组带尾部 ` found` 标记）。
/// - 第 2 列是 public id：`auto/...` 或 `group/...`。
/// - 第 3 列是 routing。
/// - 第 4 列是成员，用 ` · ` 分隔；manual 的当前成员前面带 `●`。
pub(crate) fn parse_groups_line(line: &str) -> Option<GroupRow> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("removed:") || trimmed.starts_with('(') {
        return None;
    }
    let cols = split_columns(trimmed);
    let (name, public_id, routing, member_col): (String, String, String, String) =
        if cols.len() >= 4 && is_public_id_token(cols[1]) && is_routing_token(cols[2]) {
            (
                cols[0].to_string(),
                cols[1].to_string(),
                cols[2].to_string(),
                cols[3..].join("  "),
            )
        } else {
            // 回退：按 token 扫描（列间距异常时用）
            let tokens: Vec<&str> = trimmed.split_whitespace().collect();
            let mut idx = None;
            for (i, t) in tokens.iter().enumerate() {
                if is_public_id_token(t)
                    && tokens.get(i + 1).map(|n| is_routing_token(n)).unwrap_or(false)
                {
                    idx = Some(i);
                    break;
                }
            }
            let i = idx?;
            // 名字取「public id 之前的原文」：token 拼接会把名字里的连续空格
            // 折叠掉（实测 CLI 允许 `Spaced  Out` 这种名字）。
            let name_raw = tokens[..i].join(" ");
            let _ = &name_raw;
            let head = trimmed
                .find(tokens[i])
                .map(|p| trimmed[..p].trim_end().to_string())
                .unwrap_or_else(|| name_raw);
            (
                head,
                tokens[i].to_string(),
                tokens[i + 1].to_string(),
                tokens[i + 2..].join(" "),
            )
        };
    // 显示名**原样保留**（含内部连续空格）。
    //
    // 这里**不**从名字尾部剥 `found`：`found` 是 Magpie 在 `groups` 列表里
    // 给「它自己发现的组」加的列内标记，但它和显示名之间没有可靠分隔——
    // 实测用户组也可以叫 `... found`，而普通用户组的 `group show` 首行
    // **根本没有** `found by ...`。所以「是不是 found 组」必须以
    // `group show` 首行的 `found by ...` 为准（见 `parse_group_show`），
    // 列表里的 `found_marker` 只是一个**弱提示**，前端不能拿它当准。
    //
    // 注意：**显示名里的连续空格必须原样保留**（实测 CLI 允许 `Spaced  Out`
    // 这种名字，并且会把它编号成 `-2`）。折叠空白会改掉用户看到的名字。
    let found_marker = {
        let t = name.trim_end();
        t.ends_with(" found") && t.len() > " found".len()
    };
    // 成员 = routing 之后的列（`·` 是分隔符，`●` 标记 manual 当前成员）
    let mut members: Vec<String> = Vec::new();
    let mut selected: Option<String> = None;
    let mut bullet = false;
    for tok in member_col.split_whitespace() {
        if tok == "·" {
            continue;
        }
        if tok == "●" {
            bullet = true;
            continue;
        }
        if bullet {
            // `●` 打在成员**前面**：这个 token 就是当前选中的成员，
            // 它同时也是成员列表里的一项，不能被吃掉。
            selected = Some(tok.to_string());
            bullet = false;
        }
        members.push(tok.to_string());
    }
    Some(GroupRow {
        name,
        public_id,
        routing,
        members,
        selected,
        found_marker,
    })
}

/// 解析 `magpie groups` 的 footer 里的 hidden 组 id 列表。
///
/// 形态（实测）：
/// ```text
///   removed: auto-glm-5-3, auto-glm-5-3-2 · Magpie group restore <id> brings one back
/// ```
pub(crate) fn parse_removed_ids(rest: &str) -> Vec<String> {
    let left = rest.split('·').next().unwrap_or("");
    left.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| valid_internal_id(s))
        .map(str::to_string)
        .collect()
}

/// 解析 `magpie groups` 的完整 stdout → (可见组行, hidden 组内部 id)。
pub(crate) fn parse_groups_output(stdout: &str) -> (Vec<GroupRow>, Vec<String>) {
    let mut rows = Vec::new();
    let mut hidden = Vec::new();
    for line in stdout.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("removed:") {
            hidden.extend(parse_removed_ids(rest));
            continue;
        }
        if let Some(row) = parse_groups_line(line) {
            rows.push(row);
        }
    }
    (rows, hidden)
}

/// 解析 `magpie group show <id>` 的 stdout。
///
/// 形态（实测）：
/// ```text
///   DeepSeek V4.1 Flash  auto/deepseek-v4.1-flash  found by Magpie — changing it makes it yours
///   routing   smart
///   stays     auto
///   models    1 opencode-zen/deepseek-v4.1-flash  OpenCode Zen · DeepSeek V4.1 Flash
///             2 workbuddy/deepseek-v4.1-flash  WorkBuddy · DeepSeek-V4.1-Flash
/// ```
/// - 首行 = 展示名 + public id（可能带 `found by ...` / `removed · ...` 后缀）。
/// - `routing` 行可能是 `manual  every request to ...`（长说明）。
/// - `models` 首行带序号，后续成员是缩进续行。
pub(crate) fn parse_group_show(stdout: &str) -> Option<GroupDetail> {
    let mut detail = GroupDetail {
        name: String::new(),
        public_id: String::new(),
        suffix: String::new(),
        routing: String::new(),
        routing_note: String::new(),
        stays: String::new(),
        members: Vec::new(),
        found: false,
        removed: false,
    };
    let mut seen_header = false;
    let mut in_models = false;
    for raw in stdout.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        // `group set` 的额外说明行（改名后 id 不变）
        if trimmed.starts_with("its id stays") {
            continue;
        }
        // 成员续行：`            2 workbuddy/xxx  WorkBuddy · ...`
        if in_models {
            if let Some(m) = parse_member_entry(trimmed) {
                detail.members.push(m);
                continue;
            }
        }
        if trimmed.starts_with("routing") {
            let v = trimmed
                .strip_prefix("routing")
                .unwrap_or("")
                .trim()
                .to_string();
            let mut it = v.splitn(2, char::is_whitespace);
            detail.routing = it.next().unwrap_or("").trim().to_string();
            detail.routing_note = it.next().unwrap_or("").trim().to_string();
            continue;
        }
        if trimmed.starts_with("stays") {
            let v = trimmed.strip_prefix("stays").unwrap_or("").trim();
            detail.stays = v.split_whitespace().next().unwrap_or("").to_string();
            continue;
        }
        if trimmed.starts_with("models") {
            in_models = true;
            let v = trimmed.strip_prefix("models").unwrap_or("").trim();
            if let Some(m) = parse_member_entry(v) {
                detail.members.push(m);
            }
            continue;
        }
        if !seen_header {
            let Some((name, public_id, suffix)) = parse_show_header(trimmed) else {
                continue;
            };
            detail.name = name;
            detail.public_id = public_id;
            detail.suffix = suffix;
            detail.found = detail.suffix.contains("found by");
            detail.removed = detail.suffix.contains("removed");
            seen_header = true;
        }
    }
    if !seen_header {
        return None;
    }
    Some(detail)
}

/// 解析 `group show` 的首行 → (展示名, public id, 后缀说明)。
///
/// 首行形如 `  GLM-5.3  auto/glm-5.3  found by Magpie — changing it makes it yours`。
/// public id 取第 2 列（按列间距切）；名称里出现的 `auto/xxx` 不会抢走位置。
fn parse_show_header(trimmed: &str) -> Option<(String, String, String)> {
    let cols = split_columns(trimmed);
    if cols.len() >= 2 && is_public_id_token(cols[1]) {
        let name = cols[0].to_string();
        let public_id = cols[1].to_string();
        // 后缀 = 首行里 public id 之后的原始文本
        let name_end = trimmed.find(&name).map(|i| i + name.len()).unwrap_or(0);
        let suffix = trimmed[name_end..]
            .trim_start()
            .strip_prefix(&public_id)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        return Some((name, public_id, suffix));
    }
    // 回退：token 扫描
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    for (i, tok) in tokens.iter().enumerate() {
        if !is_public_id_token(tok) {
            continue;
        }
        let next = tokens.get(i + 1).copied();
        let ok = match next {
            None => true,
            Some(n) => n.starts_with("found") || n.starts_with("removed"),
        };
        if !ok {
            continue;
        }
        // 名字取「public id 之前的原文」：保留名字内部的连续空格。
        let name = trimmed
            .find(tok)
            .map(|p| trimmed[..p].trim_end().to_string())
            .unwrap_or_else(|| tokens[..i].join(" "));
        let public_id = (*tok).to_string();
        let suffix = match next {
            Some(_) => tokens[i + 1..].join(" "),
            None => String::new(),
        };
        return Some((name, public_id, suffix));
    }
    None
}

/// 解析 `models` 行右侧的 `1 provider/model  描述`（含 manual 的 `●` 标记）。
fn parse_member_entry(v: &str) -> Option<MemberInfo> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    // 序号前缀（`1 ` / `2 `）
    let rest = match v.split_once(char::is_whitespace) {
        Some((first, rest)) if !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) => {
            rest.trim()
        }
        _ => v,
    };
    if rest.is_empty() {
        return None;
    }
    let (id, tail) = match rest.split_once(char::is_whitespace) {
        Some((id, tail)) => (id, tail.trim()),
        None => (rest, ""),
    };
    if id.is_empty() || !id.contains('/') || id == "·" {
        return None;
    }
    // manual 的当前成员：id 后面紧跟 `●`
    let (selected, tail) = match tail.strip_prefix('●') {
        Some(t) => (true, t.trim()),
        None => (false, tail),
    };
    // 描述：`Provider · Description`（按第一个 ` · ` 切）
    let (provider, description) = match tail.split_once(" · ") {
        Some((p, d)) => (p.trim().to_string(), d.trim().to_string()),
        None => (tail.trim().to_string(), String::new()),
    };
    Some(MemberInfo {
        id: id.to_string(),
        provider,
        description,
        selected,
    })
}

/// 从 `group add` 的 stdout 里取新建组的内部 id。
///
/// 形态：`✓ added <name> · agents pick it as group/<id>`。
pub(crate) fn parse_added_group_id(stdout: &str) -> Option<String> {
    const MARK: &str = "agents pick it as group/";
    for line in stdout.lines() {
        if let Some(pos) = line.find(MARK) {
            let rest = &line[pos + MARK.len()..];
            let id = rest.split_whitespace().next().unwrap_or("").trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// `group add` 是否发生了「替换既有组」（同名会被 CLI 静默替换，退出码仍是 0）。
pub(crate) fn parse_add_replaced(stdout: &str) -> bool {
    stdout.lines().any(|l| l.contains("replaced"))
}

/// `group rm` 的结果：是否只是「隐藏」（内置 found 组被移除时是隐藏，不是真删），
/// 以及可 restore 的精确内部 id。
pub(crate) fn parse_removed_outcome(stdout: &str) -> (bool, Option<String>) {
    const MARK: &str = "group restore ";
    for line in stdout.lines() {
        if let Some(pos) = line.find(MARK) {
            let rest = &line[pos + MARK.len()..];
            let id = rest.split_whitespace().next().unwrap_or("").trim();
            let hidden = line.contains("hidden");
            return (hidden, Some(id.to_string()));
        }
    }
    (false, None)
}

/// 后缀里带的 restore id（`removed · Magpie group restore auto-glm-5-3`）。
pub(crate) fn parse_suffix_restore_id(suffix: &str) -> Option<String> {
    const MARK: &str = "group restore ";
    let pos = suffix.find(MARK)?;
    let rest = &suffix[pos + MARK.len()..];
    let id = rest.split_whitespace().next().unwrap_or("").trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

// ───────────────────────── 状态装载 ─────────────────────────

/// 一个组在 Host 侧的完整视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupView {
    pub detail: GroupDetail,
    /// 内部 id（CLI 的 `group/<内部id>` 里的那一段）。
    ///
    /// **权威来源是查询时用的 ref**，不是从 public id 反推：public `auto/<model>`
    /// 与 internal `auto-<slug>` 不是同一信息空间（CLI 保留成员尾段的下划线/点号/
    /// 冒号，且选择性小写），反推会丢信息。
    pub internal_id: String,
    pub visible: bool,
    /// 已移除组的**权威** restore id（来自 `group show` 后缀
    /// `removed · Magpie group restore <id>`）。
    ///
    /// 只有 `group show` 后缀里的这个值是权威的：`groups` 的 removed footer 只能
    /// 用来发现 hidden 组，它给出的 id 不保证能被 `group restore` 接受。
    pub restore_id: Option<String>,
}

/// `magpie groups` + 逐组 `magpie group show` 的结果。
pub(crate) fn load_state(exe: &Path) -> anyhow::Result<Vec<GroupView>> {
    let list = run_magpie(exe, &["groups".to_string()])?;
    if list.code != 0 {
        anyhow::bail!("{}", cli_failure_message(&list));
    }
    let (rows, hidden_ids) = parse_groups_output(&list.stdout);
    let mut refs: Vec<String> = Vec::new();
    for row in &rows {
        refs.push(cli_ref(&internal_id_of(&row.public_id)));
    }
    for hid in &hidden_ids {
        refs.push(cli_ref(hid));
    }
    let shows = show_groups(exe, &refs);
    let mut out = Vec::new();
    // 内部 id 一律取**查询用的 ref**（`group/<id>` → `<id>`），不从回读的 public id
    // 反推；restore id 取 `group show` 后缀里的权威值。
    for (reference, show) in shows {
        let Ok(run) = show else { continue };
        if run.code != 0 {
            continue;
        }
        if let Some(view) = view_from_show(&reference, &run.stdout) {
            out.push(view);
        }
    }
    Ok(out)
}

/// 把一次 `group show` 的 stdout 折成 `GroupView`。
///
/// 纯逻辑（不碰 CLI），便于单测两条权威规则：
/// 1. 内部 id 取**查询用的 ref**（`group/<id>` → `<id>`），不从回读的 public id 反推；
/// 2. restore id 只对 removed 组取 `group show` 后缀里的权威值。
fn view_from_show(reference: &str, stdout: &str) -> Option<GroupView> {
    let detail = parse_group_show(stdout)?;
    let internal_id = reference
        .strip_prefix("group/")
        .unwrap_or(reference)
        .to_string();
    let restore_id = if detail.removed {
        parse_suffix_restore_id(&detail.suffix)
    } else {
        None
    };
    let visible = !detail.removed;
    Some(GroupView {
        detail,
        internal_id,
        visible,
        restore_id,
    })
}

/// 逐组 `group show`（限量并发），返回 `(查询用的 ref, 结果)`。
///
/// 必须把 ref 一起带回来：内部 id 的权威来源就是它（见 `GroupView::internal_id`）。
/// 失败/解析不出来的组由调用方跳过。
fn show_groups(exe: &Path, refs: &[String]) -> Vec<(String, anyhow::Result<CliRun>)> {
    let mut out = Vec::with_capacity(refs.len());
    for chunk in refs.chunks(SHOW_CONCURRENCY) {
        let results = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|r| {
                    let exe = exe.to_path_buf();
                    let r = r.clone();
                    s.spawn(move || run_magpie(&exe, &["group".to_string(), "show".to_string(), r]))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("子线程 panic"))))
                .collect::<Vec<_>>()
        });
        for (reference, result) in chunk.iter().zip(results) {
            out.push((reference.clone(), result));
        }
    }
    out
}

/// 读单个组（`group show group/<内部id>`）。
pub(crate) fn show_one(exe: &Path, internal_id: &str) -> anyhow::Result<Option<GroupDetail>> {
    let r = run_magpie(
        exe,
        &[
            "group".to_string(),
            "show".to_string(),
            cli_ref(internal_id),
        ],
    )?;
    if r.code != 0 {
        return Ok(None);
    }
    Ok(parse_group_show(&r.stdout))
}

// ───────────────────────── JSON 形状 ─────────────────────────

/// 组对象的对外 JSON。
pub(crate) fn group_json(view: &GroupView) -> Value {
    let d = &view.detail;
    json!({
        "name": d.name,
        "public_id": d.public_id,
        "internal_id": view.internal_id,
        "kind": if d.public_id.starts_with("auto/") { "auto" } else { "group" },
        "routing": d.routing,
        "routing_note": d.routing_note,
        "stays": d.stays,
        "members": d.members.iter().map(|m| json!({
            "id": m.id,
            "provider": m.provider,
            "description": m.description,
            "selected": m.selected,
        })).collect::<Vec<_>>(),
        "member_ids": d.members.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        "found": d.found,
        "removed": d.removed,
        // owned = 用户自己的组（既不是 Magpie 发现的，也没被移除）
        "owned": !d.found && !d.removed,
        "visible": view.visible,
        "restorable": d.removed,
        // 权威 restore id（来自 `group show` 后缀）；非 removed 组为 null。
        "restore_id": view.restore_id,
    })
}

fn kerr(code: &str, msg: impl Into<String>) -> Value {
    json!({"ok": false, "error": {"code": code, "message": msg.into()}})
}

// ───────────────────────── 成员白名单 ─────────────────────────

/// 取网关当前的模型清单（成员白名单的来源）。
///
/// 走 llm-adapter `models.list`（不加 `provider` 覆盖——那会拿到尚未保存的 UI 配置
/// 或本机 Ollama 目录，而不是服务端已保存的网关目录）。
async fn gateway_models(kernel: &Kernel) -> Result<HashSet<String>, Value> {
    let v = dispatch_or_err(kernel, "llm-adapter", json!({"op": "models.list"})).await?;
    let models: HashSet<String> = v
        .get("models")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.starts_with("auto/") && !s.starts_with("group/"))
                .collect()
        })
        .unwrap_or_default();
    if models.is_empty() {
        return Err(kerr("K502", "网关模型清单为空，无法校验组成员（请先确认上游网关可用）"));
    }
    Ok(models)
}

/// 取网关模型 → display_name 映射（别名归并用）。Magpie 网关在 /v1/models 每项带
/// `display_name`：同一真实模型在不同平台常是不同 id（glm-5.3-flash / gfmodel /
/// gmodel…），但 display_name 相同。前端一键建组据此把别名一起纳为成员。
/// 软降级：adapter 未回传 meta 时返回空数组，前端回退到尾段匹配。
async fn gateway_models_meta(kernel: &Kernel) -> Vec<Value> {
    let v = match dispatch_or_err(kernel, "llm-adapter", json!({"op": "models.list"})).await {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    v.get("models_meta")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.get("name").and_then(Value::as_str).is_some_and(|n| !n.starts_with("auto/") && !n.starts_with("group/"))
                && m.get("display_name").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
        })
        .collect()
}

/// 校验一个成员是否可用。
///
/// 规则（来自 CLI help 与实测）：
/// - `group/<id>`：嵌套组，必须精确存在；
/// - `provider/model`：必须在网关当前模型清单里；
/// - `provider/model:<effort>`：基础部分在清单里，后缀是已知 effort；
/// - 模型 id 本身可以带字面 `:free` / `:7b` / `:0` 后缀——先按**完整 id 精确匹配**，
///   匹配不上才把后缀当 effort 解释。
pub(crate) fn check_member(
    member: &str,
    models: &HashSet<String>,
    group_ids: &HashSet<String>,
) -> Result<(), String> {
    if !valid_member_shape(member) {
        return Err(format!("成员格式非法：{member}"));
    }
    if let Some(rest) = member.strip_prefix("group/") {
        if group_ids.contains(rest) {
            return Ok(());
        }
        return Err(format!("嵌套组不存在：group/{rest}"));
    }
    if models.contains(member) {
        return Ok(());
    }
    let base = member_base(member);
    let suffixes = member_suffixes(member);
    if suffixes.is_empty() {
        return Err(format!("Magpie 不认识该模型：{member}"));
    }
    if !models.contains(&base) {
        return Err(format!("Magpie 不认识该模型：{member}"));
    }
    for s in &suffixes {
        if is_fast_suffix(s) {
            // fast 是**厂商快速模式修饰符**，不是模型 id 的一部分
            // （CLI help: `provider/model[:effort]:fast`）。
            //
            // 实测（p17，穷举网关目录全部 83 个真实模型）：**没有任何一个**
            // 支持 fast，CLI 统一报 `has no fast mode`。所以 Host 在成员校验阶段
            // 就拦下来，并明确告诉用户是「模型不支持 fast 模式」，
            // 而不是含糊的「模型不认识」。
            return Err(format!(
                "该模型不支持 fast 模式（fast 是厂商快速模式，当前网关目录里没有支持 fast 的模型）：{member}"
            ));
        }
        if !valid_effort_suffix(s) {
            return Err(format!("成员后缀非法（{s}）：{member}"));
        }
    }
    Ok(())
}

/// 组内嵌套引用是否会成环（CLI 自己也会拦，这里提前给中文原因）。
///
/// 实测区分两件事（见 .scratch/probe_p5.txt、.scratch/probe_p6.txt）：
/// - **成环**（自引用或间接环）→ CLI 报 `can't be in itself`；
/// - **超过 8 层** → CLI 报 `has groups in it 8 deep; a group in a group goes at most 8 deep`。
///
/// 所以这里只判环，深度超限交给 CLI（它的报错更准）。
pub(crate) fn nested_cycle(
    target_internal: &str,
    new_members: &[String],
    edges: &HashMap<String, Vec<String>>,
) -> bool {
    fn reaches(
        start: &str,
        target: &str,
        edges: &HashMap<String, Vec<String>>,
        seen: &mut HashSet<String>,
    ) -> bool {
        if !seen.insert(start.to_string()) {
            // 已经走过的节点：这条路径不成环（真正的环会在上面某一步被 target 命中）
            return false;
        }
        let Some(children) = edges.get(start) else {
            return false;
        };
        for c in children {
            if c == target || reaches(c, target, edges, seen) {
                return true;
            }
        }
        false
    }
    for m in new_members {
        let Some(gid) = m.strip_prefix("group/") else {
            continue;
        };
        // 直接自引用
        if gid == target_internal {
            return true;
        }
        // 间接环：gid 的后代里出现 target
        let mut seen = HashSet::new();
        if reaches(gid, target_internal, edges, &mut seen) {
            return true;
        }
    }
    false
}

/// 嵌套深度（**不含**最外层那个组自己）。
///
/// 实测（.scratch/probe_p16.txt，链式建组逐层试）：
/// - `chain-1` 含普通模型；`chain-N` 含 `group/chain-(N-1)`。
/// - `chain-1` … `chain-9` 都能建（`chain-9` 的下游有 8 个组）；
/// - `chain-10` 被拒：`rsi-p16-chain-9 has groups in it 8 deep;
///   a group in a group goes at most 8 deep`。
/// - 直接挂 `group/chain-8` 的新组成功，挂 `group/chain-9` 失败。
/// - 混合成员（普通模型 + `group/chain-9`）同样失败——普通模型不占深度。
///
/// 结论：**最外层组往下最多再嵌 8 层组**。也就是
/// 「最外层组的组后代数」= 「成员组及其后代的最长链长度」≤ 8。
/// 目标组自己**不计入**（否则 chain-9 会被误判为 9 层而错杀）。
///
/// 注意这与**成环**是两回事：超过深度不是环，前端要给出不同文案。
pub(crate) fn nested_depth(
    new_members: &[String],
    edges: &HashMap<String, Vec<String>>,
) -> usize {
    /// 从 `start` 出发的最长下游链长度（**含 start 自身**，算 1）。
    fn longest(
        start: &str,
        edges: &HashMap<String, Vec<String>>,
        seen: &mut HashSet<String>,
    ) -> usize {
        if !seen.insert(start.to_string()) {
            // 已在当前路径上（成环）：这条分支到此为止。
            return 0;
        }
        let Some(children) = edges.get(start) else {
            return 1;
        };
        let mut best = 1usize;
        for c in children {
            // 每条分支各用一份 visited 集合：否则两条分支共享节点时会少算。
            let mut sub_seen = seen.clone();
            let sub = longest(c, edges, &mut sub_seen);
            if sub + 1 > best {
                best = sub + 1;
            }
        }
        best
    }
    let mut best = 0usize;
    for m in new_members {
        let Some(gid) = m.strip_prefix("group/") else {
            continue;
        };
        let mut seen = HashSet::new();
        let len = longest(gid, edges, &mut seen);
        if len > best {
            best = len;
        }
    }
    best
}

/// CLI 允许的最大嵌套深度（「a group in a group goes at most 8 deep」）。
pub(crate) const MAX_NEST_DEPTH: usize = 8;

/// 预检 canonical auto 内部 id 是否已被占用。
///
/// **必须**把 hidden / removed 组算进去。实测：hidden 的 found 组仍然占着它的
/// internal id，新建一个同名组抢不到它（CLI 会编号成 `-2`，结果还是普通
/// `group/*`，不是 public auto）。只看可见组的话，Host 会放行一个必然失败的
/// 创建请求——用户看到的现象是「建出来了，但不是我要的 auto」。
///
/// 返回 `Err(文案)` = 已被占用（文案区分「已移除」「内置 found」「普通占用」）；
/// `Ok(())` = 没被占用。
pub(crate) fn check_auto_id_conflict(
    groups: &[GroupView],
    target_internal: &str,
    auto_model: &str,
) -> Result<(), String> {
    let Some(hit) = groups.iter().find(|g| g.internal_id == target_internal) else {
        return Ok(());
    };
    let hint = if hit.detail.removed {
        "该 id 属于一个已移除（隐藏）的组：可先 `group restore` 恢复它再编辑，或换用别的模型名"
    } else if hit.detail.found {
        "该 id 属于 Magpie 内置发现的组：直接编辑该组即可把它变成你的组"
    } else {
        "该 id 已被现有组占用"
    };
    Err(format!(
        "目标 auto/{auto_model} 已存在（内部 id {target_internal}，public id {}）：{hint}",
        hit.detail.public_id
    ))
}

// ───────────────────────── HTTP handler ─────────────────────────

/// GET /api/routegroups：组列表（可见组 + 已移除的 hidden 组）。
pub(crate) async fn routegroups_list(stream: &mut TcpStream) -> anyhow::Result<()> {
    let state = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<GroupView>> {
        let Some(exe) = find_magpie_exe() else {
            anyhow::bail!("未找到 Magpie CLI（可设 MAGPIE_EXE env，或装到 %USERPROFILE%\\.local\\bin\\magpie.exe）");
        };
        load_state(&exe)
    })
    .await;
    let groups = match state {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return json_resp(stream, 502, kerr("K502", format!("读取 Magpie 组失败：{e}"))).await
        }
        Err(e) => {
            return json_resp(stream, 500, kerr("K500", format!("读取 Magpie 组失败：{e}"))).await
        }
    };
    let visible = groups.iter().filter(|g| g.visible).count();
    let removed = groups.len() - visible;
    let items: Vec<Value> = groups.iter().map(group_json).collect();
    json_resp(
        stream,
        200,
        json!({"ok": true, "groups": items, "counts": {"visible": visible, "removed": removed}}),
    )
    .await
}

/// GET /api/routegroups/options：成员白名单 + routing/stays 枚举。
pub(crate) async fn routegroups_options(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
    match gateway_models(kernel).await {
        Ok(models) => {
            let mut list: Vec<String> = models.into_iter().collect();
            list.sort();
            // models_meta=[{name,display_name}]：供前端按 display_name 把别名
            // （gfmodel/gmodel…）并入同一 auto 组。adapter 未回传时为空数组，前端回退尾段匹配。
            let meta = gateway_models_meta(kernel).await;
            json_resp(
                stream,
                200,
                json!({
                    "ok": true,
                    "models": list,
                    "models_meta": meta,
                    "routings": ROUTING_KINDS,
                    "stays": STAYS_KINDS,
                }),
            )
            .await
        }
        Err(e) => json_resp(stream, 502, e).await,
    }
}

/// GET /api/routegroups/<id>：单个组详情。
pub(crate) async fn routegroups_show(stream: &mut TcpStream, raw_id: &str) -> anyhow::Result<()> {
    let id = percent_decode(raw_id);
    let internal = internal_id_of(&id);
    if internal.is_empty() {
        return json_resp(stream, 400, bad_request("缺组 id", Some("id"))).await;
    }
    let shown = tokio::task::spawn_blocking({
        let internal = internal.clone();
        move || -> anyhow::Result<Option<GroupDetail>> {
            let Some(exe) = find_magpie_exe() else {
                anyhow::bail!("未找到 Magpie CLI");
            };
            show_one(&exe, &internal)
        }
    })
    .await;
    match shown {
        Ok(Ok(Some(detail))) => {
            // hidden / removed 的组：详情仍可读，但 `visible` 必须是 false
            // （不能一律 true，否则前端会把已移除的组当成可用组）。
            let removed = detail.removed;
            // restore id 取 `group show` 后缀里的权威值（只有 removed 组才有）。
            let restore_id = if removed {
                parse_suffix_restore_id(&detail.suffix)
            } else {
                None
            };
            let view = GroupView {
                internal_id: internal.clone(),
                detail,
                visible: !removed,
                restore_id,
            };
            json_resp(stream, 200, json!({"ok": true, "group": group_json(&view)})).await
        }
        Ok(Ok(None)) => {
            json_resp(stream, 404, kerr("K404", format!("组不存在：{internal}"))).await
        }
        Ok(Err(e)) => json_resp(stream, 502, kerr("K502", format!("读取组失败：{e}"))).await,
        Err(e) => json_resp(stream, 500, kerr("K500", format!("读取组失败：{e}"))).await,
    }
}

/// POST /api/routegroups：新建组（可选 `auto_model` → 建为 public `auto/<model>`）。
pub(crate) async fn routegroups_create(
    stream: &mut TcpStream,
    body: &[u8],
    kernel: &Kernel,
) -> anyhow::Result<()> {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return json_resp(stream, 400, bad_request(format!("body 非法 JSON: {e}"), None)).await
        }
    };
    let name = req.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !valid_group_name(&name) {
        return json_resp(stream, 400, bad_request("组名非法（1~120 字符，不含控制字符）", Some("name"))).await;
    }
    let members: Vec<String> = match req.get("members").and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => Vec::new(),
    };
    if members.is_empty() {
        return json_resp(stream, 400, bad_request("至少要给一个成员模型", Some("members"))).await;
    }
    let routing = req.get("routing").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !routing.is_empty() && !valid_routing(&routing) {
        return json_resp(
            stream,
            400,
            bad_request(
                format!("routing 非法（可用：{} 或 <plugin>/<name>）", ROUTING_KINDS.join(", ")),
                Some("routing"),
            ),
        )
        .await;
    }
    let stays = req.get("stays").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !stays.is_empty() && !valid_stays(&stays) {
        return json_resp(
            stream,
            400,
            bad_request(format!("stays 非法（可用：{}）", STAYS_KINDS.join(", ")), Some("stays")),
        )
        .await;
    }
    let auto_model = req
        .get("auto_model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // auto_model 是**裸目标模型名**（public `auto/<model>` 里的 `<model>`）。
    // 不许带 provider 前缀（`provider/model` 的 `/`）；但**允许字面 `:`**——
    // 网关目录里真有 `kilo/nvidia/nemotron-3.5-lightning:free` 这种模型 id，
    // 它的 public auto id 就是 `auto/nemotron-3.5-lightning:free`（实测 p13 Q3c）。
    if !auto_model.is_empty() && auto_model.contains('/') {
        return json_resp(
            stream,
            400,
            bad_request("auto_model 必须是裸模型名（不含 provider 前缀）", Some("auto_model")),
        )
        .await;
    }
    if !auto_model.is_empty() && !valid_auto_model_name(&auto_model) {
        return json_resp(
            stream,
            400,
            bad_request("auto_model 含非法字符（只允许字母数字与 . _ - :）", Some("auto_model")),
        )
        .await;
    }

    // 白名单：成员必须是网关当前真实模型（或精确存在的嵌套组）。
    let models = match gateway_models(kernel).await {
        Ok(m) => m,
        Err(e) => return json_resp(stream, 502, e).await,
    };
    let state = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<GroupView>> {
        let Some(exe) = find_magpie_exe() else {
            anyhow::bail!("未找到 Magpie CLI");
        };
        load_state(&exe)
    })
    .await;
    let groups = match state {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return json_resp(stream, 502, kerr("K502", format!("读取 Magpie 组失败：{e}"))).await
        }
        Err(e) => {
            return json_resp(stream, 500, kerr("K500", format!("读取 Magpie 组失败：{e}"))).await
        }
    };
    // 嵌套组白名单：只允许**可见**的组。removed/hidden 的组虽然还在
    // providers.json 里占着 id，但它已经不可用了，不能当成员引用。
    let group_ids: HashSet<String> = groups
        .iter()
        .filter(|g| g.visible)
        .map(|g| g.internal_id.clone())
        .collect();
    for m in &members {
        if let Err(e) = check_member(m, &models, &group_ids) {
            return json_resp(stream, 400, bad_request(e, Some("members"))).await;
        }
    }
    // 预检：同名 / 同内部 id（含 footer 里的 hidden 组）——`group add` 同名会静默替换。
    let want_name = norm_name(&name);
    let predicted_id = name_to_id(&name);
    for g in &groups {
        if norm_name(&g.detail.name) == want_name {
            return json_resp(
                stream,
                409,
                kerr("K409", format!("已有同名组：{}（{}）", g.detail.name, g.detail.public_id)),
            )
            .await;
        }
        if g.internal_id == predicted_id {
            return json_resp(
                stream,
                409,
                kerr("K409", format!("已有同 id 的组：{}（{}）", g.internal_id, g.detail.public_id)),
            )
            .await;
        }
    }
    let target_internal = if auto_model.is_empty() {
        String::new()
    } else {
        // 规范内部 id 由**成员**反查得出：目标名 = 匹配成员的模型 id 最后一段，
        // 内部 id = `auto-<slug(目标名)>`（实测 p8–p13）。
        // 注意：不能自己按 auto_model 拼 —— effort 后缀会让目标名与请求不一致
        // （`trae-cn/glm-5.3-flash-max:high` 的目标是 `glm-5.3-flash-max`）。
        let matched: Vec<String> = members
            .iter()
            .filter_map(|m| canonical_auto_id(m, &models))
            .filter(|cid| cid == &format!("auto-{}", auto_slug(&auto_model)))
            .collect();
        let t = match matched.first() {
            Some(t) => t.clone(),
            None => {
                let cands = auto_targets(&members, &models);
                let hint = if cands.is_empty() {
                    "（成员都没匹配到网关模型）".to_string()
                } else {
                    format!("（成员能匹配的 auto 目标：{}）", cands.join(", "))
                };
                return json_resp(
                    stream,
                    400,
                    bad_request(
                        format!("成员里没有匹配 {auto_model} 的模型，无法成为 auto/{auto_model}{hint}"),
                        Some("auto_model"),
                    ),
                )
                .await;
            }
        };
        // 冲突预检：**必须**包含 hidden / removed 组。
        //
        // 实测：hidden 的 found 组仍然占着它的 internal id，新建一个同名组
        // 抢不到它（CLI 会编号成 `-2`，结果还是普通 `group/*`，不是 public auto）。
        // 只看可见组的话，Host 会放行一个必然失败的创建请求。
        if let Err(msg) = check_auto_id_conflict(&groups, &t, &auto_model) {
            return json_resp(stream, 409, kerr("K409", msg)).await;
        }
        t
    };

    // 执行：`group add <name> models=...`
    let mut args: Vec<String> = vec!["group".into(), "add".into(), name.clone()];
    args.push(format!("models={}", members.join(",")));
    if !routing.is_empty() {
        args.push(format!("routing={routing}"));
    }
    if !stays.is_empty() {
        args.push(format!("stays={stays}"));
    }
    let added = tokio::task::spawn_blocking(move || -> anyhow::Result<CliRun> {
        let Some(exe) = find_magpie_exe() else {
            anyhow::bail!("未找到 Magpie CLI");
        };
        run_magpie(&exe, &args)
    })
    .await;
    let run = match added {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return json_resp(stream, 502, kerr("K502", format!("创建组失败：{e}"))).await
        }
        Err(e) => return json_resp(stream, 500, kerr("K500", format!("创建组失败：{e}"))).await,
    };
    if run.code != 0 {
        return json_resp(stream, 502, kerr("K502", format!("创建组失败：{}", cli_failure_message(&run)))).await;
    }
    if parse_add_replaced(&run.stdout) {
        // 同名替换：预检没拦住（竞态，或预检后被别的进程改了）。
        // CLI 已经把原组的成员**换掉**了，不能说「已保留 CLI 结果」——
        // 那样前端会以为原内容还在。如实报告替换已发生。
        return json_resp(
            stream,
            409,
            kerr(
                "K409",
                "同名组已存在，CLI 已把原组替换为新成员（原成员已丢失，请核对组列表）",
            ),
        )
        .await;
    }
    let Some(actual_id) = parse_added_group_id(&run.stdout) else {
        return json_resp(stream, 502, kerr("K502", "CLI 未返回新组 id，无法确认创建结果")).await;
    };
    if actual_id != predicted_id {
        // 同 slug 不同名会被 CLI 编号（`-2`）；这不算冲突，但要让调用方知道真实 id。
        tracing::info!("routegroups: 派生 id {predicted_id} 与实际 id {actual_id} 不同（CLI 编号）");
    }

    // auto 两步：第二步把内部 id 设成规范 `auto-<slug>`。
    if !target_internal.is_empty() {
        let ref_id = cli_ref(&actual_id);
        let set_args: Vec<String> = vec![
            "group".into(),
            "set".into(),
            ref_id.clone(),
            format!("id={target_internal}"),
        ];
        let set_run = tokio::task::spawn_blocking(move || -> anyhow::Result<CliRun> {
            let Some(exe) = find_magpie_exe() else {
                anyhow::bail!("未找到 Magpie CLI");
            };
            run_magpie(&exe, &set_args)
        })
        .await;
        match set_run {
            Ok(Ok(r)) if r.code == 0 => {}
            Ok(Ok(r)) => {
                let msg = cli_failure_message(&r);
                // id 没设成功 → 内部 id 仍是创建时的 actual_id
                cleanup_created(&actual_id, &name, &members).await;
                return json_resp(
                    stream,
                    502,
                    kerr("K502", format!("设置 auto 内部 id 失败（已清理新建的组）：{msg}")),
                )
                .await;
            }
            Ok(Err(e)) => {
                cleanup_created(&actual_id, &name, &members).await;
                return json_resp(
                    stream,
                    502,
                    kerr("K502", format!("设置 auto 内部 id 失败（已清理新建的组）：{e}")),
                )
                .await;
            }
            Err(e) => {
                cleanup_created(&actual_id, &name, &members).await;
                return json_resp(
                    stream,
                    500,
                    kerr("K500", format!("设置 auto 内部 id 失败（已清理新建的组）：{e}")),
                )
                .await;
            }
        }
        // 校验：CLI 实际回读的 public id 必须是 auto/<model>，否则算失败并清理。
        //
        // 不能拿 internal slug 宽松比较：成员失配时 CLI 会把组降级成
        // `group/auto-...`（退出码仍是 0），internal id 看起来完全正确。
        let verified = tokio::task::spawn_blocking({
            let target = target_internal.clone();
            move || -> anyhow::Result<Option<GroupDetail>> {
                let Some(exe) = find_magpie_exe() else {
                    anyhow::bail!("未找到 Magpie CLI");
                };
                show_one(&exe, &target)
            }
        })
        .await;
        let ok = match verified {
            Ok(Ok(Some(d))) => public_auto_id_matches(&d.public_id, &auto_model),
            _ => false,
        };
        if !ok {
            // id 已设成 target_internal，清理必须按它来（actual_id 已不存在）
            cleanup_created(&target_internal, &name, &members).await;
            return json_resp(
                stream,
                502,
                kerr(
                    "K502",
                    format!(
                        "auto 组创建后校验未通过：CLI 回读的 public id 不是 auto/{auto_model}（已清理新建的组）"
                    ),
                ),
            )
            .await;
        }
    }

    // 回读最终形态
    let final_id = if target_internal.is_empty() {
        actual_id.clone()
    } else {
        target_internal.clone()
    };
    let detail = tokio::task::spawn_blocking({
        let fid = final_id.clone();
        move || -> anyhow::Result<Option<GroupDetail>> {
            let Some(exe) = find_magpie_exe() else {
                anyhow::bail!("未找到 Magpie CLI");
            };
            show_one(&exe, &fid)
        }
    })
    .await;
    let detail = match detail {
        Ok(Ok(Some(d))) => d,
        _ => {
            return json_resp(
                stream,
                502,
                kerr("K502", "组已创建，但回读详情失败（请刷新组列表确认）"),
            )
            .await
        }
    };
    let view = GroupView {
        internal_id: final_id,
        detail,
        visible: true,
        restore_id: None,
    };
    json_resp(stream, 200, json!({"ok": true, "group": group_json(&view)})).await
}

/// 清理「刚创建但没成功」的组。
///
/// 只删**确属本次创建**的那个组，四项都要对上：
/// 1. 内部 id 精确等于调用方给出的 `internal_id_now`（不是「猜」出来的）：
///    - `group set id=` 失败时内部 id 从未改成目标 auto id，必须用 `actual_id`；
///    - 设置成功但校验不通过时内部 id 已是目标 auto id，才用 `target_internal`。
///    **绝不能**先拿 `target_internal` 去删——那可能命中同 id 的内置 found 组
///    并把它隐藏掉（实测：删内置 found 组是写 hidden 记录，不是真删）。
/// 2. 显示名与我们建的一致。
/// 3. 成员与我们提交的一致。
/// 4. 不是 Magpie 内置 found 组、也不是已移除的 hidden 组。
async fn cleanup_created(internal_id_now: &str, created_name: &str, created_members: &[String]) {
    if internal_id_now.is_empty() {
        return;
    }
    let shown = tokio::task::spawn_blocking({
        let c = internal_id_now.to_string();
        move || -> anyhow::Result<Option<GroupDetail>> {
            let Some(exe) = find_magpie_exe() else {
                anyhow::bail!("未找到 Magpie CLI");
            };
            show_one(&exe, &c)
        }
    })
    .await;
    let detail = match shown {
        Ok(Ok(Some(d))) => d,
        _ => return,
    };
    // 身份核对：只删确属本次创建的那个组。
    if norm_name(&detail.name) != norm_name(created_name) {
        return;
    }
    // 内置 found 组删了会变成 hidden 记录（不是真删），已移除的组同理——都不碰。
    if detail.found || detail.removed {
        return;
    }
    let mut cur: Vec<String> = detail.members.iter().map(|m| m.id.clone()).collect();
    let mut want: Vec<String> = created_members.to_vec();
    cur.sort();
    want.sort();
    if cur != want {
        return;
    }
    let _ = tokio::task::spawn_blocking({
        let c = internal_id_now.to_string();
        move || -> anyhow::Result<CliRun> {
            let Some(exe) = find_magpie_exe() else {
                anyhow::bail!("未找到 Magpie CLI");
            };
            run_magpie(&exe, &["group".to_string(), "rm".to_string(), cli_ref(&c)])
        }
    })
    .await;
}

/// PUT /api/routegroups/<id>：改组（name / models / routing / stays / pick）。
pub(crate) async fn routegroups_set(
    stream: &mut TcpStream,
    raw_id: &str,
    body: &[u8],
    kernel: &Kernel,
) -> anyhow::Result<()> {
    let id = percent_decode(raw_id);
    let internal = internal_id_of(&id);
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return json_resp(stream, 400, bad_request(format!("body 非法 JSON: {e}"), None)).await
        }
    };
    let new_name = req.get("name").and_then(Value::as_str).map(|s| s.trim().to_string());
    if let Some(n) = &new_name {
        if !valid_group_name(n) {
            return json_resp(stream, 400, bad_request("组名非法（1~120 字符，不含控制字符）", Some("name"))).await;
        }
    }
    let members: Option<Vec<String>> = req.get("members").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });
    if let Some(ms) = &members {
        if ms.is_empty() {
            return json_resp(stream, 400, bad_request("成员列表不能为空", Some("members"))).await;
        }
    }
    let routing = req.get("routing").and_then(Value::as_str).map(|s| s.trim().to_string());
    if let Some(r) = &routing {
        if !r.is_empty() && !valid_routing(r) {
            return json_resp(
                stream,
                400,
                bad_request(
                    format!("routing 非法（可用：{} 或 <plugin>/<name>）", ROUTING_KINDS.join(", ")),
                    Some("routing"),
                ),
            )
            .await;
        }
    }
    let stays = req.get("stays").and_then(Value::as_str).map(|s| s.trim().to_string());
    if let Some(s) = &stays {
        if !s.is_empty() && !valid_stays(s) {
            return json_resp(
                stream,
                400,
                bad_request(format!("stays 非法（可用：{}）", STAYS_KINDS.join(", ")), Some("stays")),
            )
            .await;
        }
    }
    let pick = req.get("pick").and_then(Value::as_str).map(|s| s.trim().to_string());
    if let Some(p) = &pick {
        if !p.is_empty() && !valid_member_shape(p) {
            return json_resp(stream, 400, bad_request("pick 目标格式非法", Some("pick"))).await;
        }
    }
    if new_name.is_none() && members.is_none() && routing.is_none() && stays.is_none() && pick.is_none() {
        return json_resp(stream, 400, bad_request("没有要改的字段", None)).await;
    }

    // 白名单 + 重名预检
    let models = match gateway_models(kernel).await {
        Ok(m) => m,
        Err(e) => return json_resp(stream, 502, e).await,
    };
    let state = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<GroupView>> {
        let Some(exe) = find_magpie_exe() else {
            anyhow::bail!("未找到 Magpie CLI");
        };
        load_state(&exe)
    })
    .await;
    let groups = match state {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return json_resp(stream, 502, kerr("K502", format!("读取 Magpie 组失败：{e}"))).await
        }
        Err(e) => {
            return json_resp(stream, 500, kerr("K500", format!("读取 Magpie 组失败：{e}"))).await
        }
    };
    let Some(target) = groups.iter().find(|g| g.internal_id == internal && g.visible) else {
        return json_resp(stream, 404, kerr("K404", format!("组不存在：{internal}"))).await;
    };
    // 嵌套组白名单：只允许**可见**的组（removed/hidden 的组不可引用）。
    let group_ids: HashSet<String> = groups
        .iter()
        .filter(|g| g.visible)
        .map(|g| g.internal_id.clone())
        .collect();
    if let Some(ms) = &members {
        for m in ms {
            if let Err(e) = check_member(m, &models, &group_ids) {
                return json_resp(stream, 400, bad_request(e, Some("members"))).await;
            }
        }
        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        for g in &groups {
            edges.insert(
                g.internal_id.clone(),
                g.detail
                    .members
                    .iter()
                    .filter_map(|m| m.id.strip_prefix("group/").map(|s| s.to_string()))
                    .collect(),
            );
        }
        if nested_cycle(&internal, ms, &edges) {
            return json_resp(
                stream,
                400,
                bad_request("成员里含有本组或包含本组的组（会成环）", Some("members")),
            )
            .await;
        }
        // 深度超限不是环：CLI 报的是 "a group in a group goes at most 8 deep"，
        // 给前端不同的文案，别一律说成成环。
        let depth = nested_depth(ms, &edges);
        if depth > MAX_NEST_DEPTH {
            return json_resp(
                stream,
                400,
                bad_request(
                    format!("嵌套层级 {depth} 超过 Magpie 上限（最多 {MAX_NEST_DEPTH} 层）"),
                    Some("members"),
                ),
            )
            .await;
        }
    }
    if let Some(p) = &pick {
        if !p.is_empty() {
            let allowed = members
                .as_ref()
                .map(|ms| ms.iter().any(|m| m == p))
                .unwrap_or_else(|| target.detail.members.iter().any(|m| &m.id == p));
            if !allowed {
                return json_resp(
                    stream,
                    400,
                    bad_request(format!("pick 目标不在该组成员里：{p}"), Some("pick")),
                )
                .await;
            }
        }
    }
    if let Some(n) = &new_name {
        let want = norm_name(n);
        for g in &groups {
            if g.internal_id == internal {
                continue;
            }
            if norm_name(&g.detail.name) == want {
                return json_resp(
                    stream,
                    409,
                    kerr("K409", format!("已有同名组：{}（{}）", g.detail.name, g.detail.public_id)),
                )
                .await;
            }
        }
    }

    let mut args: Vec<String> = vec!["group".into(), "set".into(), cli_ref(&internal)];
    if let Some(n) = &new_name {
        args.push(format!("name={n}"));
    }
    if let Some(ms) = &members {
        args.push(format!("models={}", ms.join(",")));
    }
    if let Some(r) = &routing {
        if !r.is_empty() {
            args.push(format!("routing={r}"));
        }
    }
    if let Some(s) = &stays {
        if !s.is_empty() {
            args.push(format!("stays={s}"));
        }
    }
    if let Some(p) = &pick {
        if !p.is_empty() {
            args.push(format!("pick={p}"));
        }
    }
    let set_run = tokio::task::spawn_blocking(move || -> anyhow::Result<CliRun> {
        let Some(exe) = find_magpie_exe() else {
            anyhow::bail!("未找到 Magpie CLI");
        };
        run_magpie(&exe, &args)
    })
    .await;
    let run = match set_run {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return json_resp(stream, 502, kerr("K502", format!("修改组失败：{e}"))).await,
        Err(e) => return json_resp(stream, 500, kerr("K500", format!("修改组失败：{e}"))).await,
    };
    if run.code != 0 {
        return json_resp(stream, 502, kerr("K502", format!("修改组失败：{}", cli_failure_message(&run)))).await;
    }
    let detail = tokio::task::spawn_blocking({
        let i = internal.clone();
        move || -> anyhow::Result<Option<GroupDetail>> {
            let Some(exe) = find_magpie_exe() else {
                anyhow::bail!("未找到 Magpie CLI");
            };
            show_one(&exe, &i)
        }
    })
    .await;
    let detail = match detail {
        Ok(Ok(Some(d))) => d,
        _ => {
            return json_resp(
                stream,
                502,
                kerr("K502", "组已修改，但回读详情失败（请刷新组列表确认）"),
            )
            .await
        }
    };
    if let Some(n) = &new_name {
        if norm_name(&detail.name) != norm_name(n) {
            return json_resp(
                stream,
                502,
                kerr("K502", format!("改名后组名与请求不一致（当前 {}）", detail.name)),
            )
            .await;
        }
    }
    let view = GroupView {
        internal_id: internal.clone(),
        detail,
        visible: true,
        restore_id: None,
    };
    json_resp(stream, 200, json!({"ok": true, "group": group_json(&view)})).await
}

/// DELETE /api/routegroups/<id>：删组（内置 found 组是「隐藏」，可 restore）。
pub(crate) async fn routegroups_rm(stream: &mut TcpStream, raw_id: &str) -> anyhow::Result<()> {
    let id = percent_decode(raw_id);
    let internal = internal_id_of(&id);
    if internal.is_empty() {
        return json_resp(stream, 400, bad_request("缺组 id", Some("id"))).await;
    }
    let rm_run = tokio::task::spawn_blocking({
        let i = internal.clone();
        move || -> anyhow::Result<CliRun> {
            let Some(exe) = find_magpie_exe() else {
                anyhow::bail!("未找到 Magpie CLI");
            };
            run_magpie(&exe, &["group".to_string(), "rm".to_string(), cli_ref(&i)])
        }
    })
    .await;
    let run = match rm_run {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return json_resp(stream, 502, kerr("K502", format!("删除组失败：{e}"))).await,
        Err(e) => return json_resp(stream, 500, kerr("K500", format!("删除组失败：{e}"))).await,
    };
    if run.code != 0 {
        let msg = cli_failure_message(&run);
        let not_found = msg.contains("no group");
        return json_resp(
            stream,
            if not_found { 404 } else { 502 },
            kerr(if not_found { "K404" } else { "K502" }, msg),
        )
        .await;
    }
    let (hidden, restore_id) = parse_removed_outcome(&run.stdout);
    json_resp(
        stream,
        200,
        json!({
            "ok": true,
            "removed": true,
            "hidden": hidden,
            "restorable": hidden,
            "restore_id": restore_id.unwrap_or_else(|| internal.clone()),
            "internal_id": internal,
        }),
    )
    .await
}

/// POST /api/routegroups/<id>/restore：把被移除的 Magpie found 组放回来。
pub(crate) async fn routegroups_restore(stream: &mut TcpStream, raw_id: &str) -> anyhow::Result<()> {
    let id = percent_decode(raw_id);
    let id = id.trim_end_matches("/restore").trim().to_string();
    let internal = internal_id_of(&id);
    if internal.is_empty() {
        return json_resp(stream, 400, bad_request("缺组 id", Some("id"))).await;
    }
    // 传给 `group restore` 的必须是**权威** restore id。
    //
    // 实测：只有内置 found 组被删后会留下 hidden 记录、可恢复，它的 restore id 由
    // `group show` 后缀（`removed · Magpie group restore <id>`）给出。用户自己创建
    // 的组删除是真删除，没有 restore id（CLI 报 `no group` → 404）。
    // 查不到时退回 internal id，让 CLI 自己给结论（`no group` / `isn't removed`）。
    let state = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<GroupView>> {
        let Some(exe) = find_magpie_exe() else {
            anyhow::bail!("未找到 Magpie CLI");
        };
        load_state(&exe)
    })
    .await;
    let restore_arg = match state {
        Ok(Ok(groups)) => pick_restore_arg(&groups, &id, &internal),
        // 读列表失败不阻断恢复：退回 internal id，让 CLI 报真实原因。
        _ => internal.clone(),
    };
    let restore_run = tokio::task::spawn_blocking({
        let i = restore_arg.clone();
        move || -> anyhow::Result<CliRun> {
            let Some(exe) = find_magpie_exe() else {
                anyhow::bail!("未找到 Magpie CLI");
            };
            run_magpie(&exe, &["group".to_string(), "restore".to_string(), i])
        }
    })
    .await;
    let run = match restore_run {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return json_resp(stream, 502, kerr("K502", format!("恢复组失败：{e}"))).await,
        Err(e) => return json_resp(stream, 500, kerr("K500", format!("恢复组失败：{e}"))).await,
    };
    if run.code != 0 {
        let msg = cli_failure_message(&run);
        let not_found = msg.contains("no group");
        return json_resp(
            stream,
            if not_found { 404 } else { 502 },
            kerr(if not_found { "K404" } else { "K502" }, msg),
        )
        .await;
    }
    json_resp(stream, 200, json!({"ok": true, "restored": true, "internal_id": internal})).await
}

/// 从当前状态里挑出传给 `group restore` 的**权威** restore id。
///
/// 调用方可能用三种形态指向同一个组：内部 id（`auto-beam-free`）、权威 restore
/// id（同形）、public id（`auto/beam-free`）。命中组后只用它的 `restore_id`；
/// 命中但该组没有 restore id（用户自建组被删）或完全没命中，都退回 internal id，
/// 让 CLI 自己给出 `no group` / `isn't removed` 的真实原因。
fn pick_restore_arg(groups: &[GroupView], id: &str, internal: &str) -> String {
    groups
        .iter()
        .find(|g| {
            g.internal_id == internal
                || g.restore_id.as_deref() == Some(internal)
                || g.detail.public_id == id
        })
        .and_then(|g| g.restore_id.clone())
        .unwrap_or_else(|| internal.to_string())
}

// ───────────────────────── 小工具 ─────────────────────────

/// URL 解码（前端可能把 `auto/glm-5.3` 编码成 `auto%2Fglm-5.3`）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── 真实 CLI 输出 fixture（p18 实测捕获，不是手写） ──────────────────
//
// 来源：.scratch/probe_p18_fixture.py（2026-11-17 实跑捕获）
// 逐字复制自 `magpie groups` / `magpie group show` 的 stdout。
//
// 这些 fixture 锁死三件事：
// 1. 列宽**自适应**（名字越长，后面的列间距越大）→ 不能按固定字符偏移切列；
// 2. 显示名里的**连续空格**被 CLI 原样保留（`RSI  Double  Space`）；
// 3. `found` 标记出现在**名称列尾部**，而 `group show` 首行才带权威的
//    `found by ...` 后缀。

/// `magpie groups` 的真实 stdout（p18）。
#[cfg(test)]
const FIXTURE_GROUPS: &str = "  RSI Fixture Plain          group/rsi-fixture-plain   smart   opencode-zen/beam-free · workbuddy/glm-5.3\n  RSI  Double  Space         group/rsi-double-space    smart   opencode-zen/beam-free\n  RSI Fixture Nest           group/rsi-fixture-nest    smart   group/rsi-fixture-plain · opencode-zen/flint-40b-free\n  RSI Fixture Manual         group/rsi-fixture-manual  manual  ● opencode-zen/beam-free · workbuddy/glm-5.3\n  Beam Free found            auto/beam-free            smart   opencode-zen/beam-free · kilo/reflection/beam-free\n  DeepSeek V4.1 Flash found  auto/deepseek-v4.1-flash  smart   opencode-zen/deepseek-v4.1-flash · workbuddy/deepseek-v4.1-flash · trae-cn/deepseek-v4.1-flash\n  DeepSeek V4 Pro found      auto/deepseek-v4-pro      smart   opencode-zen/deepseek-v4-pro · workbuddy/deepseek-v4-pro\n  GLM-5 found                auto/glm-5                smart   kiro/glm-5 · trae-cn/glm-5\n  GLM-5.3 found              auto/glm-5.3              smart   workbuddy/glm-5.3 · trae-cn/glm-5.3\n  GLM-5.3-Flash found        auto/glm-5.3-flash        smart   workbuddy/glm-5.3-flash · trae-cn/glm-5.3-flash\n  GLM-5.2 found              auto/glm-5.2              smart   workbuddy/glm-5.2 · trae-cn/glm-5.2\n";

/// `magpie group rm group/auto-beam-free` 之后的真实 stdout（p18）——多了 removed footer。
#[cfg(test)]
const FIXTURE_GROUPS_REMOVED: &str = "  RSI Fixture Plain          group/rsi-fixture-plain   smart   opencode-zen/beam-free · workbuddy/glm-5.3\n  RSI  Double  Space         group/rsi-double-space    smart   opencode-zen/beam-free\n  RSI Fixture Nest           group/rsi-fixture-nest    smart   group/rsi-fixture-plain · opencode-zen/flint-40b-free\n  RSI Fixture Manual         group/rsi-fixture-manual  manual  ● opencode-zen/beam-free · workbuddy/glm-5.3\n  DeepSeek V4.1 Flash found  auto/deepseek-v4.1-flash  smart   opencode-zen/deepseek-v4.1-flash · workbuddy/deepseek-v4.1-flash · trae-cn/deepseek-v4.1-flash\n  DeepSeek V4 Pro found      auto/deepseek-v4-pro      smart   opencode-zen/deepseek-v4-pro · workbuddy/deepseek-v4-pro\n  GLM-5 found                auto/glm-5                smart   kiro/glm-5 · trae-cn/glm-5\n  GLM-5.3 found              auto/glm-5.3              smart   workbuddy/glm-5.3 · trae-cn/glm-5.3\n  GLM-5.3-Flash found        auto/glm-5.3-flash        smart   workbuddy/glm-5.3-flash · trae-cn/glm-5.3-flash\n  GLM-5.2 found              auto/glm-5.2              smart   workbuddy/glm-5.2 · trae-cn/glm-5.2\n\n  removed: auto-beam-free · magpie group restore <id> brings one back\n    (each is a {\"hidden\": true} record in providers.json that keeps it removed; deleting the record brings it back)\n";

/// `magpie group show group/rsi-fixture-plain` 的真实 stdout（p18）。
#[cfg(test)]
const FIXTURE_SHOW_PLAIN: &str = "  RSI Fixture Plain  group/rsi-fixture-plain\n  routing   smart\n  stays     auto\n  models    1 opencode-zen/beam-free  OpenCode Zen · Beam Free\n            2 workbuddy/glm-5.3  WorkBuddy · GLM-5.3\n";

/// `magpie group show group/rsi-double-space` 的真实 stdout（p18）。
#[cfg(test)]
const FIXTURE_SHOW_DOUBLE_SPACE: &str = "  RSI  Double  Space  group/rsi-double-space\n  routing   smart\n  stays     auto\n  models    1 opencode-zen/beam-free  OpenCode Zen · Beam Free\n";

/// `magpie group show group/rsi-fixture-nest` 的真实 stdout（p18）。
#[cfg(test)]
const FIXTURE_SHOW_NEST: &str = "  RSI Fixture Nest  group/rsi-fixture-nest\n  routing   smart\n  stays     auto\n  models    1 group/rsi-fixture-plain  routing group RSI Fixture Plain · smart · opencode-zen/beam-free, workbuddy/glm-5.3\n            2 opencode-zen/flint-40b-free  OpenCode Zen · Flint 40B Free\n";

/// `magpie group show group/rsi-fixture-manual` 的真实 stdout（p18）。
#[cfg(test)]
const FIXTURE_SHOW_MANUAL: &str = "  RSI Fixture Manual  group/rsi-fixture-manual\n  routing   manual  every request to opencode-zen/beam-free; magpie group pick rsi-fixture-manual <model> picks another\n  stays     auto\n  models    1 opencode-zen/beam-free ●  OpenCode Zen · Beam Free\n            2 workbuddy/glm-5.3  WorkBuddy · GLM-5.3\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_id_rules() {
        assert!(valid_internal_id("auto-minimax-m3-1-flash"));
        assert!(valid_internal_id("rsi-t5"));
        assert!(valid_internal_id("a.b"));
        assert!(valid_internal_id("abc123"));
        // 不许：大写、下划线、斜杠、空格、首尾短横线、连续双短横线
        assert!(!valid_internal_id("RSI-T5"));
        assert!(!valid_internal_id("a_b"));
        assert!(!valid_internal_id("a/b"));
        assert!(!valid_internal_id("a b"));
        assert!(!valid_internal_id("-ab"));
        assert!(!valid_internal_id("ab-"));
        assert!(!valid_internal_id("a--b"));
        assert!(!valid_internal_id(""));
    }

    #[test]
    fn member_shape_rules() {
        assert!(valid_member_shape("workbuddy/minimax-m3.1-flash"));
        assert!(valid_member_shape("kilo/reflection/beam-free"));
        assert!(valid_member_shape("codex/gpt-6.1-sol:high"));
        assert!(valid_member_shape("group/auto-glm-5-3"));
        assert!(!valid_member_shape("no-slash"));
        assert!(!valid_member_shape(""));
        assert!(!valid_member_shape("a b/c"));
        // 逗号/等号会破坏 `models=` / `k=v` 的参数解析
        assert!(!valid_member_shape("a/b,c"));
        assert!(!valid_member_shape("a/b=c"));
    }

    #[test]
    fn routing_and_stays_rules() {
        for k in ROUTING_KINDS {
            assert!(valid_routing(k), "{k} 应合法");
        }
        assert!(valid_routing("someplugin/somename"));
        assert!(!valid_routing("not-a-routing"));
        assert!(!valid_routing("a/b/c"));
        assert!(!valid_routing("/name"));
        for k in STAYS_KINDS {
            assert!(valid_stays(k), "{k} 应合法");
        }
        assert!(!valid_stays("not-a-stays"));
    }

    #[test]
    fn name_to_id_matches_cli_derivation() {
        // 实测：CLI 从名字派生的内部 id
        assert_eq!(name_to_id("rsi-probe-plain"), "rsi-probe-plain");
        assert_eq!(name_to_id("RSI Probe Space"), "rsi-probe-space");
        assert_eq!(name_to_id("Auto.GLM 5.3"), "auto.glm-5.3");
        assert_eq!(name_to_id("Spaced  Out"), "spaced-out");
        assert_eq!(name_to_id("Mixed_Case_Name"), "mixed-case-name");
        assert_eq!(name_to_id("dots-and-dashes"), "dots-and-dashes");
        assert_eq!(name_to_id("Ünïcode Näme"), "n-code-n-me");
        assert_eq!(name_to_id("a/b"), "a-b");
        assert_eq!(name_to_id("trailing-"), "trailing");
    }

    #[test]
    fn auto_slug_rules() {
        assert_eq!(auto_slug("minimax-m3.1-flash"), "minimax-m3-1-flash");
        assert_eq!(auto_slug("glm-5.3"), "glm-5-3");
        assert_eq!(auto_slug("deepseek-v4.1-flash"), "deepseek-v4-1-flash");
        assert_eq!(auto_slug("GLM-5"), "glm-5");
        assert_eq!(auto_slug("minimax_m3_1_flash"), "minimax-m3-1-flash");
    }

    #[test]
    fn internal_id_of_maps_public_id() {
        // auto/<model> 的内部 id 是 auto-<dash-slug>，不是 public id 的后缀
        assert_eq!(internal_id_of("auto/glm-5.3"), "auto-glm-5-3");
        assert_eq!(internal_id_of("auto/minimax-m3.1-flash"), "auto-minimax-m3-1-flash");
        assert_eq!(internal_id_of("auto/deepseek-v4.1-flash"), "auto-deepseek-v4-1-flash");
        // group/<id> 的内部 id 就是 id
        assert_eq!(internal_id_of("group/rsi-probe"), "rsi-probe");
        assert_eq!(internal_id_of("group/auto-glm-5-3"), "auto-glm-5-3");
        // 裸 id 按内部 id 处理
        assert_eq!(internal_id_of("rsi-probe"), "rsi-probe");
    }

    #[test]
    fn public_auto_id_matches_needs_auto_prefix_and_same_slug() {
        // 实测（.scratch/p2e_rule2_out.txt）：CLI 回读的 public 尾段保留成员原始形态，
        // 所以只能用 CLI 实际回读结果判，不能由 internal id 反推。
        assert!(public_auto_id_matches("auto/Doubao_1_6", "Doubao_1_6"));
        assert!(public_auto_id_matches("auto/doubao-seed-2.1-turbo", "Doubao-Seed-2.1-Turbo"));
        assert!(public_auto_id_matches("auto/glm-5.2_advisor_doubao", "glm-5.2_advisor_doubao"));
        assert!(public_auto_id_matches("auto/flint-v1.0:free", "flint-v1.0:free"));
        assert!(public_auto_id_matches("auto/glyph-cluster", "glyph-cluster"));

        // 前缀不是 `auto/`：成员失配时 CLI 降级成 `group/auto-<slug>`（退出码仍是 0）
        assert!(!public_auto_id_matches("group/auto-beam-free", "beam-free"));
        assert!(!public_auto_id_matches("group/auto-glm-5-3", "glm-5.3"));
        assert!(!public_auto_id_matches("auto-beam-free", "beam-free"));
        assert!(!public_auto_id_matches("", "beam-free"));

        // 尾段不是同一个模型（规范化后仍不等）
        assert!(!public_auto_id_matches("auto/glm-5.3", "glm-5.3-flash"));
        assert!(!public_auto_id_matches("auto/beam-free", "reflection/beam-free"));
        // 规范化能吸收大小写/点号/下划线差异，但 `:free` 是模型 id 的一部分
        assert!(!public_auto_id_matches("auto/flint-v1.0", "flint-v1.0:free"));
    }

    #[test]
    fn internal_and_public_auto_id_are_different_spaces() {
        // public `auto/Doubao_1_6` 的内部 id 是 `auto-doubao-1-6`，
        // 但**不能**反过来从 `auto-doubao-1-6` 还原出 `Doubao_1_6`。
        assert_eq!(internal_id_of("auto/Doubao_1_6"), "auto-doubao-1-6");
        assert_eq!(internal_id_of("auto/Doubao-Seed-2.1-Turbo"), "auto-doubao-seed-2-1-turbo");
        assert_eq!(internal_id_of("auto/glm-5.2_advisor_doubao"), "auto-glm-5-2-advisor-doubao");
        assert_eq!(internal_id_of("auto/flint-v1.0:free"), "auto-flint-v1-0-free");
        // 两个不同 public 尾段可以落到同一个 internal slug → internal id 不是无损的
        assert_eq!(
            internal_id_of("auto/Doubao_1_6"),
            internal_id_of("auto/doubao-1.6"),
            "internal id 丢失原始分隔符信息"
        );
        // 反推会丢掉下划线/点号形态，所以绝不能拿它当 public 校验
        assert_ne!(internal_id_of("auto/Doubao_1_6"), "auto-Doubao_1_6");
    }

    #[test]
    fn cli_ref_never_uses_bare_id() {
        // 裸 id 可能命中内置 found 组并把它藏掉；必须带 group/ 前缀
        assert_eq!(cli_ref("auto-glm-5-3"), "group/auto-glm-5-3");
        assert_eq!(cli_ref("rsi-probe"), "group/rsi-probe");
    }

    #[test]
    fn member_base_and_effort_suffix() {
        assert_eq!(member_base("codex/gpt-6.1-sol:high"), "codex/gpt-6.1-sol");
        assert_eq!(member_suffixes("codex/gpt-6.1-sol:high"), vec!["high".to_string()]);
        // 字面 `:free` 是模型 id 的一部分
        assert_eq!(
            member_base("kilo/nvidia/nemotron-3.5-super-vl:free"),
            "kilo/nvidia/nemotron-3.5-super-vl"
        );
        assert_eq!(member_base("kilo/kilo-auto/free"), "kilo/kilo-auto/free");
        assert!(valid_effort_suffix("high"));
        assert!(valid_effort_suffix("low+high"));
        // `fast` 是厂商快速模式修饰符，不是 effort（实测 p14/p15）
        assert!(!valid_effort_suffix("fast"));
        assert!(is_fast_suffix("fast"));
        assert!(!valid_effort_suffix("free"));
        assert!(!valid_effort_suffix("7b"));
    }

    /// 实测网关目录里真实存在的模型（.scratch/_p2_models.json，90 个，其中 7 个是 auto/* 组）。
    fn catalog() -> HashSet<String> {
        [
            "workbuddy/minimax-m3.1-flash",
            "workbuddy/glm-5.3",
            "opencode-zen/beam-free",
            "opencode-zen/flint-40b-free",
            "opencode-zen/nemotron-3.5-super-vl-free",
            "kilo/kilo-auto/free",
            "kilo/reflection/beam-free",
            "kilo/stealth/glyph-cluster",
            "kilo/recursive/flint-v1.0:free",
            "kilo/nvidia/nemotron-3.5-super-vl:free",
            "kilo/nvidia/nemotron-3.5-lightning:free",
            "trae-cn/Doubao-Seed-Code",
            "trae-cn/glm-5.2_advisor_doubao",
            "trae-cn/glm-5.3-flash-max",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn auto_target_uses_last_path_segment() {
        let m = catalog();
        // 最后一段，不是 provider 之后的完整路径
        assert_eq!(
            auto_target("kilo/reflection/beam-free", &m).as_deref(),
            Some("beam-free")
        );
        assert_eq!(auto_target("kilo/kilo-auto/free", &m).as_deref(), Some("free"));
        assert_eq!(
            auto_target("workbuddy/minimax-m3.1-flash", &m).as_deref(),
            Some("minimax-m3.1-flash")
        );
        // 不在目录里就不参与判定
        assert_eq!(auto_target("workbuddy/nope-model", &m), None);
    }

    #[test]
    fn auto_target_keeps_literal_free_suffix() {
        let m = catalog();
        // 字面 `:free` 是模型 id 的一部分 → 计入目标名
        assert_eq!(
            auto_target("kilo/nvidia/nemotron-3.5-super-vl:free", &m).as_deref(),
            Some("nemotron-3.5-super-vl:free")
        );
        assert_eq!(
            auto_target("kilo/recursive/flint-v1.0:free", &m).as_deref(),
            Some("flint-v1.0:free")
        );
    }

    #[test]
    fn auto_target_strips_effort_suffix() {
        let m = catalog();
        // `:high` 是 effort → 不计入目标名
        assert_eq!(
            auto_target("trae-cn/glm-5.3-flash-max:high", &m).as_deref(),
            Some("glm-5.3-flash-max")
        );
        // 但如果完整 id 在目录里，就按完整 id 走（`:free` 的情形）
        assert_eq!(
            auto_target("kilo/nvidia/nemotron-3.5-lightning:free", &m).as_deref(),
            Some("nemotron-3.5-lightning:free")
        );
    }

    #[test]
    fn canonical_auto_id_matches_probe_results() {
        let m = catalog();
        // p8/p9/p12/p13 实测：这些 id 能拿到 public auto/<model>
        let ok: &[(&str, &str)] = &[
            ("kilo/kilo-auto/free", "auto-free"),
            ("kilo/recursive/flint-v1.0:free", "auto-flint-v1-0-free"),
            ("kilo/stealth/glyph-cluster", "auto-glyph-cluster"),
            ("kilo/nvidia/nemotron-3.5-super-vl:free", "auto-nemotron-3-5-super-vl-free"),
            ("kilo/nvidia/nemotron-3.5-lightning:free", "auto-nemotron-3-5-lightning-free"),
            ("trae-cn/Doubao-Seed-Code", "auto-doubao-seed-code"),
            ("trae-cn/glm-5.2_advisor_doubao", "auto-glm-5-2-advisor-doubao"),
            ("trae-cn/glm-5.3-flash-max:high", "auto-glm-5-3-flash-max"),
            ("workbuddy/minimax-m3.1-flash", "auto-minimax-m3-1-flash"),
        ];
        for (member, want) in ok {
            assert_eq!(
                canonical_auto_id(member, &m).as_deref(),
                Some(*want),
                "成员 {member} 应推出 {want}"
            );
        }
    }

    #[test]
    fn canonical_auto_id_rejects_provider_relative_path_form() {
        let m = catalog();
        // p7/p8 实测：provider 之后的完整路径当目标名 → 不是 auto
        assert_eq!(
            canonical_auto_id("kilo/reflection/beam-free", &m).as_deref(),
            Some("auto-beam-free")
        );
        assert_ne!(
            canonical_auto_id("kilo/reflection/beam-free", &m).as_deref(),
            Some("auto-reflection-beam-free")
        );
    }

    #[test]
    fn auto_targets_allows_mixed_members() {
        let m = catalog();
        // p12 实测：混入别的模型也能成 auto，只要有一个成员匹配
        let members = vec![
            "workbuddy/minimax-m3.1-flash".to_string(),
            "opencode-zen/beam-free".to_string(),
        ];
        let t = auto_targets(&members, &m);
        assert!(t.contains(&"minimax-m3.1-flash".to_string()), "{t:?}");
        assert!(t.contains(&"beam-free".to_string()), "{t:?}");
    }

    #[test]
    fn member_whitelist_prefers_full_id_match() {
        let models: HashSet<String> = [
            "workbuddy/minimax-m3.1-flash".to_string(),
            "kilo/nvidia/nemotron-3.5-super-vl:free".to_string(),
            "codex/gpt-6.1-sol".to_string(),
        ]
        .into_iter()
        .collect();
        let groups: HashSet<String> = ["auto-glm-5-3".to_string()].into_iter().collect();
        // 完整 id 精确命中（含字面 :free）
        assert!(check_member("kilo/nvidia/nemotron-3.5-super-vl:free", &models, &groups).is_ok());
        // effort 后缀解释
        assert!(check_member("codex/gpt-6.1-sol:high", &models, &groups).is_ok());
        // 不认识的模型
        assert!(check_member("workbuddy/nope", &models, &groups).is_err());
        // 基础部分不认识、后缀也不是 effort
        assert!(check_member("kilo/nvidia/nemotron-3.5-super-vl:7b", &models, &groups).is_err());
        // 嵌套组必须精确存在
        assert!(check_member("group/auto-glm-5-3", &models, &groups).is_ok());
        assert!(check_member("group/nope", &models, &groups).is_err());
    }

    #[test]
    fn auto_model_name_allows_literal_colon() {
        // public `auto/nemotron-3.5-lightning:free` 是真实存在的 auto id
        // （实测 p13 Q3c）——`:free` 是模型 id 的一部分，不能被拒。
        assert!(valid_auto_model_name("nemotron-3.5-lightning:free"));
        assert!(valid_auto_model_name("minimax-m3.1-flash"));
        assert!(valid_auto_model_name("glm-5.2_advisor_doubao"));
        // provider 前缀不行
        assert!(!valid_auto_model_name("kilo/nvidia/nemotron-3.5-lightning:free"));
        assert!(!valid_auto_model_name(""));
        assert!(!valid_auto_model_name("has space"));
        assert!(!valid_auto_model_name("semi;colon"));
    }

    #[test]
    fn public_id_token_allows_literal_colon_in_tail() {
        // 真实 public id（实测 p13 Q3c）：`auto/nemotron-3.5-lightning:free`。
        // `:free` 是模型 id 的一部分，不是 effort —— 这里若不放行 `:`，
        // `parse_groups_line` 会把它当普通模型名，真实 auto 组会被误判成「非组」。
        assert!(is_public_id_token("auto/nemotron-3.5-lightning:free"));
        // 普通形态仍然成立
        assert!(is_public_id_token("auto/minimax-m3.1-flash"));
        assert!(is_public_id_token("group/rsi-fixture-plain"));
        assert!(is_public_id_token("group/auto-glm-5-3-flash-max-high"));
        // 字面冒号可出现在尾段任意位置（模型名本身也可能带冒号）
        assert!(is_public_id_token("auto/a:b:c"));
        // 拒绝：空尾段
        assert!(!is_public_id_token("auto/"));
        assert!(!is_public_id_token("group/"));
        // 拒绝：额外 `/`（尾段不允许再分段）
        assert!(!is_public_id_token("auto/nemotron/3.5"));
        assert!(!is_public_id_token("group/a/b"));
        // 拒绝：前缀不是 auto/group
        assert!(!is_public_id_token("workbuddy/minimax-m3.1-flash"));
        assert!(!is_public_id_token("autox/minimax-m3.1-flash"));
        // 拒绝：没有斜杠 / 空串 / 非法字符
        assert!(!is_public_id_token(""));
        assert!(!is_public_id_token("auto-minimax-m3.1-flash"));
        assert!(!is_public_id_token("auto/has space"));
        assert!(!is_public_id_token("auto/semi;colon"));
    }

    #[test]
    fn check_member_rejects_fast_with_readable_reason() {
        // 实测 p14/p15：`:fast` 是厂商快速模式修饰符，CLI 会去找 fast 能力，
        // 当前网关目录里没有模型支持 → 提前给中文原因。
        let models = catalog();
        let groups: HashSet<String> = HashSet::new();
        let e = check_member("trae-cn/glm-5.3-flash-max:fast", &models, &groups)
            .expect_err("fast 应被拒");
        assert!(e.contains("fast"), "错误文案要说明 fast：{e}");
        assert!(e.contains("trae-cn/glm-5.3-flash-max:fast"), "错误文案要带成员：{e}");
        // effort 仍然放行
        assert!(check_member("trae-cn/glm-5.3-flash-max:high", &models, &groups).is_ok());
    }

    #[test]
    fn auto_target_strips_effort_but_not_literal_free() {
        let m = catalog();
        // `:high` 是 effort → 不计入目标名
        assert_eq!(
            auto_target("trae-cn/glm-5.3-flash-max:high", &m).as_deref(),
            Some("glm-5.3-flash-max")
        );
        // 字面 `:free` 在目录里 → 完整 id 精确匹配优先，目标名含 `:free`
        assert_eq!(
            auto_target("kilo/nvidia/nemotron-3.5-lightning:free", &m).as_deref(),
            Some("nemotron-3.5-lightning:free")
        );
    }

    #[test]
    fn parse_groups_line_real_outputs() {
        let l = "  Beam Free found            auto/beam-free            smart  opencode-zen/beam-free · kilo/reflection/beam-free";
        let r = parse_groups_line(l).expect("应解析出组行");
        // 名字**原样**保留（含尾部 `found` 标记）——found 状态以 group show 为准
        assert_eq!(r.name, "Beam Free found");
        assert_eq!(r.public_id, "auto/beam-free");
        assert_eq!(r.routing, "smart");
        assert_eq!(
            r.members,
            vec![
                "opencode-zen/beam-free".to_string(),
                "kilo/reflection/beam-free".to_string()
            ]
        );
        assert!(r.selected.is_none());
        assert!(r.found_marker);
    }

    #[test]
    fn parse_groups_line_manual_routing() {
        let l = "  rsi-t-p1                   group/rsi-t-p1            manual  ● kiro/glm-5 · kiro/deepseek-3.2";
        let r = parse_groups_line(l).expect("应解析出组行");
        assert_eq!(r.name, "rsi-t-p1");
        assert_eq!(r.public_id, "group/rsi-t-p1");
        assert_eq!(r.routing, "manual");
        assert_eq!(r.members[0], "kiro/glm-5");
        assert_eq!(r.members.len(), 2);
        assert_eq!(r.selected.as_deref(), Some("kiro/glm-5"));
        assert!(!r.found_marker);
    }

    #[test]
    fn parse_groups_line_long_name_group() {
        let l = "  rsi-probe-long-name-with-many-characters  group/rsi-probe-long-name-with-many-characters  smart  opencode-zen/beam-free · opencode-zen/flint-40b-free";
        let r = parse_groups_line(l).expect("应解析出组行");
        assert_eq!(r.name, "rsi-probe-long-name-with-many-characters");
        assert_eq!(r.public_id, "group/rsi-probe-long-name-with-many-characters");
        assert_eq!(r.members.len(), 2);
    }

    #[test]
    fn parse_groups_line_nested_group_member_does_not_steal_public_id() {
        // 成员里出现 group/<id> 时，public id 仍是靠前那个
        let l = "  nested                     group/nested              smart  group/auto-glm-5-3 · opencode-zen/beam-free";
        let r = parse_groups_line(l).expect("应解析出组行");
        assert_eq!(r.public_id, "group/nested");
        assert_eq!(r.members[0], "group/auto-glm-5-3");
    }

    #[test]
    fn parse_groups_output_reads_removed_footer() {
        let stdout = "  Beam Free found            auto/beam-free            smart  opencode-zen/beam-free · kilo/reflection/beam-free\n  GLM-5.3 found              auto/glm-5.3              smart  workbuddy/glm-5.3 · trae-cn/glm-5.3\n\n  removed: auto-glm-5-3, auto-glm-5-3-2 · Magpie group restore <id> brings one back\n    (each is a {\"hidden\": true} record in providers.json that keeps it removed; deleting the record brings it back)\n";
        let (rows, hidden) = parse_groups_output(stdout);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].public_id, "auto/beam-free");
        assert_eq!(hidden, vec!["auto-glm-5-3".to_string(), "auto-glm-5-3-2".to_string()]);
    }

    #[test]
    fn parse_groups_line_skips_removed_block() {
        assert!(parse_groups_line("  removed: auto-glm-5-3 · Magpie group restore <id> brings one back").is_none());
        assert!(parse_groups_line("    (each is a {\"hidden\": true} record in providers.json that keeps it removed; deleting the record brings it back)").is_none());
        assert!(parse_groups_line("").is_none());
    }

    #[test]
    fn parse_group_show_found_group() {
        let stdout = "  DeepSeek V4.1 Flash  auto/deepseek-v4.1-flash  found by Magpie — changing it makes it yours\n  routing   smart\n  stays     auto\n  models    1 opencode-zen/deepseek-v4.1-flash  OpenCode Zen · DeepSeek V4.1 Flash\n            2 workbuddy/deepseek-v4.1-flash  WorkBuddy · DeepSeek-V4.1-Flash\n            3 trae-cn/deepseek-v4.1-flash  Trae CN · DeepSeek-V4.1-Flash\n";
        let d = parse_group_show(stdout).expect("应解析出组详情");
        assert_eq!(d.name, "DeepSeek V4.1 Flash");
        assert_eq!(d.public_id, "auto/deepseek-v4.1-flash");
        assert_eq!(d.routing, "smart");
        assert_eq!(d.stays, "auto");
        assert!(d.found);
        assert!(!d.removed);
        assert_eq!(d.members.len(), 3);
        assert_eq!(d.members[0].id, "opencode-zen/deepseek-v4.1-flash");
        assert_eq!(d.members[0].provider, "OpenCode Zen");
        assert_eq!(d.members[0].description, "DeepSeek V4.1 Flash");
        assert!(!d.members[0].selected);
        assert_eq!(d.members[2].provider, "Trae CN");
    }

    #[test]
    fn parse_group_show_manual_note_and_selection() {
        let stdout = "  rsi-t2-probe  group/rsi-t2-probe\n  routing   manual  every request to workbuddy/minimax-m3.1-flash; Magpie group pick rsi-t2-probe <model> picks another\n  stays     auto\n  models    1 workbuddy/minimax-m3.1-flash ●  WorkBuddy · MiniMax-M3.1-Flash\n            2 opencode-zen/beam-free  OpenCode Zen · Beam Free\n";
        let d = parse_group_show(stdout).expect("应解析出组详情");
        assert_eq!(d.routing, "manual");
        assert!(d.routing_note.contains("every request to"));
        assert_eq!(d.members.len(), 2);
        assert_eq!(d.members[0].id, "workbuddy/minimax-m3.1-flash");
        assert!(d.members[0].selected);
        assert_eq!(d.members[0].provider, "WorkBuddy");
        assert_eq!(d.members[0].description, "MiniMax-M3.1-Flash");
        assert!(!d.members[1].selected);
        // 普通用户组：没有 found / removed 后缀 → owned
        assert!(!d.found);
        assert!(!d.removed);
    }

    #[test]
    fn parse_group_show_removed_found_group() {
        let stdout = "  GLM-5.3  auto/glm-5.3  found by Magpie — changing it makes it yours  removed · Magpie group restore auto-glm-5-3\n  routing   smart\n  stays     auto\n  models    1 workbuddy/glm-5.3  WorkBuddy · GLM-5.3\n";
        let d = parse_group_show(stdout).unwrap();
        assert!(d.removed);
        assert!(d.found);
        assert_eq!(d.public_id, "auto/glm-5.3");
        assert_eq!(
            parse_suffix_restore_id(&d.suffix).as_deref(),
            Some("auto-glm-5-3")
        );
    }

    #[test]
    fn parse_group_show_owned_group_without_suffix() {
        let owned = "  GLM-5.3  auto/glm-5.3\n  routing   smart\n  stays     auto\n  models    1 workbuddy/glm-5.3  WorkBuddy · GLM-5.3\n";
        let d = parse_group_show(owned).unwrap();
        assert!(!d.found);
        assert!(!d.removed);
        assert_eq!(d.public_id, "auto/glm-5.3");
        assert_eq!(d.stays, "auto");
    }

    #[test]
    fn parse_group_show_multi_segment_model_name() {
        let stdout = "  rsi-multi-probe  group/rsi-multi-probe\n  routing   smart\n  stays     auto\n  models    1 workbuddy/minimax-m3.1-flash  WorkBuddy · MiniMax-M3.1-Flash\n            2 kilo/reflection/beam-free  Kilo Gateway · Reflection: Beam 1.0 (free)\n";
        let d = parse_group_show(stdout).unwrap();
        assert_eq!(d.members.len(), 2);
        assert_eq!(d.members[1].id, "kilo/reflection/beam-free");
        assert_eq!(d.members[1].provider, "Kilo Gateway");
        assert_eq!(d.members[1].description, "Reflection: Beam 1.0 (free)");
    }

    #[test]
    fn parse_added_group_id_and_replaced() {
        let ok = "✓ added rsi-probe-plain · agents pick it as group/rsi-probe-plain\n  rsi-probe-plain  group/rsi-probe-plain\n  routing   smart\n";
        assert_eq!(parse_added_group_id(ok).as_deref(), Some("rsi-probe-plain"));
        assert!(!parse_add_replaced(ok));
        let replaced = "✓ replaced rsi-probe-dup2 · agents pick it as group/rsi-probe-dup2\n";
        assert_eq!(parse_added_group_id(replaced).as_deref(), Some("rsi-probe-dup2"));
        assert!(parse_add_replaced(replaced));
    }

    #[test]
    fn parse_removed_outcome_distinguishes_delete_and_hide() {
        let deleted = "✓ removed rsi-p2-probe-b\n";
        assert_eq!(parse_removed_outcome(deleted), (false, None));
        let hidden = "✓ removed GLM-5.3 · Magpie found it, so it's hidden: Magpie group restore auto-glm-5-3 brings it back\n";
        assert_eq!(
            parse_removed_outcome(hidden),
            (true, Some("auto-glm-5-3".to_string()))
        );
    }

    #[test]
    fn error_extraction_filters_plugin_noise() {
        let se = "2026/11/16 23:15:04 plugin [error]: @opencode-trae-auth@latest: provider.models: Trae CN's sign-in has expired; sign in again\nMagpie: no group \"x\" (groups: a, b)\n";
        assert_eq!(
            first_real_error(se).as_deref(),
            Some("Magpie: no group \"x\" (groups: a, b)")
        );
        assert!(first_real_error("2026/11/16 plugin [error]: Trae CN's sign-in has expired").is_none());
    }

    #[test]
    fn norm_name_folds_case_and_spaces() {
        assert_eq!(norm_name("  My   Group "), "my group");
        assert_eq!(norm_name("MY GROUP"), "my group");
    }

    #[test]
    fn nested_depth_matches_cli_boundary() {
        // 实测（.scratch/probe_p16.txt）：
        //   chain-1(含模型) … chain-9(含 group/chain-8) 都能建；
        //   chain-10(含 group/chain-9) 被拒「has groups in it 8 deep」。
        //   直接挂 group/chain-8 成功、挂 group/chain-9 失败。
        // ⇒ 最外层组往下最多再嵌 8 层组；目标组自己不计入。
        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        // c1 含普通模型；cN 含 group/c(N-1)
        edges.insert("c1".to_string(), Vec::new());
        for i in 2..=10 {
            edges.insert(format!("c{i}"), vec![format!("c{}", i - 1)]);
        }
        // 挂 c8 → 最长链 = c8..c1 = 8 → 允许
        assert_eq!(nested_depth(&["group/c8".to_string()], &edges), 8);
        assert!(nested_depth(&["group/c8".to_string()], &edges) <= MAX_NEST_DEPTH);
        // 挂 c9 → 最长链 = c9..c1 = 9 → 超限
        assert_eq!(nested_depth(&["group/c9".to_string()], &edges), 9);
        assert!(nested_depth(&["group/c9".to_string()], &edges) > MAX_NEST_DEPTH);
        // 普通模型不占深度（实测：混合成员与纯嵌套成员同样被拒）
        assert_eq!(
            nested_depth(&["workbuddy/glm-5.3".to_string(), "group/c9".to_string()], &edges),
            9
        );
        // 没有嵌套成员 → 深度 0
        assert_eq!(nested_depth(&["workbuddy/glm-5.3".to_string()], &edges), 0);
        // 超深**不是**成环
        assert!(!nested_cycle("new", &["group/c9".to_string()], &edges));
    }

    #[test]
    fn nested_depth_counts_shared_branches_separately() {
        // 两条分支共享同一个后代时，长度必须按最长的那条算，
        // 不能因为 visited 被复用而少算（老实现会少算）。
        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        edges.insert("leaf".to_string(), Vec::new());
        edges.insert("mid".to_string(), vec!["leaf".to_string()]);
        edges.insert("top".to_string(), vec!["mid".to_string(), "leaf".to_string()]);
        assert_eq!(nested_depth(&["group/top".to_string()], &edges), 3);
    }

    #[test]
    fn display_name_keeps_internal_spaces() {
        // 实测：CLI 允许 `Spaced  Out` 这种名字（并编号成 -2）。
        // 显示名里的连续空格必须原样保留，不能被规范化。
        let l = "  Spaced  Out               group/spaced-out            smart  opencode-zen/beam-free";
        let r = parse_groups_line(l).expect("应解析出组行");
        assert_eq!(r.name, "Spaced  Out");
        assert_eq!(r.public_id, "group/spaced-out");
        assert!(!r.found_marker);
    }

    #[test]
    fn found_marker_is_only_a_weak_hint() {
        // `found` 是列表列里的标记，和显示名之间没有可靠分隔。
        // 所以：**名字原样保留**，`found_marker` 只是弱提示，
        // 真正的 found 状态以 `group show` 首行的 `found by ...` 为准。
        let l = "  my found thing             group/my-found-thing        smart  opencode-zen/beam-free";
        let r = parse_groups_line(l).expect("应解析出组行");
        assert!(!r.found_marker);
        assert_eq!(r.name, "my found thing");
        // 列表里带 found 标记的组：名字保留尾部 `found`
        let l2 = "  Beam Free found            auto/beam-free            smart  opencode-zen/beam-free";
        let r2 = parse_groups_line(l2).expect("应解析出组行");
        assert!(r2.found_marker);
        assert_eq!(r2.name, "Beam Free found");
        // 真正的判据：group show 首行
        let found_show = "  Beam Free  auto/beam-free  found by Magpie — changing it makes it yours\n  routing   smart\n";
        assert!(parse_group_show(found_show).expect("应解析出详情").found);
        let own_show = "  my found thing  group/my-found-thing\n  routing   smart\n";
        assert!(!parse_group_show(own_show).expect("应解析出详情").found);
    }

    #[test]
    fn show_header_keeps_internal_spaces() {
        let d = parse_group_show("  Spaced  Out  group/spaced-out\n  routing   smart\n").expect("应解析出详情");
        assert_eq!(d.name, "Spaced  Out");
        assert_eq!(d.public_id, "group/spaced-out");
    }

    #[test]
    fn nested_cycle_detects_self_and_indirect() {
        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        edges.insert("a".to_string(), vec!["b".to_string()]);
        edges.insert("b".to_string(), vec!["c".to_string()]);
        assert!(nested_cycle("c", &["group/a".to_string()], &edges));
        assert!(nested_cycle("a", &["group/a".to_string()], &edges));
        assert!(!nested_cycle("a", &["group/b".to_string()], &edges));
    }

    #[test]
    fn percent_decode_handles_encoded_public_id() {
        assert_eq!(percent_decode("auto%2Fglm-5.3"), "auto/glm-5.3");
        assert_eq!(percent_decode("group%2Frsi-probe"), "group/rsi-probe");
    }

    #[test]
    fn fixture_groups_real_output_parses() {
        // 真实捕获的 `magpie groups` 输出（p18）。
        let (rows, hidden) = parse_groups_output(FIXTURE_GROUPS);
        assert_eq!(rows.len(), 11, "4 个用户组 + 7 个内置 auto 组");
        assert!(hidden.is_empty(), "没有 removed 组时 footer 为空：{hidden:?}");

        // 第 1 行：普通用户组
        assert_eq!(rows[0].name, "RSI Fixture Plain");
        assert_eq!(rows[0].public_id, "group/rsi-fixture-plain");
        assert_eq!(rows[0].routing, "smart");
        assert_eq!(rows[0].members, vec!["opencode-zen/beam-free", "workbuddy/glm-5.3"]);
        assert!(!rows[0].found_marker);

        // 第 2 行：**连续空格**必须原样保留
        assert_eq!(rows[1].name, "RSI  Double  Space");
        assert_eq!(rows[1].public_id, "group/rsi-double-space");

        // 第 3 行：嵌套组成员
        assert_eq!(rows[2].name, "RSI Fixture Nest");
        assert_eq!(rows[2].members, vec!["group/rsi-fixture-plain", "opencode-zen/flint-40b-free"]);

        // 第 4 行：manual routing，`●` 打在成员**前面**且该成员仍在列表里
        assert_eq!(rows[3].routing, "manual");
        assert_eq!(rows[3].selected.as_deref(), Some("opencode-zen/beam-free"));
        assert_eq!(rows[3].members, vec!["opencode-zen/beam-free", "workbuddy/glm-5.3"]);

        // 内置 auto 组：public id 是 `auto/...`，found 标记是**弱提示**
        let beam = &rows[4];
        assert_eq!(beam.name, "Beam Free found");
        assert_eq!(beam.public_id, "auto/beam-free");
        assert!(beam.found_marker, "名称尾部看起来像 found");
        assert_eq!(
            internal_id_of(&beam.public_id),
            "auto-beam-free"
        );
    }

    #[test]
    fn fixture_groups_removed_footer_real_output() {
        // 真实捕获的 removed footer（p18）：删掉内置 found 组后的 `magpie groups`。
        let (rows, hidden) = parse_groups_output(FIXTURE_GROUPS_REMOVED);
        assert_eq!(rows.len(), 10, "Beam Free 被移出可见列表");
        assert_eq!(hidden, vec!["auto-beam-free".to_string()]);
        // 可见列表里不该再有 auto/beam-free
        assert!(!rows.iter().any(|r| r.public_id == "auto/beam-free"));
    }

    #[test]
    fn fixture_show_real_output_parses() {
        // 普通组
        let d = parse_group_show(FIXTURE_SHOW_PLAIN).expect("应解析出组");
        assert_eq!(d.name, "RSI Fixture Plain");
        assert_eq!(d.public_id, "group/rsi-fixture-plain");
        assert_eq!(d.routing, "smart");
        assert_eq!(d.stays, "auto");
        assert_eq!(d.members.len(), 2);
        assert_eq!(d.members[0].id, "opencode-zen/beam-free");
        assert_eq!(d.members[0].provider, "OpenCode Zen");
        assert_eq!(d.members[1].id, "workbuddy/glm-5.3");
        assert!(!d.found);
        assert!(!d.removed);

        // 连续空格的显示名
        let d2 = parse_group_show(FIXTURE_SHOW_DOUBLE_SPACE).expect("应解析出组");
        assert_eq!(d2.name, "RSI  Double  Space");
        assert_eq!(d2.public_id, "group/rsi-double-space");
        assert_eq!(d2.members.len(), 1);

        // 嵌套组
        let d3 = parse_group_show(FIXTURE_SHOW_NEST).expect("应解析出组");
        assert_eq!(d3.name, "RSI Fixture Nest");
        assert_eq!(d3.members.len(), 2);
        assert_eq!(d3.members[0].id, "group/rsi-fixture-plain");
        // 嵌套组的描述形态是 `routing group <组名> · <routing> · <成员列表>`，
        // 按**第一个** ` · ` 切，所以 provider 段是 `routing group RSI Fixture Plain`。
        assert_eq!(d3.members[0].provider, "routing group RSI Fixture Plain");
        assert_eq!(
            d3.members[0].description,
            "smart · opencode-zen/beam-free, workbuddy/glm-5.3"
        );
        assert_eq!(d3.members[1].id, "opencode-zen/flint-40b-free");

        // manual routing：首行 routing 后面跟着长说明，成员里 `●` 标在**后面**
        let d4 = parse_group_show(FIXTURE_SHOW_MANUAL).expect("应解析出组");
        assert_eq!(d4.routing, "manual");
        assert!(d4.routing_note.contains("every request to"), "routing_note={}", d4.routing_note);
        assert_eq!(d4.members.len(), 2);
        assert!(d4.members[0].selected, "第一个成员是 manual 当前选中的");
        assert!(!d4.members[1].selected);
    }


    /// 造一个 GroupView（供冲突预检等纯逻辑测试用）。
    fn mk_view(
        internal_id: &str,
        public_id: &str,
        found: bool,
        removed: bool,
        visible: bool,
    ) -> GroupView {
        GroupView {
            internal_id: internal_id.to_string(),
            visible,
            restore_id: if removed {
                Some(internal_id.to_string())
            } else {
                None
            },
            detail: GroupDetail {
                name: format!("Name {internal_id}"),
                public_id: public_id.to_string(),
                // removed 组的权威 restore id 来自 `group show` 首行后缀，这里按实测格式造出来
                suffix: if removed {
                    format!(
                        "found by Magpie — changing it makes it yours  removed · Magpie group restore {internal_id}"
                    )
                } else {
                    String::new()
                },
                routing: "smart".to_string(),
                routing_note: String::new(),
                stays: "auto".to_string(),
                members: Vec::new(),
                found,
                removed,
            },
        }
    }

    #[test]
    fn view_from_show_uses_query_ref_as_internal_id() {
        // 内部 id 的权威来源是**查询用的 ref**，不是回读的 public id：
        // public 尾段 `Doubao_1_6` 的下划线/大小写无法从 internal slug 无损还原，
        // 所以两者各存各的，谁也不从谁推导。
        let stdout = "  Doubao 1.6  auto/Doubao_1_6\n  routing   smart\n  stays     auto\n  models    1 trae-cn/Doubao_1_6  Trae CN · Doubao 1.6\n";
        let v = view_from_show("group/auto-doubao-1-6", stdout).expect("应折出 GroupView");
        assert_eq!(v.internal_id, "auto-doubao-1-6");
        assert_eq!(v.detail.public_id, "auto/Doubao_1_6");
        assert!(v.visible, "未移除的组可见");
        assert_eq!(v.restore_id, None, "未移除的组没有 restore id");

        // ref 不带 `group/` 前缀时原样当内部 id
        let v2 = view_from_show("auto-doubao-1-6", stdout).expect("应折出 GroupView");
        assert_eq!(v2.internal_id, "auto-doubao-1-6");
    }

    #[test]
    fn view_from_show_takes_authoritative_restore_id_from_suffix() {
        // removed 组：restore id 只认 `group show` 后缀里的值；同时不可见。
        let stdout = "  GLM-5.3  auto/glm-5.3  found by Magpie — changing it makes it yours  removed · Magpie group restore auto-glm-5-3\n  routing   smart\n  stays     auto\n  models    1 workbuddy/glm-5.3  WorkBuddy · GLM-5.3\n";
        let v = view_from_show("group/auto-glm-5-3", stdout).expect("应折出 GroupView");
        assert_eq!(v.internal_id, "auto-glm-5-3");
        assert_eq!(v.restore_id.as_deref(), Some("auto-glm-5-3"));
        assert!(!v.visible, "removed 组必须不可见");
        assert!(v.detail.removed && v.detail.found);

        // 解析不出来（垃圾 stdout）→ None，调用方跳过该组
        assert!(view_from_show("group/x", "not a group show output\n").is_none());
    }

    #[test]
    fn pick_restore_arg_prefers_authoritative_id() {
        let groups = vec![
            // 已移除的内置组：internal id 与 restore id 同形
            mk_view("auto-beam-free", "auto/beam-free", true, true, false),
            // 普通用户组：没有 restore id
            mk_view("rsi-fixture-plain", "group/rsi-fixture-plain", false, false, true),
        ];
        // 三种调用形态都能命中同一个组，并返回它的权威 restore id
        assert_eq!(
            pick_restore_arg(&groups, "auto/beam-free", "auto-beam-free"),
            "auto-beam-free"
        );
        assert_eq!(
            pick_restore_arg(&groups, "auto-beam-free", "auto-beam-free"),
            "auto-beam-free"
        );
        // 用户自建组：命中但没有 restore id → 退回 internal id，让 CLI 报真实原因
        assert_eq!(
            pick_restore_arg(&groups, "group/rsi-fixture-plain", "rsi-fixture-plain"),
            "rsi-fixture-plain"
        );
        // 完全没命中 → 退回 internal id
        assert_eq!(pick_restore_arg(&groups, "auto/nope", "auto-nope"), "auto-nope");
        // 空状态也不能 panic
        assert_eq!(pick_restore_arg(&[], "auto/beam-free", "auto-beam-free"), "auto-beam-free");
    }

    #[test]
    fn auto_id_conflict_checks_all_groups_not_only_visible() {
        // 实测（p5/p6）：hidden 的 found 组**仍然占着**它的 internal id。
        // 新建同名组抢不到它（CLI 编号成 `-2`，结果还是普通 `group/*`），
        // 所以预检必须把 removed/hidden 组算进去，不能只看可见组。
        let groups = vec![
            // 可见的普通用户组
            mk_view("rsi-fixture-plain", "group/rsi-fixture-plain", false, false, true),
            // 可见的内置 found 组
            mk_view("auto-glm-5-3", "auto/glm-5.3", true, false, true),
            // **不可见**的 hidden found 组（removed 记录）
            mk_view("auto-beam-free", "auto/beam-free", true, true, false),
        ];

        // 没被占用 → 放行
        assert!(check_auto_id_conflict(&groups, "auto-minimax-m3-1-flash", "minimax-m3.1-flash").is_ok());

        // 普通用户组占用
        let e = check_auto_id_conflict(&groups, "rsi-fixture-plain", "x").expect_err("同 id 应冲突");
        assert!(e.contains("已被现有组占用"), "{e}");
        assert!(e.contains("group/rsi-fixture-plain"), "{e}");

        // 可见的内置 found 组
        let e = check_auto_id_conflict(&groups, "auto-glm-5-3", "glm-5.3").expect_err("应冲突");
        assert!(e.contains("内置发现的组"), "{e}");
        assert!(e.contains("auto/glm-5.3"), "{e}");

        // **hidden（removed）** 的 found 组也必须拦住——这是本次修的核心
        let e = check_auto_id_conflict(&groups, "auto-beam-free", "beam-free").expect_err("应冲突");
        assert!(e.contains("已移除"), "{e}");
        assert!(e.contains("auto/beam-free"), "{e}");
        // 文案要给出可操作的下一步
        assert!(e.contains("group restore"), "{e}");
    }
}
