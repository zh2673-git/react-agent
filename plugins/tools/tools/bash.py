"""bash 工具：命令执行 + 超时 + 截断 + 受限令牌沙箱（宿主层 fail-closed，05 §2.1）。

执行路径由宿主装配决定（SANDBOX_HELPER 指向 sandbox-run 助手则走沙箱）：
  沙箱路径：助手以 SAFER NORMALUSER 受限令牌执行（剥离高危特权、管理组 deny-only），
            助手缺失/令牌创建失败 → SANDBOX_FAILED 字段级错误，绝不回退无沙箱直跑；
  直跑路径：仅当 BASH_SANDBOX=off 显式豁免时（宿主不传 SANDBOX_HELPER），描述如实声明无沙箱。

cwd 归一（rsi-agent 快照适配第 3 处）：子进程 cwd 一律走 `_shell_cwd()`——剥 `\\?\` 扩展长度
前缀再校验可用性，不可用即 fail-closed。原因：宿主 instances.rs 用 `canonicalize()` 写
WORKSPACE_ROOT 会带该前缀，而 Windows cmd.exe 拒绝 UNC 形态并**静默改在 C:\Windows 执行且
exit=0**（错目录 + 越界写 + 追溯失效三重危害）。

N3 输出解码：沙箱/直跑两侧统一 `_decode_output()`——UTF-8 优先，非法字节段以
反斜杠十六进制（backslash-x-ff 形态）转义保留（原样可读，零信息丢失），不再按
locale（中文 Windows 缺省 GBK）解码。模型无需再垫 `chcp 65001` 仪式；GBK 输出
会以混合形态呈现（合法 UTF-8 序列正常解码，罕见重合序列呈现为可读字符——诚实
边界，不影响命令执行与退出码判定）。

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
# 惰性读取（不再 import 期固化）：spawn 期 env 已固定，行为不变；可测试注入。
def _sandbox_helper() -> str:
    return os.environ.get("SANDBOX_HELPER", "").strip()


# 沙箱描述随调用期助手存在性变化（不 import 期固化：宿主装配与运行期 env 可能不同）
def _sandboxed() -> bool:
    return bool(_sandbox_helper())

def _shell_cwd() -> str:
    """shell 子进程 cwd：WORKSPACE_ROOT 的「shell 可用」形态，fail-closed。

    Windows `cmd.exe` 不认 `\\\\?\\` 扩展长度前缀（会打印
    「UNC paths are not supported. Defaulting to Windows directory」并以
    **exit=0 在 C:\\Windows 执行**——静默错目录 + 越界写 + 追溯失效）。
    宿主 instances.rs 用 `canonicalize()` 写 WORKSPACE_ROOT 正好产出该前缀，
    故在执行点归一：剥前缀、分隔符统一，再校验可用性；不可用即报错，
    **绝不回落到系统目录**（工具声明的 cwd=WORKSPACE_ROOT 是硬契约）。
    """
    ws = str(workspace_root())
    if ws.startswith("\\\\?\\UNC\\"):  # UNC 的扩展形态 → 普通 UNC
        ws = "\\\\" + ws[len("\\\\?\\UNC\\"):]
    elif ws.startswith("\\\\?\\"):
        ws = ws[len("\\\\?\\"):]
    ws = ws.replace("/", os.sep)
    if not os.path.isdir(ws):
        raise ToolError(
            f"工作区不可作为 shell cwd: WORKSPACE_ROOT='{ws}' 不存在或不是目录。"
            "bash 工具不会回落到系统目录执行（fail-closed）；请检查 WORKSPACE_ROOT 配置。",
            code="BAD_WORKSPACE",
        )
    return ws


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
            timeout=timeout_ms / 1000,
            cwd=_shell_cwd(),
        )
    except subprocess.TimeoutExpired:
        return _timeout_result(timeout_ms)
    # N3：不再用 text=True（locale 解码，中文 Windows 下 UTF-8 输出必乱码），
    # 与沙箱路径同走 _decode_output（UTF-8 优先 + 非法字节转义）。
    output = _decode_output((proc.stdout or b"") + (proc.stderr or b""))
    return _result(proc.returncode, False, output)


def _run_sandboxed(command: str, timeout_ms: int) -> dict:
    helper = _sandbox_helper()
    if not helper:
        raise ToolError(
            "SANDBOX_HELPER 为空（沙箱模式已启用但助手路径缺失）；fail-closed，"
            "不回退无沙箱执行；如接受直跑请设 BASH_SANDBOX=off",
            code="SANDBOX_FAILED",
        )
    try:
        proc = subprocess.run(
            [helper, "exec", str(timeout_ms), command],
            capture_output=True,  # 助手结果走 stdout 单行 JSON；错误细节走 stderr
            timeout=timeout_ms / 1000 + 10.0,  # 助手自身挂死兜底（超时+10s 余量）
            cwd=_shell_cwd(),
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
    output = _decode_output(base64.b64decode(data.get("output_b64", "")))
    return _result(data.get("exit_code"), bool(data.get("timeout")), output)


def _timeout_result(timeout_ms: int) -> dict:
    return {
        "exit_code": None,
        "timeout": True,
        "truncated": False,
        "output": f"[timeout] 命令超过 {timeout_ms}ms 被终止。如需更长时间请分步执行或调大 timeout_ms（上限 {_MAX_TIMEOUT_MS}ms）",
    }


def _decode_output(raw: bytes) -> str:
    """N3 输出解码：UTF-8 优先，非法字节以转义序列保留（零信息丢失）。

    旧逻辑按 locale.getpreferredencoding(False) 解码——中文 Windows 缺省 GBK，
    工具链（git/python/rust）普遍吐 UTF-8 → 每次中文输出都是乱码，模型被迫
    在命令前垫 `chcp 65001` 仪式（实测 46 会话 62 次）。新语义：strict UTF-8
    成功即用；失败则**按首字节定长**逐段扫描——ASCII（<0x80）直出；首字节
    C2-DF / E0-EF / F0-F4 分别按 2/3/4 字节成段试解码；首字节非法、或段解码
    失败（含尾部截断）只转义当前字节再前进一个。任何分支指针必推进 ≥1：
    不死循环，也不会把失败段之后的合法字节卷进失败窗口（定长窗扫法两病
    皆犯——尾部 ASCII 被转义——由回归用例抓住后改为本算法）。诚实边界：
    GBK 双字节几乎总不是合法 UTF-8（如「你」= C4 E3，E3 不可作续字节），
    会以 backslash-x-NN 转义呈现——不假装解码成功，模型可按转义反推原始编码。
    """
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        pass
    out: list[str] = []
    i, n = 0, len(raw)
    while i < n:
        lead = raw[i]
        if lead < 0x80:
            out.append(chr(lead))
            i += 1
            continue
        if 0xC2 <= lead <= 0xDF:
            expect = 2
        elif 0xE0 <= lead <= 0xEF:
            expect = 3
        elif 0xF0 <= lead <= 0xF4:
            expect = 4
        else:
            out.append("\\x%02x" % lead)
            i += 1
            continue
        chunk = raw[i : i + expect]
        try:
            out.append(chunk.decode("utf-8"))
            i += expect
        except UnicodeDecodeError:
            out.append("\\x%02x" % lead)
            i += 1
    return "".join(out)


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
    # cmd.exe /c 只执行首行（换行即命令终止，且不尊重引号）：多行命令的其余行会被
    # **静默丢弃且 exit_code 仍为 0**。静默截断比报错危险得多——模型会基于虚假成功
    # 继续推理。故此处 fail-loud，把问题挡在入口。
    if "\n" in command or "\r" in command:
        raise ToolError(
            "bash command 含换行符：cmd.exe /c 只执行首行，其余行会被静默丢弃（exit_code 仍为 0）。"
            "请改用单行 + `&&` / `&` 串联，或拆成多次调用；python 多语句用 `;` 分隔。",
            code="MULTILINE_UNSUPPORTED",
            field="command",
        )
    timeout_ms = optional_int(args, "timeout_ms", 30_000, 1_000, _MAX_TIMEOUT_MS)
    before = _scan_zone() if _trace_enabled() else None  # 执行前快照（pre-copy 内容缓存）
    result = _run_sandboxed(command, timeout_ms) if _sandboxed() else _run_direct(command, timeout_ms)
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


def _description() -> str:
    """工具描述：沙箱有无随调用期 env 定形（诚实声明，不 import 期固化）。"""
    return (
        "Run a shell command in the workspace (cwd = WORKSPACE_ROOT) via a restricted-token sandbox helper "
        "(reduced privileges, admin groups deny-only; fail-closed). Large output keeps head 40KB + tail 20KB. "
        "Args: command (required), timeout_ms (default 30000, max 60000). "
        "IMPORTANT: command must be a SINGLE line - a newline makes cmd.exe /c run only the first line "
        "and silently drop the rest with exit_code=0; chain steps with && or & instead. "
        "This tool is for running/building/testing, git, process and port queries - do NOT use it for "
        "file operations (reading, writing, editing, searching, listing files); use the dedicated tools "
        "(read_file/write_file/edit_file/grep/list_dir/symbols_search) instead."
        if _sandboxed()
        else (
            "Run a shell command in the workspace (cwd = WORKSPACE_ROOT). "
            "Runs with full user permissions - NOT sandboxed (BASH_SANDBOX=off). "
            "Large output keeps head 40KB + tail 20KB. "
            "Args: command (required), timeout_ms (default 30000, max 60000). "
            "IMPORTANT: command must be a SINGLE line - a newline makes cmd.exe /c run only the first line "
            "and silently drop the rest with exit_code=0; chain steps with && or & instead. "
            "Output is decoded as UTF-8 (non-UTF-8 bytes appear as backslash-x escapes) - do NOT prefix "
            "commands with chcp 65001 or code-page env vars; they are unnecessary. "
            "This tool is for running/building/testing, git, process and port queries - do NOT use it for "
            "file operations (reading, writing, editing, searching, listing files); use the dedicated tools "
            "(read_file/write_file/edit_file/grep/list_dir/symbols_search) instead."
        )
    )


TOOLS = {
    "bash": {
        "description": _description(),
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
