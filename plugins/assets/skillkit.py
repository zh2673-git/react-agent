"""技能契约的纯函数实现（无内核依赖）。

单一事实来源：assets guest 的 `skills.validate` / `skills.state` 与 rsi-agent 的
`scripts/skill_validate.py` / `skill_state.py` 都调用本模块，避免两套真相。

设计原则：
- 只读技能目录，不改内容；错误定位到 `code/path/message`；
- `mode="install"` 只做结构级拦截（坏技能进不来，存量技能不被误伤）；
- `mode="full"` 追加验收段/案例/契约/工具声明等质量门（candidate/active 必过）。
"""

from __future__ import annotations

import json
import re
from pathlib import Path

ALLOWED_STATUS = ("draft", "candidate", "active", "retired")
ALLOWED_SIGNALS = ("assert", "judge", "human", "test")
NAME_RE = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*$")
# 引用 token：允许中文/Unicode 文件名（技能 references 常带中文篇名）；
# 遇空白、引号、括号、反引号、尖括号即止，避免把整段话吞进来。
REF_RE = re.compile(r"references/[^\s`\"'<>()\[\]（）。，；：、《》「」]+")
# 验收段：标题行含关键词，或以关键词开头的行；中英文都认。
ACC_RE = re.compile(
    r"(?:^|\n)(?:#{1,6}\s*[^\n]*?(?:验收|成功标准|acceptance|success criteria)"
    r"|(?:验收|成功标准|acceptance|success criteria))",
    re.IGNORECASE,
)
# 状态迁移表（只允许这些边；晋升/回退/退役不走跳步）
TRANSITIONS = {
    "draft": ("candidate",),
    "candidate": ("active", "draft"),
    "active": ("retired", "candidate"),
    "retired": ("draft",),
}
DEFAULT_MAX_SKILL_LINES = 120


def parse_frontmatter(text: str) -> dict | None:
    """最小 frontmatter：`---` 围栏内的 `key: value` 行。缺 name/description → None。"""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return None
    meta: dict = {}
    for line in lines[1:]:
        if line.strip() == "---":
            break
        if ":" in line and not line.startswith((" ", "\t")):
            key, _, value = line.partition(":")
            meta[key.strip()] = value.strip()
    if not meta.get("name") or not meta.get("description"):
        return None
    return meta


def read_json(path: Path) -> dict | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def read_state(skill_dir: Path, meta: dict | None = None) -> dict:
    """状态读取优先级：state.json > skill.json > frontmatter > draft。"""
    meta = meta or {}
    contract = read_json(skill_dir / "skill.json") or {}
    state = read_json(skill_dir / "state.json") or {}
    status = state.get("status") or contract.get("status") or meta.get("status") or "draft"
    verification = state.get("verification") or contract.get("verification") or "unverified"
    legacy = bool(contract.get("legacy")) or (skill_dir / "evals" / "cases.jsonl").is_file() is False
    return {
        "status": status,
        "verification": verification,
        "legacy": legacy,
        "contract": contract,
        "state": state,
    }


def allowed_transition(src: str, dst: str) -> tuple[bool, str]:
    if src not in ALLOWED_STATUS or dst not in ALLOWED_STATUS:
        return False, f"非法状态: {src} -> {dst}（合法值: {', '.join(ALLOWED_STATUS)}）"
    if dst in TRANSITIONS.get(src, ()):
        return True, ""
    return False, f"不允许跳步: {src} -> {dst}"


def _case_errors(path: Path, required: bool) -> list[dict]:
    if not path.is_file():
        if required:
            return [{"code": "E_CASES_MISSING", "path": str(path), "message": "candidate/active 技能至少需要 3 条案例（evals/cases.jsonl）"}]
        return [{"code": "W_NO_EVALS", "path": str(path), "message": "未提供 evals/cases.jsonl，最多停留在 draft"}]
    cases = []
    for i, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = line.strip()
        if not line:
            continue
        try:
            case = json.loads(line)
        except ValueError:
            return [{"code": "E_CASE_JSON", "path": f"{path}#L{i}", "message": "案例不是合法 JSON"}]
        missing = [k for k in ("id", "input", "expect", "rubric", "signal", "source") if not case.get(k)]
        if missing:
            return [{"code": "E_CASE_FIELDS", "path": f"{path}#L{i}", "message": f"案例缺少字段: {', '.join(missing)}"}]
        if case.get("signal") not in ALLOWED_SIGNALS:
            return [{"code": "E_CASE_SIGNAL", "path": f"{path}#L{i}", "message": f"signal 必须属于 {ALLOWED_SIGNALS}"}]
        cases.append(case)
    if len(cases) < 3:
        return [{"code": "E_CASES_LESS_THAN_3", "path": str(path), "message": f"案例数 {len(cases)} < 3"}]
    return []


def _tools_errors(skill_dir: Path, meta: dict) -> list[dict]:
    decl = (meta.get("tools") or "").strip()
    path = skill_dir / (decl if decl else "tools.json")
    if not path.is_file():
        return []  # 声明缺失由 assets 侧忽略；这里只校验已存在的声明文件
    try:
        items = json.loads(path.read_text(encoding="utf-8"))
    except ValueError:
        return [{"code": "E_TOOLS_JSON", "path": str(path), "message": "tools.json 不是合法 JSON"}]
    if not isinstance(items, list) or not items:
        return [{"code": "E_TOOLS_SHAPE", "path": str(path), "message": "tools.json 必须是非空数组"}]
    errors = []
    for idx, item in enumerate(items):
        where = f"{path}#item{idx}"
        if not isinstance(item, dict):
            errors.append({"code": "E_TOOLS_ITEM", "path": where, "message": "工具项必须是对象"})
            continue
        for key in ("name", "description", "parameters", "exec"):
            if key not in item:
                errors.append({"code": "E_TOOLS_FIELD", "path": where, "message": f"缺少字段 {key}"})
        exec_spec = item.get("exec") if isinstance(item.get("exec"), dict) else {}
        cmd = exec_spec.get("cmd")
        if not isinstance(cmd, list) or not cmd:
            errors.append({"code": "E_TOOLS_EXEC", "path": where, "message": "exec.cmd 必须是非空数组"})
            continue
        for token in cmd:
            if not isinstance(token, str) or not token:
                errors.append({"code": "E_TOOLS_EXEC", "path": where, "message": "exec.cmd 元素必须是非空字符串"})
                continue
            # 带路径分隔符的 token 视为执行体文件，做存在性检查；其余多为解释器/子命令
            if "/" in token or "\\" in token:
                target = Path(token) if Path(token).is_absolute() else skill_dir / token
                if not target.exists():
                    errors.append({"code": "E_TOOLS_EXEC_MISSING", "path": where, "message": f"执行体不存在: {token}"})
    return errors


def validate_skill(skill_dir: str | Path, mode: str = "full") -> dict:
    """校验一个技能目录；返回 `{ok, skill, mode, status, errors, warnings}`。"""
    skill_dir = Path(skill_dir)
    errors: list[dict] = []
    warnings: list[dict] = []
    skill_md = skill_dir / "SKILL.md"
    if not skill_md.is_file():
        return {
            "ok": False,
            "skill": skill_dir.name,
            "mode": mode,
            "errors": [{"code": "E_SKILL_MD_MISSING", "path": str(skill_md), "message": "缺少 SKILL.md"}],
            "warnings": [],
        }
    try:
        text = skill_md.read_text(encoding="utf-8")[:65536]
    except OSError as exc:
        return {
            "ok": False,
            "skill": skill_dir.name,
            "mode": mode,
            "errors": [{"code": "E_SKILL_READ", "path": str(skill_md), "message": f"读取失败: {exc}"}],
            "warnings": [],
        }
    meta = parse_frontmatter(text)
    if meta is None:
        return {
            "ok": False,
            "skill": skill_dir.name,
            "mode": mode,
            "errors": [{"code": "E_FRONTMATTER", "path": str(skill_md), "message": "frontmatter 缺 name/description"}],
            "warnings": [],
        }
    name = meta["name"]
    state = read_state(skill_dir, meta)
    status = state["status"]

    if not NAME_RE.match(name):
        errors.append({"code": "E_NAME_NOT_KEBAB", "path": str(skill_md), "message": f"技能名不是 kebab-case: {name}"})
    if skill_dir.name != name:
        errors.append({"code": "E_DIR_NAME_MISMATCH", "path": str(skill_md), "message": f"目录名 {skill_dir.name} 与 name {name} 不一致"})
    desc = meta.get("description", "").strip()
    if len(desc) < 8:
        warnings.append({"code": "W_DESCRIPTION_SHORT", "path": str(skill_md), "message": "description 过短，难以被正确触发"})
    if len(desc) > 200:
        warnings.append({"code": "W_DESCRIPTION_LONG", "path": str(skill_md), "message": "description 超过 200 字符，消耗发现预算"})

    # 引用完整性：SKILL.md 提到的 references/* 必须存在；install 模式降为告警
    # （存量技能存在跨技能引用，安装门只拦「结构不可用」；full 模式才是质量门）。
    refs = {m.group(0).rstrip(".,;:。，；：、") for m in REF_RE.finditer(text)}
    for ref in sorted(refs):
        # 「references/01」这类篇号简写不是文件路径（正文引用习惯），不做存在性校验
        tail = ref.split("references/", 1)[1].strip()
        if tail.isdigit():
            continue
        if not (skill_dir / ref).is_file():
            target = errors if mode == "full" else warnings
            target.append({"code": "E_REF_MISSING", "path": str(skill_md), "message": f"引用的 {ref} 不存在"})
    ref_dir = skill_dir / "references"
    if ref_dir.is_dir():
        for f in sorted(ref_dir.rglob("*")):
            if f.is_file() and f.relative_to(skill_dir).as_posix() not in refs:
                warnings.append({"code": "W_ORPHAN_REF", "path": f.relative_to(skill_dir).as_posix(), "message": "未被 SKILL.md 引用"})

    # 验收段：full 模式为 error；install 模式只读结构，不因质量段缺失拒绝存量技能
    if not ACC_RE.search(text):
        target = errors if mode == "full" else warnings
        target.append({"code": "E_ACCEPTANCE_MISSING", "path": str(skill_md), "message": "缺少验收/成功标准段落"})

    if status not in ALLOWED_STATUS:
        errors.append({"code": "E_STATUS_INVALID", "path": str(skill_md), "message": f"状态 {status} 非法（合法值: {', '.join(ALLOWED_STATUS)}）"})

    # 案例与契约：candidate/active 强制
    need_cases = status in ("candidate", "active")
    if mode == "full" or need_cases:
        case_errors = _case_errors(skill_dir / "evals" / "cases.jsonl", required=need_cases)
        for e in case_errors:
            (errors if e["code"].startswith("E_") and need_cases else warnings).append(e)
        if need_cases and not (skill_dir / "skill.json").is_file():
            errors.append({"code": "E_CONTRACT_MISSING", "path": str(skill_dir / "skill.json"), "message": "candidate/active 需要 skill.json 契约"})

    # 工具声明（生成代码工具的契约闸）：JSON 形状 + 执行体存在
    tool_errors = _tools_errors(skill_dir, meta)
    if status in ("candidate", "active"):
        errors.extend(tool_errors)
    else:
        warnings.extend(tool_errors)

    lines = text.count("\n") + 1
    if lines > DEFAULT_MAX_SKILL_LINES:
        warnings.append({"code": "W_SKILL_TOO_LONG", "path": str(skill_md), "message": f"SKILL.md {lines} 行，超过预算 {DEFAULT_MAX_SKILL_LINES}"})
    if state["verification"] not in ("verified", "partial"):
        warnings.append({"code": "W_UNVERIFIED", "path": str(skill_md), "message": "技能未经验证（verification=unverified/legacy）"})

    return {
        "ok": not errors,
        "skill": name,
        "mode": mode,
        "status": status,
        "verification": state["verification"],
        "errors": errors,
        "warnings": warnings,
    }


def set_state(skill_dir: str | Path, dst: str) -> dict:
    """状态迁移（只允许 TRANSITIONS 中的边），写入 state.json。"""
    skill_dir = Path(skill_dir)
    skill_md = skill_dir / "SKILL.md"
    meta = parse_frontmatter(skill_md.read_text(encoding="utf-8")) if skill_md.is_file() else None
    state = read_state(skill_dir, meta)
    src = state["status"]
    ok, reason = allowed_transition(src, dst)
    if not ok:
        return {"ok": False, "skill": skill_dir.name, "from": src, "to": dst, "error": {"code": "E_TRANSITION", "message": reason}}
    payload = dict(state["state"])
    payload.update({"skill": skill_dir.name, "status": dst, "updated_at": _now()})
    (skill_dir / "state.json").write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return {"ok": True, "skill": skill_dir.name, "from": src, "to": dst, "state": payload}


def _now() -> str:
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).astimezone().isoformat(timespec="seconds")
