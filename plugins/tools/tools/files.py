"""4 个文件工具 + realpath 越界校验（唯一拦截点：拦截点=操作点，空间防越界）。"""

import fnmatch
import os
from pathlib import Path

from . import ToolError, optional_int, require, workspace_root

_MAX_READ_BYTES = 256 * 1024


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


def _read_text(real: Path) -> tuple[str, bool]:
    raw = real.read_bytes()
    truncated_bytes = len(raw) > _MAX_READ_BYTES
    if truncated_bytes:
        raw = raw[:_MAX_READ_BYTES]
    return raw.decode("utf-8", errors="replace"), truncated_bytes


def _read_file(args: dict) -> str:
    real = _guard(require(args, "path"))
    if real.is_dir():
        raise ToolError(f"'{real}' 是目录；读目录请用 list_dir", code="IS_DIR", field="path")
    if not real.exists():
        raise ToolError(f"文件不存在: '{_display(real)}'", code="NOT_FOUND", field="path")
    offset = optional_int(args, "offset", 0, 0, 10**9)
    limit = optional_int(args, "limit", 2000, 1, 10_000)
    text, cut = _read_text(real)
    lines = text.splitlines()
    window = lines[offset : offset + limit]
    numbered = "\n".join(f"{offset + i + 1:6d}\t{line}" for i, line in enumerate(window))
    notes = []
    if cut:
        notes.append(f"注意: 文件超过 {_MAX_READ_BYTES // 1024}KB，仅读取前 {_MAX_READ_BYTES // 1024}KB")
    if offset + limit < len(lines):
        notes.append(f"截断: 显示第 {offset + 1}-{offset + len(window)} 行（共 {len(lines)} 行）；继续读请用 offset={offset + limit}")
    return numbered + ("\n\n[" + "；".join(notes) + "]" if notes else "")


def _write_file(args: dict) -> dict:
    real = _guard(require(args, "path"))
    content = args.get("content")
    if not isinstance(content, str):
        raise ToolError("缺少必填参数 'content'（string，全量覆盖写入）", code="MISSING_ARG", field="content")
    real.parent.mkdir(parents=True, exist_ok=True)
    real.write_text(content, encoding="utf-8")
    return {"path": _display(real), "bytes": len(content.encode("utf-8")), "action": "written"}


def _edit_file(args: dict) -> dict:
    real = _guard(require(args, "path"))
    old = require(args, "old_string")
    new = require(args, "new_string")
    replace_all = bool(args.get("replace_all", False))
    if old == new:
        raise ToolError("old_string 与 new_string 相同，无需编辑", code="NO_OP", field="new_string")
    if not real.exists():
        raise ToolError(f"文件不存在: '{_display(real)}'（新建文件请用 write_file）", code="NOT_FOUND", field="path")
    text, _ = _read_text(real)
    count = text.count(old)
    if count == 0:
        raise ToolError(
            f"old_string 在文件中命中 0 处；请确认精确文本（含缩进/换行），或先用 read_file 查看",
            code="EDIT_NO_MATCH",
            field="old_string",
        )
    if count > 1 and not replace_all:
        raise ToolError(
            f"old_string 命中 {count} 处；请提供更长的上下文使其唯一，或传 replace_all=true 全部替换",
            code="EDIT_AMBIGUOUS",
            field="old_string",
        )
    new_text = text.replace(old, new) if replace_all else text.replace(old, new, 1)
    real.write_text(new_text, encoding="utf-8")
    return {"path": _display(real), "replacements": count if replace_all else 1, "action": "edited"}


def _list_dir(args: dict) -> str:
    real = _guard(str(args.get("path") or "."))
    if not real.exists():
        raise ToolError(f"目录不存在: '{_display(real)}'", code="NOT_FOUND", field="path")
    if not real.is_dir():
        raise ToolError(f"'{_display(real)}' 是文件；读文件请用 read_file", code="IS_FILE", field="path")
    pattern = args.get("glob") or "*"
    if not isinstance(pattern, str):
        raise ToolError("参数 'glob' 必须是字符串（如 '*.py'）", code="BAD_ARG", field="glob")
    entries = sorted(real.iterdir(), key=lambda p: (p.is_file(), p.name.lower()))
    rows = []
    for e in entries:
        if not fnmatch.fnmatch(e.name.lower(), pattern.lower()):
            continue
        if e.is_dir():
            rows.append(f"dir   {e.name}/")
        else:
            try:
                size = e.stat().st_size
            except OSError:
                size = -1
            rows.append(f"file  {e.name}  ({size} bytes)")
    if not rows:
        rows.append(f"(空，或无匹配 '{pattern}' 的条目)")
    return f"{_display(real)}/\n" + "\n".join(rows)


TOOLS = {
    "read_file": {
        "description": (
            "Read a text file inside the workspace. Returns line-numbered content. "
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
            "Create or fully overwrite a file (parent dirs auto-created). Args: path, content (both required)."
        ),
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
            "required": ["path", "content"],
        },
        "run": _write_file,
    },
    "edit_file": {
        "description": (
            "Exact string replacement in a file. old_string must match exactly (incl. whitespace). "
            "Fails with match count when 0 or ambiguous. Args: path, old_string, new_string (required), replace_all (bool)."
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
        "description": "List a directory (name + type + size). Args: path (default '.'), glob (default '*', e.g. '*.py').",
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "glob": {"type": "string"}},
            "required": [],
        },
        "run": _list_dir,
    },
}
