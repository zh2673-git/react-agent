"""bash 工具：命令执行 + 超时 + 截断 + 受限令牌沙箱（宿主层 fail-closed，05 §2.1）。

执行路径由宿主装配决定（SANDBOX_HELPER 指向 sandbox-run 助手则走沙箱）：
  沙箱路径：助手以 SAFER NORMALUSER 受限令牌执行（剥离高危特权、管理组 deny-only），
            助手缺失/令牌创建失败 → SANDBOX_FAILED 字段级错误，绝不回退无沙箱直跑；
  直跑路径：仅当 BASH_SANDBOX=off 显式豁免时（宿主不传 SANDBOX_HELPER），描述如实声明无沙箱。

W16 bash 文件追溯（B 方案，不禁写只留痕）：对「快照区」= WORKSPACE_ROOT −（噪音目录 /
.stream / 产物目录 / MEMORY_DATA_DIR / .git）执行前 pre-copy、执行后逐文件比对，变更
（新增/修改/删除）写 undo 快照并随结果返回 changes[]——agent-loop 转发为 op=bash 的
file_change 事件，与 write/edit 同一追溯/回滚链路（diff 双栏 + 回滚撤销全自动生效），
含核心区路径的间接写也被留痕可撤销。诚实边界：工作区外的 bash 改动不追踪；同
mtime+size 的内容替换不检测。`BASH_WRITE_TRACE=off` 可关闭快照。
"""

import base64
import json
import locale
import os
import subprocess
import uuid

from . import ToolError, optional_int, require, workspace_root
from .files import _NOISE_DIRS, _UNDO_MAX_BYTES, _snapshot_change

_MAX_TIMEOUT_MS = 60_000
_MAX_OUTPUT = 64 * 1024
# 快照区 pre-copy 内容缓存上限：总字节 / 文件数双闸（超限仍做 stat 清单比对，
# 但变更无 before 字节 → 无 undo 引用，diff 降级单侧、回滚跳过）
_SNAP_MAX_BYTES = 32 * 1024 * 1024
_SNAP_MAX_FILES = 4_000
_CHANGES_MAX = 50

# 助手退出码协议（sandbox-run）：0=已执行（stdout 单行 JSON）；2=用法；3=平台不支持；4=沙箱建立失败
_SANDBOX_HELPER = os.environ.get("SANDBOX_HELPER", "").strip()
_SANDBOXED = bool(_SANDBOX_HELPER)


def _trace_enabled() -> bool:
    return os.environ.get("BASH_WRITE_TRACE", "").strip().lower() != "off"


def _excluded_roots() -> list[str]:
    """快照区排除项（normcase 绝对路径前缀）：噪音目录 / 流目录 / 产物目录 / memory 数据目录。"""
    ws = os.path.normcase(str(workspace_root()))
    out = []
    for d in _NOISE_DIRS:
        out.append(os.path.normcase(os.path.join(ws, d)))
    stream = os.environ.get("AGENT_STREAM_DIR") or os.path.join(ws, ".stream")
    out.append(os.path.normcase(stream))
    out_dir = os.environ.get("AGENT_OUTPUT_DIR") or os.path.join(ws, "outputs")
    out.append(os.path.normcase(out_dir if os.path.isabs(out_dir) else os.path.join(ws, out_dir)))
    mem = os.environ.get("MEMORY_DATA_DIR") or os.path.join(ws, "plugins", "memory", "data")
    out.append(os.path.normcase(mem))
    return out


def _in_snapshot_zone(rel: str, excluded: list[str]) -> bool:
    """rel（'/' 分隔相对路径）是否属于快照区。"""
    real = os.path.normcase(os.path.join(str(workspace_root()), rel.replace("/", os.sep)))
    return not any(real == e or real.startswith(e + os.sep) for e in excluded)


def _scan_zone(with_content: bool = True) -> tuple[dict[str, tuple[int, int]], dict[str, bytes] | None]:
    """遍历快照区：返回 (stat 清单 rel->(mtime_ns,size), 内容缓存或 None=超限/未请求)。"""
    ws = str(workspace_root())
    excluded = _excluded_roots()
    manifest: dict[str, tuple[int, int]] = {}
    content: dict[str, bytes] | None = {} if with_content else None
    total = 0
    for dirpath, dirnames, filenames in os.walk(ws):
        dirnames[:] = [d for d in dirnames if d not in _NOISE_DIRS and not d.startswith(".")]
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, ws).replace("\\", "/")
            if not _in_snapshot_zone(rel, excluded):
                continue
            try:
                st = os.stat(full)
            except OSError:
                continue
            manifest[rel] = (st.st_mtime_ns, st.st_size)
            if content is not None:
                if total + st.st_size > _SNAP_MAX_BYTES or len(content) >= _SNAP_MAX_FILES:
                    content = None  # 超限：放弃内容缓存，退化为 stat 清单比对
                else:
                    try:
                        with open(full, "rb") as f:
                            b = f.read()
                    except OSError:
                        b = None
                    if b is not None:
                        content[rel] = b
                        total += len(b)
    return manifest, content


def _diff_zone(
    before_m: dict[str, tuple[int, int]], content: dict[str, bytes] | None
) -> tuple[list[dict], bool]:
    """执行后比对：返回 (changes, truncated)。change = {path, kind, undo?}。"""
    ws = str(workspace_root())
    after_m, _ = _scan_zone(with_content=False)  # 二次遍历（仅 stat 清单）
    changes: list[dict] = []
    truncated = False

    seen = set(after_m)
    for rel in after_m:
        if rel not in before_m:
            kind = "create"
        elif before_m[rel] != after_m[rel]:
            kind = "modify"
        else:
            continue
        changes.append({"path": rel, "kind": kind})
    for rel in before_m:
        if rel not in seen:
            changes.append({"path": rel, "kind": "delete"})

    # 逐条落 undo 快照（kind 判定后读现盘字节；stat 未变的内容替换不检测——诚实边界）
    out: list[dict] = []
    for ch in changes:
        if len(out) >= _CHANGES_MAX:
            truncated = True
            break
        rel, kind = ch["path"], ch["kind"]
        before_b = content.get(rel) if content is not None else None
        item: dict = {"path": rel, "kind": kind}
        if kind == "delete":
            if before_b is not None:
                if (snap := _snapshot_change(before_b, None)) is not None:
                    item["undo"] = snap
            else:
                item["note"] = "文件过大未快照，删除不可自动恢复"
        elif kind == "create":
            try:
                after_b = open(os.path.join(ws, rel.replace("/", os.sep)), "rb").read()
            except OSError:
                after_b = None
            if after_b is not None and len(after_b) <= _UNDO_MAX_BYTES:
                if (snap := _snapshot_change(None, after_b)) is not None:
                    item["undo"] = snap
        else:  # modify
            try:
                after_b = open(os.path.join(ws, rel.replace("/", os.sep)), "rb").read()
            except OSError:
                after_b = None
            if before_b is not None and after_b is not None and before_b == after_b:
                continue  # stat 变了但内容没变（touch）：不登记
            if (
                before_b is not None
                and after_b is not None
                and len(before_b) <= _UNDO_MAX_BYTES
                and len(after_b) <= _UNDO_MAX_BYTES
            ):
                if (snap := _snapshot_change(before_b, after_b)) is not None:
                    item["undo"] = snap
        out.append(item)
    return out, truncated


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
    total = len(output.encode("utf-8"))
    truncated = total > _MAX_OUTPUT
    if truncated:
        # head+tail 双保留：报错信息多在尾部，只留头部会丢关键线索（v3）
        eb = output.encode("utf-8")
        head = eb[:40_000].decode("utf-8", errors="ignore")
        tail = eb[-20_000:].decode("utf-8", errors="ignore")
        output = head + f"\n…[中段略去约 {(total - 60_000) // 1024}KB，保留头部 40KB + 尾部 20KB]…\n" + tail
    return {"exit_code": exit_code, "timeout": timeout, "truncated": truncated, "output": output}


def _run_bash(args: dict) -> dict:
    command = require(args, "command")
    timeout_ms = optional_int(args, "timeout_ms", 30_000, 1_000, _MAX_TIMEOUT_MS)
    before = _scan_zone() if _trace_enabled() else None  # 执行前快照（pre-copy 内容缓存）
    result = _run_sandboxed(command, timeout_ms) if _SANDBOXED else _run_direct(command, timeout_ms)
    if before is not None:
        try:
            changes, cut = _diff_zone(*before)
            if changes or cut:
                result["changes"] = changes
                if cut:
                    result["changes_truncated"] = True
        except OSError:
            pass  # 追溯失败不影响命令结果本身
    return result


_DESCRIPTION = (
    "Run a shell command in the workspace (cwd = WORKSPACE_ROOT) via a restricted-token sandbox helper "
        "(reduced privileges, admin groups deny-only; fail-closed). Large output keeps head 40KB + tail 20KB. "
        "Args: command (required), timeout_ms (default 30000, max 60000)."
        if _SANDBOXED
        else (
            "Run a shell command in the workspace (cwd = WORKSPACE_ROOT). "
            "Runs with full user permissions - NOT sandboxed (BASH_SANDBOX=off). "
            "Large output keeps head 40KB + tail 20KB. "
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
        # R16 观测胸牌：输出文本扫描产物 + changes[] 快照区变更展开（W16 追溯半）
        "obs": {"artifacts": "output_text", "changes": {"op": "bash", "from": "change_list"}},
        "run": _run_bash,
    },
}
