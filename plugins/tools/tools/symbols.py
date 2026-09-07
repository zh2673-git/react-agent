"""symbols_search 工具：语法级符号检索（2026-09-07 解冻新增，第 9 件内置工具，PLAN v4）。

把「文本搜索」升级为「语法级符号地图」：tree-sitter 解析源码，只回定义位置
（function/method/class/struct/...），模型无需从 grep 命中噪声里辨认哪一行是定义。
query = 符号名子串匹配（大小写不敏感）+ kind/language 过滤；不暴露原生 S-expression
query（模型手写语法错误率高，且定位语义即名字级搜索）。

纪律：与 grep 同源——_guard realpath 越界拦截（唯一拦截点）、点目录与噪音目录剪枝、
大文件（>1MB）跳过并计数、max_results 全局早停；即时解析不建索引（无跨调用可变态）。

依赖：tree-sitter-language-pack（pin <1.0——1.x 重写为按需下载 grammar，0.x wheel 内置
预编译 grammar 离线可用）。依赖缺失 → 模块 import 失败 → tools_plugin 顶层 try/except
跳过装载（fail-closed，其余 8 件与插件本身不受影响，与 reload 纪律一致）。
"""

import json
import os
from pathlib import Path

from tree_sitter_language_pack import get_parser

from . import ToolError, optional_int
from .files import _NOISE_DIRS, _guard, _display

_MAX_FILE_BYTES = 1024 * 1024
_MAX_SIG_CHARS = 160

# 扩展名 → 语言（未覆盖扩展名的文件直接跳过；language 过滤按此枚举校验）
_EXT_LANGS: dict[str, str] = {}
for _exts, _lang in (
    ((".rs",), "rust"),
    ((".py",), "python"),
    ((".ts",), "typescript"),
    ((".tsx",), "tsx"),
    ((".js", ".jsx", ".mjs", ".cjs"), "javascript"),
):
    for _e in _exts:
        _EXT_LANGS[_e] = _lang
SUPPORTED_LANGS = sorted(set(_EXT_LANGS.values()))

# kind 枚举（kind 过滤按此校验；大小写不敏感）
_KINDS = {
    "function", "method", "class", "struct", "enum", "trait",
    "impl", "module", "typedef", "const", "interface",
}

# 每语言定义节点表：node.type → (kind, name 字段)；kind=None 表示按父节点判定（fn→function/method）
_RUST_DEFS = {
    "function_item": (None, "name"),
    "struct_item": ("struct", "name"),
    "enum_item": ("enum", "name"),
    "trait_item": ("trait", "name"),
    "impl_item": ("impl", "type"),
    "mod_item": ("module", "name"),
    "type_item": ("typedef", "name"),
    "const_item": ("const", "name"),
}
_PY_DEFS = {
    "function_definition": (None, "name"),
    "class_definition": ("class", "name"),
}
_TS_DEFS = {
    "function_declaration": ("function", "name"),
    "class_declaration": ("class", "name"),
    "method_definition": ("method", "name"),
    "interface_declaration": ("interface", "name"),
    "type_alias_declaration": ("typedef", "name"),
    "enum_declaration": ("enum", "name"),
}
_LANG_DEFS: dict[str, dict] = {"rust": _RUST_DEFS, "python": _PY_DEFS, "typescript": _TS_DEFS, "tsx": _TS_DEFS, "javascript": _TS_DEFS}
# 可变值才视为函数绑定：const arrow = (x) => ...（排除 const x = foo() 调用结果）
_FN_VALUE_TYPES = {"arrow_function", "function_expression", "function"}


def _signature(text: str) -> str:
    """签名 = 节点文本截到第一个 {/;/换行，压平空白，封顶 160 字符。"""
    for ch in ("{", ";", "\n"):
        idx = text.find(ch)
        if idx != -1:
            text = text[:idx]
    return " ".join(text.split())[:_MAX_SIG_CHARS]


def _extract(root_node, lang: str) -> list[tuple[str, str, int, str]]:
    """深度优先提取定义：[(kind, name, line(1-based), signature)]。"""
    defs = _LANG_DEFS[lang]
    out: list[tuple[str, str, int, str]] = []
    stack = [root_node]
    while stack:
        n = stack.pop()
        if not n.is_named:
            continue
        rule = defs.get(n.type)
        if rule:
            kind, field = rule
            name_node = n.child_by_field_name(field)
            resolved = kind
            if resolved is None:  # function_item / function_definition 按父判定 method
                parent = n.parent
                if lang == "rust" and parent is not None and parent.type == "declaration_list":
                    resolved = "method"
                elif (
                    lang == "python"
                    and parent is not None and parent.type == "block"
                    and parent.parent is not None and parent.parent.type == "class_definition"
                ):
                    resolved = "method"
                else:
                    resolved = "function"
            if name_node is not None:
                name = (name_node.text or b"").decode("utf-8", "replace").strip()
                if name:
                    out.append((resolved, name, n.start_point[0] + 1, _signature((n.text or b"").decode("utf-8", "replace"))))
            # 定义节点内部仍可能嵌套定义（impl 内 fn、class 内 class）→ 继续下钻
        if n.type == "variable_declarator":
            val = n.child_by_field_name("value")
            name_node = n.child_by_field_name("name")
            if (
                val is not None and val.type in _FN_VALUE_TYPES
                and name_node is not None
            ):
                name = (name_node.text or b"").decode("utf-8", "replace").strip()
                if name:
                    out.append(("function", name, n.start_point[0] + 1, _signature((n.text or b"").decode("utf-8", "replace"))))
            continue  # declarator 子树无需再找定义
        stack.extend(reversed(n.children))
    return out


def _symbols_search(args: dict) -> str:
    query = args.get("query")
    if query is not None and (not isinstance(query, str) or not query.strip()):
        raise ToolError("参数 'query' 必须是非空字符串（符号名子串，大小写不敏感；省略则列出全部）", code="BAD_ARG", field="query")
    needle = query.strip().lower() if isinstance(query, str) else None
    kind = args.get("kind")
    if kind is not None:
        if not isinstance(kind, str) or kind.strip().lower() not in _KINDS:
            raise ToolError(f"参数 'kind' 须为 {sorted(_KINDS)} 之一，收到: {kind!r}", code="BAD_ARG", field="kind")
        kind = kind.strip().lower()
    lang = args.get("language")
    if lang is not None:
        if not isinstance(lang, str) or lang.strip().lower() not in SUPPORTED_LANGS:
            raise ToolError(f"参数 'language' 须为 {SUPPORTED_LANGS} 之一，收到: {lang!r}", code="BAD_ARG", field="language")
        lang = lang.strip().lower()
    max_results = optional_int(args, "max_results", 50, 1, 200)
    file_raw = args.get("file")
    if file_raw is not None and (not isinstance(file_raw, str) or not file_raw.strip()):
        raise ToolError("参数 'file' 必须是非空字符串", code="BAD_ARG", field="file")
    if file_raw is not None and args.get("path") is not None:
        raise ToolError("file 与 path 二选一，不可同时给出", code="BAD_ARG", field="file")

    if file_raw is not None:
        roots = [_guard(file_raw)]
    else:
        root = _guard(str(args.get("path") or "."))
        if not root.exists():
            raise ToolError(f"路径不存在: '{_display(root)}'", code="NOT_FOUND", field="path")
        roots = [root]

    symbols: list[dict] = []
    skipped_big = skipped_lang = 0
    truncated = False
    for root in roots:
        if truncated:
            break
        if root.is_file():
            entries = [root]
        else:
            entries = [Path(dp) / f for dp, _dns, fns in _walk_os(root) for f in fns]
        for p in entries:
            if truncated:
                break
            lang_of_file = _EXT_LANGS.get(p.suffix.lower())
            if lang_of_file is None or (lang is not None and lang_of_file != lang):
                skipped_lang += 1
                continue
            try:
                if p.stat().st_size > _MAX_FILE_BYTES:
                    skipped_big += 1
                    continue
                src = p.read_bytes()
            except OSError:
                continue
            try:
                tree = get_parser(lang_of_file).parse(src)
            except Exception:  # noqa: BLE001 - 单文件解析失败跳过（fail-open 于观测，不影响其余文件）
                continue
            for k, name, line, sig in _extract(tree.root_node, lang_of_file):
                if kind is not None and k != kind:
                    continue
                if needle is not None and needle not in name.lower():
                    continue
                symbols.append({
                    "kind": k,
                    "name": name,
                    "file": _display(p).replace(os.sep, "/"),
                    "line": line,
                    **({"signature": sig} if sig else {}),
                })
                if len(symbols) >= max_results:
                    truncated = True
                    break
    symbols.sort(key=lambda s: (s["file"], s["line"]))
    return json.dumps(
        {
            "symbols": symbols,
            "total": len(symbols),
            "truncated": truncated,
            **({"skipped": {"oversize": skipped_big, "unsupported_ext": skipped_lang}} if (skipped_big or skipped_lang) else {}),
        },
        ensure_ascii=False,
    )


def _walk_os(root: Path):
    """与 grep._walk_files 同纪律的目录遍历（点目录 + 噪音目录剪枝）。"""
    for dp, dns, fns in os.walk(root):
        dns[:] = [d for d in dns if not d.startswith(".") and d not in _NOISE_DIRS]
        yield dp, dns, fns


TOOLS = {
    "symbols_search": {
        "description": (
            "Syntax-level symbol search over source files via tree-sitter: returns definition sites only "
            "(kind/name/file/line/signature for function, method, class, struct, enum, trait, impl, module, "
            "typedef, const, interface). query is a case-insensitive substring of the symbol name; omit it to "
            "list all definitions in range. Languages: rust/py/ts/tsx/js (by extension). Skips dot-dirs, noise "
            "dirs, binary and >1MB files. Args: query?, file? | path? (mutually exclusive), kind?, language?, "
            "max_results (1-200, default 50)."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "symbol-name substring (case-insensitive); omit for all"},
                "file": {"type": "string", "description": "single file to scan (mutually exclusive with path)"},
                "path": {"type": "string", "description": "file or dir to scan (default '.')"},
                "kind": {"type": "string", "enum": sorted(_KINDS)},
                "language": {"type": "string", "enum": SUPPORTED_LANGS},
                "max_results": {"type": "integer", "description": "max symbols (1-200)"},
            },
        },
        "run": _symbols_search,
    },
}
