"""tools guest 插件（Python，Process 域 gRPC）——瘦分派层 + scope 装配。

线契约（03 §2.3）：
  {"op":"list"} → {"ok":true,"tools":[{"name","description","parameters"}]}
  {"op":"call","name":str,"args":object} → {"ok":true,"result":any} | {"ok":false,"error":{"code","message","field"?}}

工具实现位于 tools/ 包（files / bash / web，按运行时关注点分文件）；
本文件只做：init 装配 scope → list 过滤 → call 校验分发 → ToolError 转字段级错误。

scope：TOOLS_ENABLED=read_file,bash（白名单；未列出的工具 Schema 与实现双不可见）。
保留名 load_skill 不在本注册表（归 agent-loop 路由，见 03 §3）。
"""

import os

from agent_kernel.guest import serve

from tools import ToolError
from tools import bash as bash_mod
from tools import files as files_mod
from tools import web as web_mod

ALL_TOOLS: dict = {**files_mod.TOOLS, **bash_mod.TOOLS, **web_mod.TOOLS}
_ENABLED: set[str] = set()


def _parse_enabled() -> set[str]:
    raw = (os.environ.get("TOOLS_ENABLED") or "").strip()
    if not raw:
        return set(ALL_TOOLS)
    names = {n.strip() for n in raw.split(",") if n.strip()}
    unknown = names - set(ALL_TOOLS)
    if unknown:
        raise SystemExit(f"TOOLS_ENABLED 含未知工具: {sorted(unknown)}（合法值: {sorted(ALL_TOOLS)}）")
    return names


def _err(message: str, code: str = "TOOL_ERROR", field: str | None = None) -> dict:
    error: dict = {"code": code, "message": message}
    if field:
        error["field"] = field
    return {"ok": False, "error": error}


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
            tools = [
                {"name": name, "description": spec["description"], "parameters": spec["parameters"]}
                for name, spec in ALL_TOOLS.items()
                if name in _ENABLED
            ]
            return {"ok": True, "tools": tools}
        if op == "call":
            return self._call(payload)
        return _err(f"unknown op: {op}", code="K400")

    def _call(self, payload: dict) -> dict:
        name = payload.get("name")
        if name not in ALL_TOOLS:
            avail = sorted(_ENABLED)
            return _err(f"unknown tool: {name}（可用: {', '.join(avail)}）", code="UNKNOWN_TOOL")
        if name not in _ENABLED:
            return _err(
                f"tool '{name}' 未授权（TOOLS_ENABLED={','.join(sorted(_ENABLED))}）。"
                "如需启用请在宿主环境调整 TOOLS_ENABLED。",
                code="TOOL_DISABLED",
            )
        args = payload.get("args") or {}
        try:
            if not isinstance(args, dict):
                return _err(f"args 必须是对象，收到: {type(args).__name__}", code="BAD_ARGS", field="args")
            return {"ok": True, "result": ALL_TOOLS[name]["run"](args)}
        except ToolError as exc:
            return _err(str(exc), code=exc.code, field=exc.field)
        except Exception as exc:  # noqa: BLE001 - 工具失败回喂 LLM，不中断循环
            return _err(f"{type(exc).__name__}: {exc}")

    def destroy(self) -> None:
        pass


if __name__ == "__main__":
    serve(ToolsPlugin())
