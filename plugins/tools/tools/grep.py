"""grep 工具：工作区正则内容搜索（2026-09-06 解冻新增，第 8 件内置工具，PLAN v3 §6.1；
2026-09-21 v5 效率增强：上下文行 / count / files_with_matches / .gitignore，PLAN §十一）。

纪律：与 files 同源的 realpath 越界拦截（唯一拦截点=操作点）；二进制（NUL 探测）与
大文件（>1MB）跳过并计数；点目录与噪音目录默认剪枝；max_results 全局早停防全库扫。
.gitignore 为轻量子集（! 取反 / 尾 / 目录锚 / 首 / 锚定 / basename 与路径后缀匹配，
最后匹配生效），语义缺失（** 递归、字符类、全局 excludes）由硬剪枝兜底（PLAN §十一）。
"""

import fnmatch
import os
import re
from pathlib import Path

from . import ToolError, optional_int, require
from .files import _NOISE_DIRS, _decode, _display, _guard

_MAX_FILE_BYTES = 1024 * 1024
_MAX_LINE_SHOW = 200
_OUTPUT_MODES = ("content", "count", "files_with_matches")


class _Ignore:
    """轻量 .gitignore（PLAN §十一）：逐目录加载规则，只作用于该目录子树。

    支持子集：`!` 取反、尾 `/` 目录锚、首 `/` 锚定；匹配 = basename 或路径后缀
    （fnmatch，`*` 跨 `/` 近似 `**`）；最后匹配生效（git 语义）。
    """

    def __init__(self) -> None:
        # (scope_posix, pattern, negated, dir_only, anchored)
        self.rules: list[tuple[str, str, bool, bool, bool]] = []
        self.files_skipped = 0
        self.dirs_skipped = 0

    def load(self, d: Path) -> None:
        try:
            text = (d / ".gitignore").read_text(encoding="utf-8", errors="replace")
        except OSError:
            return
        scope = d.as_posix()
        for raw in text.splitlines():
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            negated = line.startswith("!")
            if negated:
                line = line[1:].strip()
                if not line:
                    continue
            dir_only = line.endswith("/")
            if dir_only:
                line = line.rstrip("/")
            anchored = line.startswith("/")
            if anchored:
                line = line.lstrip("/")
            if not line:
                continue
            self.rules.append((scope, line, negated, dir_only, anchored))

    def _matched(self, rel: str, pat: str, dir_only: bool, anchored: bool) -> bool:
        parts = rel.split("/")
        if dir_only:
            # 文件：任一父组件命中；目录（剪枝判定）：含自身组件
            comps = parts[:-1] or parts
            return any(fnmatch.fnmatch(c, pat) for c in comps)
        if anchored:
            return fnmatch.fnmatch(rel, pat) or fnmatch.fnmatch(rel, pat + "/*")
        return fnmatch.fnmatch(parts[-1], pat) or any(
            fnmatch.fnmatch("/".join(parts[i:]), pat) for i in range(len(parts))
        )

    def ignored(self, path: Path) -> bool:
        p = path.as_posix()
        result = False
        for scope, pat, negated, dir_only, anchored in self.rules:
            if scope and not p.startswith(scope + "/"):
                continue
            rel = p[len(scope) + 1:]
            if self._matched(rel, pat, dir_only, anchored):
                result = not negated
        return result


def _walk_files(root: Path, ignore: _Ignore | None):
    """剪枝遍历：点目录 + 噪音目录不进入；.gitignore 忽略的文件跳过、目录不下钻；
    单文件路径直接产出。"""
    if root.is_file():
        yield root
        return
    for dp, dns, fns in os.walk(root):
        base = Path(dp)
        if ignore is not None:
            ignore.load(base)
            kept_dirs = [
                d for d in dns
                if not d.startswith(".") and d not in _NOISE_DIRS and not ignore.ignored(base / d)
            ]
            ignore.dirs_skipped += len(dns) - len(kept_dirs)
            dns[:] = kept_dirs
            kept_files = [f for f in fns if not ignore.ignored(base / f)]
            ignore.files_skipped += len(fns) - len(kept_files)
            fns = kept_files
        else:
            dns[:] = [d for d in dns if not d.startswith(".") and d not in _NOISE_DIRS]
        for f in sorted(fns):
            yield base / f


def _blocks(idxs: list[int], total: int, ctx_before: int, ctx_after: int) -> list[list[int]]:
    """命中行号 → 合并重叠后的连续区间 [lo, hi]（0-based 闭区间）。"""
    ranges: list[list[int]] = []
    for i in idxs:
        lo = max(0, i - ctx_before)
        hi = min(total - 1, i + ctx_after)
        if ranges and lo <= ranges[-1][1] + 1:
            ranges[-1][1] = max(ranges[-1][1], hi)
        else:
            ranges.append([lo, hi])
    return ranges


def _grep(args: dict) -> str:
    pattern = require(args, "pattern")
    path_raw = str(args.get("path") or ".")
    glob = args.get("glob")
    if glob is not None and (not isinstance(glob, str) or not glob.strip()):
        raise ToolError("参数 'glob' 必须是非空字符串（如 '*.py'）", code="BAD_ARG", field="glob")
    ignore_case = bool(args.get("ignore_case", False))
    max_results = optional_int(args, "max_results", 50, 1, 200)
    ctx_before = optional_int(args, "context_before", 0, 0, 10)
    ctx_after = optional_int(args, "context_after", 0, 0, 10)
    output_mode = args.get("output_mode") or "content"
    if output_mode not in _OUTPUT_MODES:
        raise ToolError(
            f"参数 'output_mode' 须为 {list(_OUTPUT_MODES)} 之一，收到: {output_mode!r}",
            code="BAD_ARG", field="output_mode",
        )
    respect_gitignore = bool(args.get("respect_gitignore", True))
    try:
        rx = re.compile(pattern, re.IGNORECASE if ignore_case else 0)
    except re.error as exc:
        raise ToolError(f"正则非法: {exc}", code="BAD_REGEX", field="pattern")

    root = _guard(path_raw)
    if not root.exists():
        raise ToolError(f"路径不存在: '{_display(root)}'", code="NOT_FOUND", field="path")

    ignore = _Ignore() if respect_gitignore else None
    hits: list[str] = []
    files_matched = skipped_bin = skipped_big = 0
    matched_total = 0  # content 模式按命中行计数（上下文行不计入 max_results）
    truncated = False
    with_ctx = ctx_before > 0 or ctx_after > 0
    for p in _walk_files(root, ignore):
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
        lines = text.splitlines()
        idxs = [i for i, line in enumerate(lines) if rx.search(line)]
        if not idxs:
            continue
        files_matched += 1
        rel = _display(p).replace(os.sep, "/")
        if output_mode == "count":
            hits.append(f"{rel}:{len(idxs)}")
            if len(hits) >= max_results:
                truncated = True
                break
        elif output_mode == "files_with_matches":
            hits.append(rel)
            if len(hits) >= max_results:
                truncated = True
                break
        elif with_ctx:
            match_set = set(idxs)
            for bi, (lo, hi) in enumerate(_blocks(idxs, len(lines), ctx_before, ctx_after)):
                if bi:
                    hits.append("--")
                for j in range(lo, hi + 1):
                    t = lines[j].strip()[:_MAX_LINE_SHOW]
                    if j in match_set:
                        hits.append(f"{rel}:{j + 1}: {t}")
                    else:
                        hits.append(f"{rel}-{j + 1}- {t}")
            matched_total += len(idxs)
            if matched_total >= max_results:
                truncated = True
                break
        else:
            for i in idxs:
                hits.append(f"{rel}:{i + 1}: {lines[i].strip()[:_MAX_LINE_SHOW]}")
            matched_total += len(idxs)
            if matched_total >= max_results:
                truncated = True
                break

    if not hits:
        extra = f"；已按 .gitignore 忽略 {ignore.files_skipped} 文件/{ignore.dirs_skipped} 目录" if ignore else ""
        return (
            f"0 命中: '{pattern}' 于 {_display(root)}\n"
            f"[已跳过二进制 {skipped_bin}、大文件 {skipped_big}{extra}；"
            "请确认 pattern/glob/path，或该范围确无文本匹配]"
        )
    notes = []
    if output_mode == "count":
        notes.append(f"{files_matched} 个文件命中（每文件计数，上限 {max_results}）")
    elif output_mode == "files_with_matches":
        notes.append(f"{files_matched} 个文件命中（仅列文件，上限 {max_results}）")
    else:
        notes.append(f"{files_matched} 个文件共 {matched_total} 处命中")
    if truncated:
        notes.append(f"已达 max_results={max_results} 截断，请收窄 pattern/glob/path 或调大上限")
    if skipped_bin or skipped_big:
        notes.append(f"跳过二进制 {skipped_bin}、大文件 {skipped_big}")
    if ignore and (ignore.files_skipped or ignore.dirs_skipped):
        notes.append(f"按 .gitignore 忽略文件 {ignore.files_skipped}、目录 {ignore.dirs_skipped}")
    return "\n".join(hits) + "\n\n[" + "；".join(notes) + "]"


TOOLS = {
    "grep": {
        "description": (
            "Search file contents across the workspace with a regular expression (grep-like). "
            "Skips binary files, files >1MB, dot-dirs, noise dirs (.git, node_modules, target, "
            "__pycache__, .venv, venv, dist, build, .idea) and .gitignore'd files. "
            "Args: pattern (required, regex), path (default '.'), glob (filename filter, e.g. '*.py'), "
            "ignore_case (bool, default false), max_results (1-200, default 50), "
            "output_mode ('content' default | 'count' per-file match counts | 'files_with_matches' file list — "
            "use count/files_with_matches first to gauge scope, then narrow), "
            "context_before / context_after (0-10, default 0: show surrounding lines per match, "
            "rg-style 'path-ln- text', blocks separated by '--' — avoids a follow-up read_file), "
            "respect_gitignore (bool, default true). "
            "Output lines: path:lineno: text."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "regular expression"},
                "path": {"type": "string", "description": "file or dir to search (default '.')"},
                "glob": {"type": "string", "description": "filename filter, e.g. '*.py'"},
                "ignore_case": {"type": "boolean"},
                "max_results": {"type": "integer", "description": "max matched lines / files (1-200)"},
                "output_mode": {
                    "type": "string",
                    "enum": list(_OUTPUT_MODES),
                    "description": "content (default) | count | files_with_matches",
                },
                "context_before": {"type": "integer", "description": "context lines before each match (0-10)"},
                "context_after": {"type": "integer", "description": "context lines after each match (0-10)"},
                "respect_gitignore": {"type": "boolean", "description": "skip .gitignore'd files (default true)"},
            },
            "required": ["pattern"],
        },
        "run": _grep,
    },
}
