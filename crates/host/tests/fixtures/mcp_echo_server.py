"""MCP stdio mock server（e2e fixture）：initialize / tools/list / tools/call 最小实现。

工具两件：
  echo —— 回显参数（content text = "echo: <text>"）；
  boom —— isError=true 的工具级失败（content text = "boom-failed"）。

RA_MCP_CRASH=1 时在 tools/list 阶段直接退出（模拟握手后崩溃 server，
供 e2e 断言 fail-closed 跳过与 mcp_tools 视图 failed 状态）。
"""
import json
import os
import sys

LOG = os.path.join(os.environ.get("TEMP", "/tmp"), "mcp_fixture.log")


def _log(msg: str) -> None:
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(f"{msg}\n")


def main() -> None:
    _log(f"started pid={os.getpid()} argv={sys.argv}")
    while True:
        line = sys.stdin.readline()
        if not line:
            _log("stdin closed")
            return
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            _log(f"non-json line: {line[:80]}")
            continue
        method = msg.get("method")
        rid = msg.get("id")
        _log(f"recv method={method} id={rid}")
        if method == "notifications/initialized":
            continue
        if method == "initialize":
            resp = {
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock-echo", "version": "0.0.1"},
                },
            }
        elif method == "tools/list":
            if os.environ.get("RA_MCP_CRASH") == "1":
                os._exit(1)
            resp = {
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "回显输入文本",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"text": {"type": "string"}},
                            },
                        },
                        {
                            "name": "boom",
                            "description": "总是失败",
                            "inputSchema": {"type": "object", "properties": {}},
                        },
                    ]
                },
            }
        elif method == "tools/call":
            params = msg.get("params") or {}
            name = params.get("name")
            args = params.get("arguments") or {}
            if name == "echo":
                result = {"content": [{"type": "text", "text": f"echo: {args.get('text', '')}"}]}
            elif name == "boom":
                result = {"content": [{"type": "text", "text": "boom-failed"}], "isError": True}
            else:
                resp = {
                    "jsonrpc": "2.0",
                    "id": rid,
                    "error": {"code": -32602, "message": f"unknown tool: {name}"},
                }
                sys.stdout.write(json.dumps(resp) + "\n")
                sys.stdout.flush()
                continue
            resp = {"jsonrpc": "2.0", "id": rid, "result": result}
        elif rid is not None:
            resp = {
                "jsonrpc": "2.0",
                "id": rid,
                "error": {"code": -32601, "message": f"unknown method: {method}"},
            }
        else:
            continue
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
