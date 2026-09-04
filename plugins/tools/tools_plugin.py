"""tools guest 插件（Python，Process 域 gRPC）——瘦分派层 + scope 装配。

线契约（03 §2.3）：
  {"op":"list"} → {"ok":true,"tools":[{"name","description","parameters"}]}
  {"op":"list","all":true} → 额外含未启用工具，各项附 "enabled": bool（配置中心视图，08）
  {"op":"call","name":str,"args":object} → {"ok":true,"result":any} | {"ok":false,"error":{"code","message","field"?}}

扩展 op（08，Phase 4）：
  {"op":"configure","enabled":[str,...]}       → 运行时整体替换白名单（未知名 → 字段级 400）
  {"op":"reload"}                              → 动态装载 tools/ 目录下的工具模块（L2 自扩展）。
                                                 **装载 ≠ 启用**：新工具进可用池但不进白名单，
                                                 需显式 configure 才出现在 list（写与启用两步分离）。
                                                 单模块 import 失败 → 跳过并回 skipped（fail-closed，
                                                 该模块旧工具保留，内置工具永不覆盖）。

工具实现位于 tools/ 包（files / bash / web，按运行时关注点分文件）；
本文件只做：init 装配 scope → list 过滤 → call 校验分发 → ToolError 转字段级错误。

scope：TOOLS_ENABLED=read_file,bash（白名单；未列出的工具 Schema 与实现双不可见）。
保留名 load_skill 不在本注册表（归 agent-loop 路由，见 03 §3）。
"""

import importlib
import os
import sys
from pathlib import Path

from agent_kernel.guest import serve

from tools import ToolError
from tools import bash as bash_mod
from tools import files as files_mod
from tools import web as web_mod

ALL_TOOLS: dict = {**files_mod.TOOLS, **bash_mod.TOOLS, **web_mod.TOOLS}
_ENABLED: set[str] = set()
# L2 动态装载：按模块分表保留（某模块 reload 失败时其旧工具原样保留——fail-closed）
_EXTRA_BY_MODULE: dict[str, dict] = {}

# reload 时跳过的内置模块名（重入 sys.modules 清理无意义，且防止测试/误删内置实现）
_BUILTIN_MODULES = frozenset({"files", "bash", "web"})
# 动态工具条目必须暴露的键（与内置 ToolSpec 三元组同规范，05 §2）
_REQUIRED_KEYS = ("name", "description", "parameters", "run")


def _parse_enabled() -> set[str]:
    raw = (os.environ.get("TOOLS_ENABLED") or "").strip()
    if not raw:
        return set(ALL_TOOLS)
    names = {n.strip() for n in raw.split(",") if n.strip()}
    unknown = names - set(ALL_TOOLS)
    if unknown:
        raise SystemExit(f"TOOLS_ENABLED 含未知工具: {sorted(unknown)}（合法值: {sorted(ALL_TOOLS)}）")
    return names


def _pool() -> dict:
    """可用池 = 内置 + 动态装载（动态未启用不出 list，见「装载 ≠ 启用」）。"""
    return {**ALL_TOOLS, **_extra_pool()}


def _extra_pool() -> dict:
    merged: dict = {}
    for mod_tools in _EXTRA_BY_MODULE.values():
        merged.update(mod_tools)
    return merged


def _err(message: str, code: str = "TOOL_ERROR", field: str | None = None) -> dict:
    error: dict = {"code": code, "message": message}
    if field:
        error["field"] = field
    return {"ok": False, "error": error}


def _reload_modules() -> tuple[list[str], list[str], list[dict]]:
    """扫描 tools/ 包目录逐模块动态装载。返回 (loaded_modules, added_tools, skipped)。"""
    pkg_dir = Path(__import__("tools").__file__).resolve().parent
    loaded: list[str] = []
    added: list[str] = []
    skipped: list[dict] = []
    before = set(_pool())
    for py in sorted(pkg_dir.glob("*.py")):
        mod_name = py.stem
        if mod_name == "__init__" or mod_name in _BUILTIN_MODULES:
            continue
        try:
            sys.modules.pop(f"tools.{mod_name}", None)
            mod = importlib.import_module(f"tools.{mod_name}")
            spec = getattr(mod, "TOOLS", None)
            if not isinstance(spec, dict) or not spec:
                raise TypeError("模块未暴露非空 TOOLS dict")
            clean: dict = {}
            for name, t in spec.items():
                if not isinstance(t, dict) or any(k not in t for k in _REQUIRED_KEYS) or not callable(t["run"]):
                    raise TypeError(f"工具 '{name}' 形状不合规（需 name/description/parameters/run）")
                if name in ALL_TOOLS:
                    raise TypeError(f"工具名 '{name}' 与内置冲突（内置不可覆盖）")
                clean[name] = t
            _EXTRA_BY_MODULE[mod_name] = clean
            loaded.append(mod_name)
        except Exception as exc:  # noqa: BLE001 - 单模块失败跳过，旧表保留（fail-closed）
            skipped.append({"module": mod_name, "error": f"{type(exc).__name__}: {exc}"})
    added = sorted(set(_pool()) - before)
    return loaded, added, skipped


class ToolsPlugin:
    def manifest(self) -> dict:
        return {"id": "tools", "version": "0.2.0", "api_version": "0.1"}

    def init(self, config) -> None:
        global _ENABLED
        _ENABLED = _parse_enabled()
        os.environ.setdefault("WORKSPACE_ROOT", os.getcwd())

    def on_event(self, envelope: dict) -> dict:
        payload = envelope.get("payload") or {}
        op = payload.get("op")
        if op == "list":
            show_all = bool(payload.get("all"))
            pool = _pool()
            tools = [
                {
                    "name": name,
                    "description": spec["description"],
                    "parameters": spec["parameters"],
                    **({"enabled": name in _ENABLED} if show_all else {}),
                }
                for name, spec in pool.items()
                if show_all or name in _ENABLED
            ]
            return {"ok": True, "tools": tools}
        if op == "call":
            return self._call(payload)
        if op == "configure":
            return self._configure(payload)
        if op == "reload":
            loaded, added, skipped = _reload_modules()
            return {"ok": True, "loaded": loaded, "added": added, "skipped": skipped,
                    "enabled_hint": "added 工具未启用（装载≠启用），需 configure 加入 enabled"}
        return _err(f"unknown op: {op}", code="K400")

    def _configure(self, payload: dict) -> dict:
        global _ENABLED
        names = payload.get("enabled")
        if not isinstance(names, list) or not names or not all(isinstance(n, str) and n.strip() for n in names):
            return _err("enabled 必须是非空字符串数组", code="K400", field="enabled")
        uniq = {n.strip() for n in names}
        unknown = uniq - set(_pool())
        if unknown:
            return _err(
                f"未知工具: {sorted(unknown)}（合法值: {sorted(_pool())}）",
                code="K400",
                field="enabled",
            )
        _ENABLED = uniq
        return {"ok": True, "enabled": sorted(_ENABLED)}

    def _call(self, payload: dict) -> dict:
        name = payload.get("name")
        if name not in _pool():
            avail = sorted(_ENABLED)
            return _err(f"unknown tool: {name}（可用: {', '.join(avail)}）", code="UNKNOWN_TOOL")
        if name not in _ENABLED:
            return _err(
                f"tool '{name}' 未授权（TOOLS_ENABLED={','.join(sorted(_ENABLED))}）。"
                "如需启用请经 configure op 或宿主 TOOLS_ENABLED 调整（装载≠启用）。",
                code="TOOL_DISABLED",
            )
        args = payload.get("args") or {}
        try:
            if not isinstance(args, dict):
                return _err(f"args 必须是对象，收到: {type(args).__name__}", code="BAD_ARGS", field="args")
            return {"ok": True, "result": _pool()[name]["run"](args)}
        except ToolError as exc:
            return _err(str(exc), code=exc.code, field=exc.field)
        except Exception as exc:  # noqa: BLE001 - 工具失败回喂 LLM，不中断循环
            return _err(f"{type(exc).__name__}: {exc}")

    def destroy(self) -> None:
        pass


if __name__ == "__main__":
    serve(ToolsPlugin())
