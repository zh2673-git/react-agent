"""grep 工具：工作区正则内容搜索（2026-09-06 解冻新增，第 8 件内置工具，PLAN v3 §6.1）。

纪律：与 files 同源的 realpath 越界拦截（唯一拦截点=操作点）；二进制（NUL 探测）与
大文件（>1MB）跳过并计数；点目录与噪音目录默认剪枝；max_results 全局早停防全库扫。
"""

import fnmatch
import os
import re
from pathlib import Path

from . import ToolError, optional_int, require
from .files import _NOISE_DIRS, _decode, _display, _guard

_MAX_FILE_BYTES = 1024 * 1024
_MAX_LINE_SHOW = 200


def _walk_files(root: Path):
    """剪枝遍历：点目录 + 噪音目录不进入；单文件路径直接产出。"""
    if root.is_file():
        yield root
        return
    for dp, dns, fns in os.walk(root):
        dns[:] = [d for d in dns if not d.startswith(".") and d not in _NOISE_DIRS]
        base = Path(dp)
        for f in sorted(fns):
            yield base / f


def _grep(args: dict) -> str:
    pattern = require(args, "pattern")
    path_raw = str(args.get("path") or ".")
    glob = args.get("glob")
    if glob is not None and (not isinstance(glob, str) or not glob.strip()):
        raise ToolError("参数 'glob' 必须是非空字符串（如 '*.py'）", code="BAD_ARG", field="glob")
    ignore_case = bool(args.get("ignore_case", False))
    max_results = optional_int(args, "max_results", 50, 1, 200)
    try:
        rx = re.compile(pattern, re.IGNORECASE if ignore_case else 0)
    except re.error as exc:
        raise ToolError(f"正则非法: {exc}", code="BAD_REGEX", field="pattern")

    root = _guard(path_raw)
    if not root.exists():
        raise ToolError(f"路径不存在: '{_display(root)}'", code="NOT_FOUND", field="path")

    hits: list[str] = []
    files_matched = skipped_bin = skipped_big = 0
    truncated = False
    for p in _walk_files(root):
        if truncated:
            break
        if glob and not fnmatch.fnmatch(p.name.lower(), glob.lower()):
            continue
        try:
            if p.stat().st_size > _MAX_FILE_BYTES:
                skipped_big += 1
                continue
            raw = p.read_bytes()
        except OSError:
            continue
        if b"\x00" in raw[:8192]:
            skipped_bin += 1
            continue
        text, _ = _decode(raw)
        matched_here = False
        for i, line in enumerate(text.splitlines()):
            if rx.search(line):
                if not matched_here:
                    files_matched += 1
                    matched_here = True
                hits.append(f"{_display(p).replace(os.sep, '/')}:{i + 1}: {line.strip()[:_MAX_LINE_SHOW]}")
                if len(hits) >= max_results:
                    truncated = True
                    break
    if not hits:
        return (
            f"0 命中: '{pattern}' 于 {_display(root)}\n"
            f"[已跳过二进制 {skipped_bin}、大文件 {skipped_big}；"
            "请确认 pattern/glob/path，或该范围确无文本匹配]"
        )
    notes = [f"{files_matched} 个文件共 {len(hits)} 处命中"]
    if truncated:
        notes.append(f"已达 max_results={max_results} 截断，请收窄 pattern/glob/path 或调大上限")
    if skipped_bin or skipped_big:
        notes.append(f"跳过二进制 {skipped_bin}、大文件 {skipped_big}")
    return "\n".join(hits) + "\n\n[" + "；".join(notes) + "]"


TOOLS = {
    "grep": {
        "description": (
            "Search file contents across the workspace with a regular expression (grep-like). "
            "Skips binary files, files >1MB, dot-dirs and noise dirs (.git, node_modules, target, "
            "__pycache__, .venv, venv, dist, build, .idea). "
            "Args: pattern (required, regex), path (default '.'), glob (filename filter, e.g. '*.py'), "
            "ignore_case (bool, default false), max_results (1-200, default 50). "
            "Output lines: path:lineno: text."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "regular expression"},
                "path": {"type": "string", "description": "file or dir to search (default '.')"},
                "glob": {"type": "string", "description": "filename filter, e.g. '*.py'"},
                "ignore_case": {"type": "boolean"},
                "max_results": {"type": "integer", "description": "max hits (1-200)"},
            },
            "required": ["pattern"],
        },
        "run": _grep,
    },
}
