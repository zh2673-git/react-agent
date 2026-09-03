"""bash 工具：命令执行 + 超时 + 截断 + 受限令牌沙箱（宿主层 fail-closed，05 §2.1）。

执行路径由宿主装配决定（SANDBOX_HELPER 指向 sandbox-run 助手则走沙箱）：
  沙箱路径：助手以 SAFER NORMALUSER 受限令牌执行（剥离高危特权、管理组 deny-only），
            助手缺失/令牌创建失败 → SANDBOX_FAILED 字段级错误，绝不回退无沙箱直跑；
  直跑路径：仅当 BASH_SANDBOX=off 显式豁免时（宿主不传 SANDBOX_HELPER），描述如实声明无沙箱。
"""

import base64
import json
import locale
import os
import subprocess

from . import ToolError, optional_int, require, workspace_root

_MAX_TIMEOUT_MS = 60_000
_MAX_OUTPUT = 64 * 1024

# 助手退出码协议（sandbox-run）：0=已执行（stdout 单行 JSON）；2=用法；3=平台不支持；4=沙箱建立失败
_SANDBOX_HELPER = os.environ.get("SANDBOX_HELPER", "").strip()
_SANDBOXED = bool(_SANDBOX_HELPER)


def _run_direct(command: str, timeout_ms: int) -> dict:
    try:
        proc = subprocess.run(  # noqa: S602 - 仅 BASH_SANDBOX=off 显式豁免时可达
            command,
            shell=True,
            capture_output=True,
            text=True,
            timeout=timeout_ms / 1000,
            cwd=str(workspace_root()),
        )
    except subprocess.TimeoutExpired:
        return _timeout_result(timeout_ms)
    output = (proc.stdout or "") + (proc.stderr or "")
    return _result(proc.returncode, False, output)


def _run_sandboxed(command: str, timeout_ms: int) -> dict:
    try:
        proc = subprocess.run(
            [_SANDBOX_HELPER, "exec", str(timeout_ms), command],
            capture_output=True,  # 助手结果走 stdout 单行 JSON；错误细节走 stderr
            timeout=timeout_ms / 1000 + 10.0,  # 助手自身挂死兜底（超时+10s 余量）
            cwd=str(workspace_root()),
        )
    except subprocess.TimeoutExpired:
        # fail-closed：助手超限挂死视为沙箱边界不可信，不回退直跑
        return _timeout_result(timeout_ms + 10_000)
    except OSError as exc:
        raise ToolError(
            f"沙箱助手不可用: {exc}（fail-closed，不回退无沙箱执行；如接受直跑请设 BASH_SANDBOX=off）",
            code="SANDBOX_FAILED",
        )
    if proc.returncode != 0:
        detail = (proc.stderr or "").decode(errors="replace").strip().splitlines()
        raise ToolError(
            f"沙箱建立失败（助手 exit={proc.returncode}）: {detail[-1] if detail else '未知原因'}。"
            "fail-closed，不回退无沙箱执行；如接受直跑请设 BASH_SANDBOX=off。",
            code="SANDBOX_FAILED",
        )
    try:
        data = json.loads(proc.stdout.decode(errors="replace"))
    except (ValueError, UnicodeDecodeError) as exc:
        raise ToolError(f"沙箱助手返回不可解析: {exc}", code="SANDBOX_FAILED")
    output = base64.b64decode(data.get("output_b64", "")).decode(locale.getpreferredencoding(False), errors="replace")
    return _result(data.get("exit_code"), bool(data.get("timeout")), output)


def _timeout_result(timeout_ms: int) -> dict:
    return {
        "exit_code": None,
        "timeout": True,
        "truncated": False,
        "output": f"[timeout] 命令超过 {timeout_ms}ms 被终止。如需更长时间请分步执行或调大 timeout_ms（上限 {_MAX_TIMEOUT_MS}ms）",
    }


def _result(exit_code, timeout: bool, output: str) -> dict:
    truncated = len(output.encode("utf-8")) > _MAX_OUTPUT
    if truncated:
        output = output.encode("utf-8")[:_MAX_OUTPUT].decode("utf-8", errors="ignore") + "\n[truncated: 输出超过 64KB]"
    return {"exit_code": exit_code, "timeout": timeout, "truncated": truncated, "output": output}


def _run_bash(args: dict) -> dict:
    command = require(args, "command")
    timeout_ms = optional_int(args, "timeout_ms", 30_000, 1_000, _MAX_TIMEOUT_MS)
    if _SANDBOXED:
        return _run_sandboxed(command, timeout_ms)
    return _run_direct(command, timeout_ms)


_DESCRIPTION = (
    "Run a shell command in the workspace (cwd = WORKSPACE_ROOT) via a restricted-token sandbox helper "
    "(reduced privileges, admin groups deny-only; fail-closed). "
    "Args: command (required), timeout_ms (default 30000, max 60000)."
    if _SANDBOXED
    else (
        "Run a shell command in the workspace (cwd = WORKSPACE_ROOT). "
        "Runs with full user permissions - NOT sandboxed (BASH_SANDBOX=off). "
        "Args: command (required), timeout_ms (default 30000, max 60000)."
    )
)

TOOLS = {
    "bash": {
        "description": _DESCRIPTION,
        "parameters": {
            "type": "object",
            "properties": {"command": {"type": "string"}, "timeout_ms": {"type": "integer"}},
            "required": ["command"],
        },
        "run": _run_bash,
    },
}
