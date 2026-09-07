"""MCP stdio 客户端（tools PLAN §八）——Model Context Protocol 服务器子进程接入。

职责：把外部 MCP stdio server（每台一个子进程，JSON-RPC 2.0 换行分帧）接入工具池。
本模块只做协议传输，不做工具池/启用闸判断（归 tools_plugin）。

连接时序：spawn → `initialize` 握手 → `notifications/initialized` 通知 → `tools/list`
（分页取全）。调用：`tools/call`（name 用服务器侧原名，命名空间由 tools_plugin 包装）。

生命周期：server 随插件 destroy **逆序**回收（先 terminate 后 kill）；进程崩溃后
调用触发按退避窗口（5s）重启——窗口内直接报 MCP_UNAVAILABLE，不高频抖动重启。

线程模型：每 server 一条常驻读线程（stdout 逐行 → 响应队列，仅带 id 的响应入队；
通知与服务端请求忽略）。请求-响应按 id 配对，陈旧/乱序响应丢弃；tools 插件为 Serial
guest，同一时刻至多一个在途请求，id 配对只为兜底超时后的迟到响应。
"""

import json
import os
import queue
import shutil
import subprocess
import sys
import threading
import time

PROTOCOL_VERSION = "2024-11-05"
RESTART_BACKOFF_SECS = 5.0

# 读线程 EOF 哨兵：进程退出即刻唤醒等待中的 request（不等超时烧满——crasher 类
# server 握手即死，若等超时会把 guest init 拖过内核 init 窗口）
_EOF = object()


class McpError(Exception):
    """MCP 传输/远端错误（message 面向模型回喂，code 进字段级错误）。"""

    def __init__(self, message: str, code: str = "MCP_ERROR"):
        super().__init__(message)
        self.code = code


class McpServer:
    """单台 MCP stdio server 的子进程会话。"""

    def __init__(self, name: str, spec: dict):
        self.name = name
        self.cmd = [str(c) for c in spec["command"]]
        self.cwd = spec.get("cwd")
        self.env_extra = {str(k): str(v) for k, v in (spec.get("env") or {}).items()}
        self._proc: subprocess.Popen | None = None
        self._responses: queue.Queue = queue.Queue()
        self._next_id = 0
        self._id_lock = threading.Lock()
        self._last_start = 0.0  # monotonic；崩溃退避窗口基准
        self.status = "stopped"

    # ── 进程与会话 ────────────────────────────────────────────────────────

    def _spawn(self) -> None:
        cmd = list(self.cmd)
        if os.sep not in cmd[0] and "/" not in cmd[0]:
            # Windows 下 .cmd 脚本（npm/npx）不被 CreateProcess 直接解析，按 PATH 解析全名
            resolved = shutil.which(cmd[0])
            if resolved:
                cmd[0] = resolved
        env = {**os.environ, **self.env_extra} if self.env_extra else None
        kwargs = {"cwd": str(self.cwd)} if self.cwd else {}
        self._proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,  # MCP server 日志走 stderr；直弃防管道阻塞
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
            **kwargs,
        )
        self._responses = queue.Queue()
        self._last_start = time.monotonic()
        threading.Thread(target=self._pump, args=[self._proc], daemon=True).start()

    def _pump(self, proc: subprocess.Popen) -> None:
        """读线程：stdout 逐行 → 响应队列。stdout 关闭 = 进程退出。"""
        for line in proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                continue  # 非 JSON 行（server 杂散输出）忽略
            if isinstance(msg, dict) and "id" in msg and ("result" in msg or "error" in msg):
                self._responses.put(msg)
        self._responses.put(_EOF)
        self.status = "crashed"

    def start(self, timeout: float = 15.0) -> dict:
        """spawn + initialize 握手 + initialized 通知。返回 initialize result。"""
        self._spawn()
        self.status = "starting"
        result = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "react-agent", "version": "0.4.0"},
            },
            timeout,
        )
        self.notify("notifications/initialized")
        self.status = "ready"
        return result

    def _ensure_running(self) -> None:
        """调用前保活：活着直接用；崩溃则按退避窗口重启（窗口内拒绝，防抖动）。"""
        if self._proc is not None and self._proc.poll() is None and self.status == "ready":
            return
        if time.monotonic() - self._last_start < RESTART_BACKOFF_SECS:
            raise McpError(f"MCP server '{self.name}' 不可用（崩溃退避中，稍后重试）", "MCP_UNAVAILABLE")
        self.start()

    # ── JSON-RPC ─────────────────────────────────────────────────────────

    def _write(self, msg: dict) -> None:
        if self._proc is None or self._proc.stdin is None:
            raise McpError(f"MCP server '{self.name}' 未连接", "MCP_UNAVAILABLE")
        try:
            self._proc.stdin.write(json.dumps(msg, ensure_ascii=False) + "\n")
            self._proc.stdin.flush()
        except (OSError, ValueError) as exc:
            raise McpError(f"MCP server '{self.name}' 写入失败: {exc}", "MCP_UNAVAILABLE") from exc

    def notify(self, method: str, params: dict | None = None) -> None:
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        try:
            self._write(msg)
        except McpError:
            pass  # 通知失败不致命：下次 request 的管道写入/读超时会暴露

    def request(self, method: str, params: dict | None, timeout: float) -> dict:
        if self._proc is None or self._proc.poll() is not None:
            raise McpError(f"MCP server '{self.name}' 未连接", "MCP_UNAVAILABLE")
        with self._id_lock:
            self._next_id += 1
            rid = self._next_id
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        self._write(msg)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise McpError(
                    f"MCP server '{self.name}' 请求超时（>{timeout:g}s）: {method}", "MCP_TIMEOUT"
                )
            try:
                resp = self._responses.get(timeout=remaining)
            except queue.Empty:
                continue  # get 超时 → 回到 deadline 复查（不吞超时语义）
            if resp is _EOF:
                raise McpError(
                    f"MCP server '{self.name}' 进程退出，{method} 无响应", "MCP_UNAVAILABLE"
                )
            if resp.get("id") != rid:
                continue  # 迟到/陈旧响应丢弃
            if "error" in resp:
                err = resp.get("error") or {}
                raise McpError(
                    f"MCP server '{self.name}' {method} 远端错误: {err.get('message', err)}",
                    "MCP_REMOTE_ERROR",
                )
            return resp.get("result") or {}

    # ── MCP 语义 ─────────────────────────────────────────────────────────

    def list_tools(self, timeout: float) -> list[dict]:
        """tools/list 分页取全。"""
        tools: list[dict] = []
        cursor = None
        while True:
            params = {"cursor": cursor} if cursor else {}
            result = self.request("tools/list", params, timeout)
            items = result.get("tools")
            if isinstance(items, list):
                tools.extend(t for t in items if isinstance(t, dict))
            cursor = result.get("nextCursor")
            if not cursor:
                return tools

    def call_tool(self, tool: str, args: dict, timeout: float) -> dict:
        """tools/call。返回原始 result（content 解析归 tools_plugin 契约层）。"""
        return self.request("tools/call", {"name": tool, "arguments": args}, timeout)

    def stop(self) -> None:
        proc, self._proc = self._proc, None
        self.status = "stopped"
        if proc is None:
            return
        try:
            proc.terminate()
            proc.wait(timeout=5)
        except Exception:  # noqa: BLE001 - 回收尽力而为，杀不掉强杀
            try:
                proc.kill()
            except Exception:  # noqa: BLE001
                pass


def content_to_text(result: dict) -> str:
    """tools/call result → 文本：content[] 中 text 项拼接；isError=true → McpError
    （工具级失败如实回喂，不伪装成功）；无 text 而 structuredContent 存在 → JSON 序列化。"""
    parts: list[str] = []
    content = result.get("content")
    if isinstance(content, list):
        for item in content:
            if not isinstance(item, dict):
                continue
            if item.get("type") == "text":
                parts.append(str(item.get("text", "")))
            else:
                parts.append(f"[不支持的内容类型: {item.get('type', 'unknown')}]")
    if result.get("isError"):
        raise McpError("\n".join(p for p in parts if p) or "MCP tool 执行失败（isError=true）", "MCP_TOOL_ERROR")
    if not any(p for p in parts) and isinstance(result.get("structuredContent"), dict):
        return json.dumps(result["structuredContent"], ensure_ascii=False)
    return "\n".join(p for p in parts if p)
