"""tools guest 插件（Python，Process 域 gRPC）。纯 stdlib。

线契约：
  {"op":"list"} → {"ok":true,"tools":[{"name","description","parameters"}]}
  {"op":"call","name":str,"args":object} → {"ok":true,"result":any} | {"ok":false,"error":{"message"}}

内置工具：
  calculator   — AST 白名单求值（禁 eval）
  current_time — ISO-8601（可选时区）
  http_get     — urllib，scheme 白名单 http/https，10s 超时
"""

import ast
import operator
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from zoneinfo import ZoneInfo

from agent_kernel.guest import serve


def _err(message: str) -> dict:
    return {"ok": False, "error": {"message": message}}


# ---- calculator：AST 白名单求值（禁 eval/exec） -----------------------------

_BIN_OPS = {
    ast.Add: operator.add,
    ast.Sub: operator.sub,
    ast.Mult: operator.mul,
    ast.Div: operator.truediv,
    ast.FloorDiv: operator.floordiv,
    ast.Mod: operator.mod,
    ast.Pow: operator.pow,
}
_UNARY_OPS = {ast.USub: operator.neg, ast.UAdd: operator.pos}


def _calc(node) -> float:
    if isinstance(node, ast.Expression):
        return _calc(node.body)
    if isinstance(node, ast.Constant) and isinstance(node.value, (int, float)) and not isinstance(node.value, bool):
        return node.value
    if isinstance(node, ast.BinOp) and type(node.op) in _BIN_OPS:
        return _BIN_OPS[type(node.op)](_calc(node.left), _calc(node.right))
    if isinstance(node, ast.UnaryOp) and type(node.op) in _UNARY_OPS:
        return _UNARY_OPS[type(node.op)](_calc(node.operand))
    raise ValueError(f"unsupported expression element: {type(node).__name__}")


# ---- 工具实现 ---------------------------------------------------------------

TOOLS = {
    "calculator": {
        "description": "Evaluate an arithmetic expression. Supports + - * / // % ** and parentheses.",
        "parameters": {"type": "object", "properties": {"expr": {"type": "string"}}, "required": ["expr"]},
        "run": lambda args: _calc(ast.parse(args["expr"], mode="eval")),
    },
    "current_time": {
        "description": "Current time in ISO-8601. Optional tz (IANA name, default UTC).",
        "parameters": {"type": "object", "properties": {"tz": {"type": "string"}}, "required": []},
        "run": lambda args: (
            datetime.now(ZoneInfo(args["tz"])).isoformat()
            if args.get("tz")
            else datetime.now(timezone.utc).isoformat()
        ),
    },
    "http_get": {
        "description": "HTTP GET a URL (http/https only, 10s timeout, first 64KB).",
        "parameters": {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]},
        "run": lambda args: _http_get(args["url"]),
    },
}


def _http_get(url: str) -> dict:
    scheme = urllib.parse.urlsplit(url).scheme
    if scheme not in ("http", "https"):
        raise ValueError(f"scheme '{scheme}' not allowed (http/https only)")
    with urllib.request.urlopen(url, timeout=10) as resp:  # noqa: S310 - scheme 已白名单
        body = resp.read(64 * 1024)
        return {"status": resp.status, "body": body.decode("utf-8", errors="replace")}


class ToolsPlugin:
    def manifest(self) -> dict:
        return {"id": "tools", "version": "0.1.0", "api_version": "0.1"}

    def init(self, config) -> None:
        pass

    def on_event(self, envelope: dict) -> dict:
        payload = envelope.get("payload") or {}
        op = payload.get("op")
        if op == "list":
            tools = [
                {"name": name, "description": spec["description"], "parameters": spec["parameters"]}
                for name, spec in TOOLS.items()
            ]
            return {"ok": True, "tools": tools}
        if op == "call":
            name = payload.get("name")
            spec = TOOLS.get(name)
            if spec is None:
                return _err(f"unknown tool: {name}")
            args = payload.get("args") or {}
            try:
                return {"ok": True, "result": spec["run"](args)}
            except Exception as exc:  # noqa: BLE001 - 工具失败回喂 LLM，不中断循环
                return _err(f"{type(exc).__name__}: {exc}")
        return _err(f"unknown op: {op}")

    def destroy(self) -> None:
        pass


if __name__ == "__main__":
    serve(ToolsPlugin())
