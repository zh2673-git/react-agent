"""4 个文件工具 + realpath 越界校验（唯一拦截点：拦截点=操作点，空间防越界）。

v3 增强（2026-09-06，PLAN §6）：编码探测（BOM/UTF-8/GBK 回退，结果标注）；二进制/图片
识别（不再出乱码）；超长行截断；write_file 原子写 + .bak 备份 + 变更统计；edit_file
「行尾空白 + 换行符」容错匹配 + 失败候选线索 + unified diff 回显 + 按探测编码回写；
list_dir 递归 + mtime + 排序 + 条目上限 + 噪音剪枝。写统一 newline=""（不翻译换行，
保持文件原有 LF/CRLF 风格——修复旧版 edit/write 把 LF 文件改写成 CRLF 的隐性问题）。

v6 核心写保护（2026-09-08，自扩展安全边界）：write_file/edit_file 追加「核心路径黑名单」
——agent 自扩展只能走四条固定途径（SKILL.md / 技能 tools.json / plugins/tools/*.py 新
文件 / config.json 的 mcp_servers），对 agent 自身运行体（crates 源树、memory/llm_adapter
插件、内置工具实现、config.json、.git）的写入默认拒绝（fail-closed）。技能根目录（自扩展
主通道）显式豁免。逃生舱：env ALLOW_CORE_WRITE=1（config.json tools.allow_core_write 持久
通道映射，改后重启 host 生效）。诚实边界：bash 间接写不经此闸（靠 SYSTEM.md 纪律 + bash
沙箱兜底）。
"""

import difflib
import fnmatch
import os
import re
import time
import uuid
from pathlib import Path

from . import ToolError, optional_int, require, workspace_root

_MAX_READ_BYTES = 256 * 1024
_MAX_LINE_CHARS = 2000
_DIFF_MAX_CHARS = 4000
_MAX_LIST_ENTRIES = 5000

# 递归遍历（list_dir recursive / grep）默认剪枝的噪音目录
_NOISE_DIRS = frozenset(
    {".git", "node_modules", "target", "__pycache__", ".venv", "venv", "dist", "build", ".idea"}
)

# ── 核心写保护（v6）：禁止 write_file/edit_file 触碰的路径（相对 WORKSPACE_ROOT，normcase
# 前缀匹配）。原则：agent 的运行体（宿主源码、内核插件、工具实现、配置）不可自改；自扩展
# 走白名单途径。skills root 单独豁免（SKILLS_DIR 或 plugins/assets/skills）。 ──
_CORE_BLOCKLIST = (
    "crates",                       # Rust 源树（host/agent-loop + web-dist 前端）
    "plugins/memory",               # 内核插件：trace 记忆
    "plugins/llm_adapter",          # 内核插件：模型适配
    "plugins/assets",               # 内核插件：SYSTEM.md/出厂技能（skills root 内豁免）
    "plugins/tools/tools",          # 内置 9 件工具实现包
    "plugins/tools/tools_plugin.py",  # 工具插件分派层（装载/启用闸所在）
    "config.json",                  # 密钥与运行配置（防自改授权/密钥）
    ".git",                         # 版本库（防篡改历史/回退能力）
)
# 说明：plugins/tools 顶层不在黑名单——L2 自扩展通道（放新 .py 工具模块），
# 仅内置模块名（files/bash/web/grep/symbols/mcp/__init__）在 _is_core_path 内单独保护。


def _core_write_allowed() -> bool:
    return os.environ.get("ALLOW_CORE_WRITE", "").strip() == "1"


def _skills_root_real() -> Path:
    env = os.environ.get("SKILLS_DIR")
    base = Path(env) if env else Path(workspace_root()) / "plugins" / "assets" / "skills"
    return Path(os.path.realpath(base))


def _is_core_path(real: Path) -> bool:
    """real 是否落在核心黑名单内（skills root 显式豁免——自扩展主通道不可断）。"""
    ws_real = Path(os.path.realpath(workspace_root()))
    try:
        rel = real.relative_to(ws_real)
    except ValueError:
        return False  # 工作区外的软链/重定向：越界闸已拦，此处不再判
    parts = rel.parts
    sk = _skills_root_real()
    if str(real).startswith(str(sk) + os.sep) or real == sk:
        return False  # 技能目录：SKILL.md / tools.json / 技能工具执行体的合法落点
    rel_low = os.path.normcase(str(rel))
    for entry in _CORE_BLOCKLIST:
        e = os.path.normcase(entry)
        if rel_low == e or rel_low.startswith(e + os.sep):
            return True
    # plugins/tools 顶层：只允许新建/改非内置模块 .py（L2 自扩展），内置名保护
    if len(parts) >= 3 and parts[0] == "plugins" and parts[1] == "tools":
        name = parts[2]
        if name in {"files", "bash", "web", "grep", "symbols", "mcp", "__init__", "__pycache__"}:
            return True
    return False


def _guard_write(real: Path, path_str: str) -> Path:
    """写入闸（write_file/edit_file 专用）：核心黑名单默认拒绝，ALLOW_CORE_WRITE=1 放行。"""
    if _core_write_allowed():
        return real
    if _is_core_path(real):
        raise ToolError(
            f"核心路径写入被拒绝: '{_display(real)}'——这是 agent 自身的运行体（宿主源码/内核"
            "插件/内置工具/配置），自扩展只能走固定途径（skills 目录写 SKILL.md、技能目录放 "
            "tools.json、plugins/tools/ 顶层放新工具 .py、config.json 声明 mcp_servers）。"
            "确需修改核心请直接向用户说明，由用户自行修改或显式开启 ALLOW_CORE_WRITE=1。",
            code="CORE_PROTECTED",
            field="path",
        )
    return real


def _guard(path_str: str) -> Path:
    """realpath 前缀校验：相对路径锚定 WORKSPACE_ROOT；越界一律拒绝。"""
    p = Path(path_str)
    if not p.is_absolute():
        p = workspace_root() / p
    real = Path(os.path.realpath(p))
    ws_real = Path(os.path.realpath(workspace_root()))
    a, b = os.path.normcase(str(real)), os.path.normcase(str(ws_real))
    if not (a == b or a.startswith(b + os.path.sep)):
        raise ToolError(
            f"路径越界: '{path_str}' 解析为 '{real}'，不在工作区内。合法根: {ws_real}"
            "（相对路径以工作区为基准；如需其他目录请调整 WORKSPACE_ROOT）",
            code="PATH_OUTSIDE_WORKSPACE",
            field="path",
        )
    return real


def _display(p: Path) -> str:
    try:
        return str(p.relative_to(workspace_root()))
    except ValueError:
        return str(p)


def _decode(raw: bytes) -> tuple[str, str]:
    """编码探测：BOM → UTF-8 严格 → GBK 严格 → UTF-8 replace 兜底。返回 (text, 编码标注)。"""
    if raw.startswith(b"\xef\xbb\xbf"):
        return raw.decode("utf-8-sig", errors="replace"), "utf-8-sig"
    if raw.startswith((b"\xff\xfe", b"\xfe\xff")):
        return raw.decode("utf-16", errors="replace"), "utf-16"
    try:
        return raw.decode("utf-8"), "utf-8"
    except UnicodeDecodeError:
        pass
    try:
        return raw.decode("gbk"), "gbk"
    except UnicodeDecodeError:
        return raw.decode("utf-8", errors="replace"), "utf-8(replace)"


def _sniff_binary(raw: bytes) -> str | None:
    """二进制/图片识别（magic + NUL 探测）。返回描述，None = 文本。"""
    head = raw[:8192]
    if head.startswith(b"\x89PNG"):
        return "图片(png)"
    if head.startswith(b"\xff\xd8\xff"):
        return "图片(jpeg)"
    if head.startswith((b"GIF87a", b"GIF89a")):
        return "图片(gif)"
    if head.startswith(b"RIFF") and head[8:12] == b"WEBP":
        return "图片(webp)"
    if head.startswith(b"BM") and b"\x00" in head:
        return "图片(bmp)"
    if b"\x00" in head:
        return "二进制"
    return None


def _read_text(real: Path) -> tuple[str, bool, str]:
    raw = real.read_bytes()
    truncated_bytes = len(raw) > _MAX_READ_BYTES
    if truncated_bytes:
        raw = raw[:_MAX_READ_BYTES]
    text, enc = _decode(raw)
    return text, truncated_bytes, enc


def _write_newline_safe(real: Path, text: str, encoding: str = "utf-8") -> None:
    """newline="" 写入：不翻译换行（默认平台翻译会把 LF 文件改成 CRLF）。"""
    with real.open("w", encoding=encoding, newline="") as f:
        f.write(text)


def _read_file(args: dict) -> str:
    real = _guard(require(args, "path"))
    if real.is_dir():
        raise ToolError(f"'{real}' 是目录；读目录请用 list_dir", code="IS_DIR", field="path")
    if not real.exists():
        raise ToolError(f"文件不存在: '{_display(real)}'", code="NOT_FOUND", field="path")
    raw = real.read_bytes()
    kind = _sniff_binary(raw)
    if kind is not None:
        if kind.startswith("图片"):
            return (
                f"[{kind}文件: {_display(real)}, {len(raw) // 1024}KB，文本工具不显示图像内容；"
                "如需视觉理解，请把它作为聊天附件上传（多模态模型）]"
            )
        return f"[二进制文件: {_display(real)}, {len(raw) // 1024}KB，内容不显示；可用 bash 处理]"
    text, cut, enc = _read_text(real)
    lines = text.splitlines()
    if not lines:
        return f"{_display(real)}: (空文件)"
    offset = optional_int(args, "offset", 0, 0, 10**9)
    limit = optional_int(args, "limit", 2000, 1, 10_000)
    window: list[str] = []
    clipped = 0
    for line in lines[offset : offset + limit]:
        if len(line) > _MAX_LINE_CHARS:
            window.append(line[:_MAX_LINE_CHARS] + f"…[本行超长已截断，全长 {len(line)} 字符]")
            clipped += 1
        else:
            window.append(line)
    numbered = "\n".join(f"{offset + i + 1:6d}\t{line}" for i, line in enumerate(window))
    notes = []
    if enc != "utf-8":
        notes.append(f"编码: {enc}")
    if cut:
        notes.append(f"注意: 文件超过 {_MAX_READ_BYTES // 1024}KB，仅读取前 {_MAX_READ_BYTES // 1024}KB")
    if offset + limit < len(lines):
        notes.append(f"截断: 显示第 {offset + 1}-{offset + len(window)} 行（共 {len(lines)} 行）；继续读请用 offset={offset + limit}")
    if clipped:
        notes.append(f"{clipped} 行含超长截断")
    return numbered + ("\n\n[" + "；".join(notes) + "]" if notes else "")


def _write_file(args: dict) -> dict:
    real = _guard_write(_guard(require(args, "path")), require(args, "path"))
    content = args.get("content")
    if not isinstance(content, str):
        raise ToolError("缺少必填参数 'content'（string，全量覆盖写入）", code="MISSING_ARG", field="content")
    real.parent.mkdir(parents=True, exist_ok=True)
    existed = real.exists()
    result: dict = {
        "path": _display(real),
        "bytes": len(content.encode("utf-8")),
        "action": "overwritten" if existed else "written",
    }
    old_text: str | None = None
    before_raw: bytes | None = None
    if existed:
        old_raw = real.read_bytes()
        before_raw = old_raw
        if _sniff_binary(old_raw) is None:
            old_text, _, _ = _read_text(real)
        if bool(args.get("backup", True)):  # 覆盖前备份（.bak 只留最近一份）
            bak = real.with_name(real.name + ".bak")
            bak.write_bytes(old_raw)
            result["backup"] = _display(bak)
    if old_text is not None:
        added = removed = 0
        for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(
            a=old_text.splitlines(), b=content.splitlines(), autojunk=False
        ).get_opcodes():
            if tag in ("delete", "replace"):
                removed += i2 - i1
            if tag in ("insert", "replace"):
                added += j2 - j1
        result["changes"] = {"added_lines": added, "removed_lines": removed}
    tmp = real.with_name(real.name + f".tmp{os.getpid()}")  # 同目录临时文件 + 原子替换
    try:
        _write_newline_safe(tmp, content)
        os.replace(tmp, real)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise
    if (snap := _snapshot_change(before_raw, real.read_bytes())) is not None:
        result["undo"] = snap
    return result


def _fuzzy_re(old: str) -> re.Pattern:
    """容错匹配正则：各行为精确文本 + 行尾空白容忍，行间 CRLF/LF 均可（缩进仍须一致）。"""
    lines = old.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    return re.compile(r"\r?\n".join(re.escape(l) + r"[ \t]*" for l in lines))


def _line_candidates(text: str, needle: str, limit: int = 3) -> list[str]:
    """old 首个非空行在文件中的近似位置（0 命中时给线索）：先全行找，再退化为前 12 字符探针。"""
    first = next((l.strip() for l in needle.splitlines() if l.strip()), "")
    if not first:
        return []
    for probe in (first, first[:12]):
        if not probe:
            continue
        out = []
        for i, line in enumerate(text.splitlines()):
            if probe in line:
                out.append(f"第 {i + 1} 行: {line.strip()[:120]}")
                if len(out) >= limit:
                    return out
        if out:
            return out
    return []


def _edit_file(args: dict) -> dict:
    real = _guard_write(_guard(require(args, "path")), require(args, "path"))
    old = require(args, "old_string")
    new = require(args, "new_string")
    replace_all = bool(args.get("replace_all", False))
    if old == new:
        raise ToolError("old_string 与 new_string 相同，无需编辑", code="NO_OP", field="new_string")
    if not real.exists():
        raise ToolError(f"文件不存在: '{_display(real)}'（新建文件请用 write_file）", code="NOT_FOUND", field="path")
    text, _, enc = _read_text(real)
    write_enc = "utf-8" if enc == "utf-8(replace)" else enc
    mode = "exact"
    count = text.count(old)
    if count == 0:  # 降级：行尾空白 / CRLF-LF 容错（缩进不容错，避免重排风险）
        fuzzy = _fuzzy_re(old)
        count = len(fuzzy.findall(text))
        if count:
            mode = "fuzzy"
    if count == 0:
        cands = _line_candidates(text, old)
        hint = ("；文件中近似位置: " + " | ".join(cands)) if cands else "；文件中无相似行（确认目标文件是否正确）"
        raise ToolError(
            f"old_string 在文件中命中 0 处（已含行尾空白/换行符容错）；请先用 read_file 核对精确文本{hint}",
            code="EDIT_NO_MATCH",
            field="old_string",
        )
    if count > 1 and not replace_all:
        raise ToolError(
            f"old_string 命中 {count} 处；请提供更长的上下文使其唯一，或传 replace_all=true 全部替换",
            code="EDIT_AMBIGUOUS",
            field="old_string",
        )
    if mode == "fuzzy":
        new_used = new
        if "\r\n" in text and "\r\n" not in new:  # CRLF 文件：new 的换行归一为 CRLF，避免混排
            new_used = re.sub(r"\r?\n", "\r\n", new)
        new_text = _fuzzy_re(old).sub(lambda m: new_used, text, count=0 if replace_all else 1)
    else:
        new_text = text.replace(old, new) if replace_all else text.replace(old, new, 1)
    before_raw = real.read_bytes()  # 变更前原始字节（快照与精确恢复依据）
    diff = "\n".join(difflib.unified_diff(text.splitlines(), new_text.splitlines(), lineterm="", n=1))
    if len(diff) > _DIFF_MAX_CHARS:
        diff = diff[:_DIFF_MAX_CHARS] + "\n…[diff 过长已截断]"
    _write_newline_safe(real, new_text, encoding=write_enc)
    result = {
        "path": _display(real),
        "replacements": count if replace_all else 1,
        "match_mode": mode,
        "action": "edited",
        "diff": diff,
    }
    if (snap := _snapshot_change(before_raw, real.read_bytes())) is not None:
        result["undo"] = snap
    return result


def _fmt_mtime(st: os.stat_result) -> str:
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(st.st_mtime))


# ── 变更快照（回滚撤销 + 双侧 diff 数据源）：write_file/edit_file 成功后把变更前后的
# 原始字节落 MEMORY_DATA_DIR/undo/<id>.{before,after}（工作区外，不进文件树）。事件只带
# 引用不带内容——trace 不膨胀；undo 侧按 id 取字节对，after 与当前文件逐字节比对做冲突
# 检测（agent 写完后人又改过 → 跳过撤销，绝不硬覆盖）。超限或 IO 失败 → 无 undo 引用，
# 前端 diff 降级（edit 由 old/new 重构、write 新建 before=空、覆盖场景仅单侧）。
_UNDO_MAX_BYTES = 2 * 1024 * 1024


def _undo_dir() -> Path:
    base = os.environ.get("MEMORY_DATA_DIR") or str(
        Path(workspace_root()) / "plugins" / "memory" / "data"
    )
    d = Path(base) / "undo"
    d.mkdir(parents=True, exist_ok=True)
    return d


def _snapshot_change(before_raw: bytes | None, after_raw: bytes | None) -> dict | None:
    """写后快照。before_raw=None 表示新建（undo 时整文件删除）；after_raw=None 表示
    文件被删除（W16 bash 删除追溯：undo 时还原 before 字节）。"""
    try:
        if (after_raw is not None and len(after_raw) > _UNDO_MAX_BYTES) or (
            before_raw is not None and len(before_raw) > _UNDO_MAX_BYTES
        ):
            return None
        uid = f"{int(time.time() * 1000)}-{uuid.uuid4().hex[:8]}"
        (_undo_dir() / f"{uid}.before").write_bytes(before_raw or b"")
        if after_raw is not None:
            (_undo_dir() / f"{uid}.after").write_bytes(after_raw)
        return {
            "id": uid,
            "created": before_raw is None,
            "deleted": after_raw is None,
            "bytes_before": 0 if before_raw is None else len(before_raw),
            "bytes_after": 0 if after_raw is None else len(after_raw),
        }
    except OSError:
        return None


def _list_dir(args: dict) -> str:
    real = _guard(str(args.get("path") or "."))
    if not real.exists():
        raise ToolError(f"目录不存在: '{_display(real)}'", code="NOT_FOUND", field="path")
    if not real.is_dir():
        raise ToolError(f"'{_display(real)}' 是文件；读文件请用 read_file", code="IS_FILE", field="path")
    pattern = args.get("glob") or "*"
    if not isinstance(pattern, str):
        raise ToolError("参数 'glob' 必须是字符串（如 '*.py'）", code="BAD_ARG", field="glob")
    recursive = bool(args.get("recursive", False))
    include_noise = bool(args.get("include_noise", False))
    sort = str(args.get("sort") or "name").lower()
    if sort not in ("name", "size", "mtime"):
        raise ToolError("参数 'sort' 只支持 name|size|mtime", code="BAD_ARG", field="sort")
    max_entries = optional_int(args, "max_entries", 500, 1, _MAX_LIST_ENTRIES)
    max_depth = optional_int(args, "max_depth", 3, 1, 8)

    def sort_key(item):
        p, st = item
        return {"size": st.st_size, "mtime": st.st_mtime}.get(sort, p.name.lower())

    items: list[tuple[Path, os.stat_result]] = []
    if recursive:
        for dp, dns, fns in os.walk(real):
            depth = len(Path(dp).relative_to(real).parts)  # 根=0；该层序号 ≥ max_depth 时整层（含文件）跳过
            if depth >= max_depth:
                dns[:] = []
                continue
            elif not include_noise:  # 剪枝噪音目录与点目录
                dns[:] = [d for d in dns if not d.startswith(".") and d not in _NOISE_DIRS]
            base = Path(dp)
            for name in fns:
                if not fnmatch.fnmatch(name.lower(), pattern.lower()):
                    continue
                p = base / name
                try:
                    items.append((p, p.stat()))
                except OSError:
                    pass
    else:
        for e in sorted(real.iterdir(), key=lambda p: (p.is_file(), p.name.lower())):
            if not fnmatch.fnmatch(e.name.lower(), pattern.lower()):
                continue
            try:
                items.append((e, e.stat()))
            except OSError:
                pass
    items.sort(key=sort_key if sort != "name" else (lambda it: (it[0].is_file(), it[0].name.lower())))

    total = len(items)
    rows = []
    for p, st in items[:max_entries]:
        mt = _fmt_mtime(st)
        if recursive:
            rows.append(f"file  {p.relative_to(real).as_posix()}  ({st.st_size} bytes, {mt})")
        elif p.is_dir():
            rows.append(f"dir   {p.name}/  ({mt})")
        else:
            rows.append(f"file  {p.name}  ({st.st_size} bytes, {mt})")
    head = f"{_display(real)}/" + ("（递归）" if recursive else "")
    if not rows:
        if recursive:
            return f"{head}\n(无文件；噪音/点目录已剪枝，include_noise=true 可显示)"
        rows.append(f"(空，或无匹配 '{pattern}' 的条目)")
    elif total > max_entries:
        rows.append(f"[已达 max_entries={max_entries} 截断，共 {total} 项；请用 glob/path 收窄或调大 max_entries]")
    return head + "\n" + "\n".join(rows)


TOOLS = {
    "read_file": {
        "description": (
            "Read a text file inside the workspace. Returns line-numbered content. Auto-detects "
            "encoding (BOM/UTF-8/GBK, noted when not UTF-8); binary/image files are reported instead "
            "of garbled; overlong lines clipped at 2000 chars. "
            "Args: path (required), offset (line, default 0), limit (lines, default 2000)."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "description": "0-based line offset"},
                "limit": {"type": "integer", "description": "max lines to show"},
            },
            "required": ["path"],
        },
        "run": _read_file,
    },
    "write_file": {
        "description": (
            "Create or fully overwrite a file (parent dirs auto-created; atomic tmp+rename; newline "
            "style preserved). Overwrite writes '<name>.bak' backup by default (backup=false to skip) "
            "and returns line changes stats. Args: path, content (both required), backup (bool)."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
                "backup": {"type": "boolean", "description": "write .bak on overwrite (default true)"},
            },
            "required": ["path", "content"],
        },
        "run": _write_file,
    },
    "edit_file": {
        "description": (
            "Exact string replacement in a file. Falls back to trailing-whitespace/CRLF tolerant "
            "matching when exact fails; 0-match error lists near candidates; result includes unified "
            "diff and match_mode. Args: path, old_string, new_string (required), replace_all (bool)."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"},
            },
            "required": ["path", "old_string", "new_string"],
        },
        "run": _edit_file,
    },
    "list_dir": {
        "description": (
            "List a directory (name + type + size + mtime; sort=name|size|mtime). Recursive mode "
            "(recursive=true, max_depth default 3) lists files as flat relative paths, pruning "
            "dot-dirs and noise dirs (.git, node_modules, target, __pycache__, .venv, venv, dist, "
            "build, .idea; include_noise=true to keep). Args: path (default '.'), glob (default '*'), "
            "recursive, max_depth, sort, max_entries (default 500), include_noise."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "glob": {"type": "string"},
                "recursive": {"type": "boolean"},
                "max_depth": {"type": "integer", "description": "recursion depth cap (1-8)"},
                "sort": {"type": "string", "description": "name|size|mtime"},
                "max_entries": {"type": "integer"},
                "include_noise": {"type": "boolean"},
            },
            "required": [],
        },
        "run": _list_dir,
    },
}
