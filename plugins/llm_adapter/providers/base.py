"""provider pack 协议（规则层：唯一被 registry 认可的形状）。

每个 provider 模块必须暴露模块级常量 PROVIDER：

    PROVIDER = {
        "name": str,                     # 与 llm.chat 请求 payload["provider"] / LLM_PROVIDER 对应
        "chat": Callable[[dict], dict],  # payload -> 归一化响应（见下），异常一律抛出由分派层兜底
        "requires_env": list[str],       # 缺失时 chat 内自行报错；此处仅作文档
    }

归一化响应契约（线契约，见 03 §2.2，既有字段逐字不变）：
    {"ok": True,  "content": str|null, "tool_calls": [{"id","name","arguments":object}],
     "model": str, "finish_reason": str}
    {"ok": False, "error": {"code": str, "message": str}}

追加可选字段（缺省 = provider 未提供，旧消费者无感知，故不破坏冻结契约）：
    "reasoning": str|null   思考内容（DeepSeek reasoning_content 等）
    "usage": {"input_tokens","output_tokens","cache_read_tokens","reasoning_tokens"}
    "elapsed_ms": int       本次请求墙钟耗时

流式旁路（guest 协议是 unary gRPC，插件无反向推事件通道，故走文件旁路）：
    payload 携带 "stream_path"（绝对路径）+ "sid" 时，provider 在生成过程中把增量
    写往该文件（StreamSink），宿主 tail 后经 SSE 推送前端；不携带则行为与改造前一致。
"""

import json
import os
import re
import threading
import time


# ── 取消（R1 停止失效修复）──────────────────────────────────────────────
# host cancel 端点在 agent-loop cancel（轮次边界收敛）之外，向本插件并行 dispatch
# `abort` op——流式循环每帧检查命中即关流返回 K499，单轮长生成无需等轮次边界。
# 进程级注册表：session_id → 取消请求时间戳(ms)。线程安全由 _ABORTS_LOCK 保证。
#
# 时间戳语义（误伤防护）：流式开始时记录 start_ts，仅「时间戳 >= start_ts」的取消
# 命中——取消早于本轮流式开始属陈旧信号（轮次边界检查已覆盖），流式启动时消费掉、
# 不中断本轮。命中即消费，防残留误杀同会话下轮对话。
_ABORTS: dict[str, int] = {}
_ABORTS_LOCK = threading.Lock()

# sid 形如 "{session_id}-r{round}"（agent-loop chat_run 逐轮编号）。
# 贪婪取最后一次 "-r<数字>" 前缀，session_id 自身含 "-r<数字>" 时仍还原正确。
_SID_RE = re.compile(r"^(.*)-r\d+$")


def sid_session(sid: str | None) -> str | None:
    """流式 sid → 会话 id；无法解析（无 sid/无 -r 后缀）返回 None（不参与取消检查）。"""
    if not sid or not isinstance(sid, str):
        return None
    m = _SID_RE.match(sid)
    return m.group(1) if m else None


def request_abort(session_id: str) -> None:
    """置位取消（llm_plugin abort op 入口；重复置位刷新时间戳）。"""
    with _ABORTS_LOCK:
        _ABORTS[session_id] = int(time.time() * 1000)


def stream_checkpoint(sid: str | None) -> int:
    """流式启动检查点：记录本轮 start_ts 并消费陈旧取消信号（早于本轮开始的
    取消属上一轮遗留——轮次边界检查已覆盖，不得误杀本轮），返回 start_ts 供
    逐帧 `abort_requested(sid, start_ts)` 对位。"""
    start_ts = int(time.time() * 1000)
    session = sid_session(sid)
    if session is not None:
        with _ABORTS_LOCK:
            ts = _ABORTS.get(session)
            if ts is not None and ts < start_ts:
                _ABORTS.pop(session, None)
    return start_ts


def abort_requested(sid: str | None, start_ts: int) -> bool:
    """流式循环逐帧调用：本会话存在「晚于本轮开始」的取消请求 → True（调用方应立即关流收敛）。"""
    session = sid_session(sid)
    if session is None:
        return False
    with _ABORTS_LOCK:
        ts = _ABORTS.get(session)
        return ts is not None and ts >= start_ts


def consume_abort(sid: str | None) -> None:
    """命中取消后消费信号（防残留误杀同会话后续对话）。"""
    session = sid_session(sid)
    if session is not None:
        with _ABORTS_LOCK:
            _ABORTS.pop(session, None)


def err(message: str, code: str = "LLM_ERROR") -> dict:
    return {"ok": False, "error": {"code": code, "message": message}}


# 「上下文超限」类失败特征（L1/R8）：确定性但可行动——agent-loop 据此降级重试而非直接失败。
_OVERFLOW_MARKERS = (
    "exceed_context_size_error",           # ollama 结构化 code
    "exceeds the available context size",  # ollama 文案
    "context_length_exceeded",             # OpenAI 兼容族结构化 code
    "maximum context length",              # OpenAI 兼容族文案
    "prompt is too long",                  # Anthropic
)


def _is_context_overflow(resp_text: str) -> bool:
    low = resp_text.lower()
    return any(m in low for m in _OVERFLOW_MARKERS)


# ollama 前缀截断连锁（L2）：输入超窗口时服务端从前往后截断（system/user 先被丢弃），
# 报 500「no user query found in messages」——文案指向「缺 user 消息」，真实原因是窗口装不下。
# 该文案是 ollama 特有形态，仅对 ollama + 精确匹配放行 500（避免 OpenAI 500 误判泛滥）。
_OLLAMA_TRUNCATION_MARKER = "no user query found in messages"


def err_from_resp(provider: str, status: int, resp_text: str) -> dict:
    """HTTP ≥ 400 响应 → 归一化错误 payload（L1/R8，归属 adapter：形状归一化的错误侧对齐）。

    命中「上下文超限」类（ollama exceed_context_size_error / OpenAI context_length_exceeded /
    Anthropic prompt is too long 等）→ code=CONTEXT_OVERFLOW（+ limit/required 元数据，尽力而为）；
    另对 ollama 放行一条 500 特例（前缀截断连锁，L2）→ 同归 CONTEXT_OVERFLOW；
    未命中 → 维持既有形状原文透传（code=LLM_ERROR），消费方行为不变。
    """
    message = f"{provider} HTTP {status}: {resp_text[:500]}"
    overflow = status in (400, 413) and _is_context_overflow(resp_text)
    if not overflow and provider == "ollama" and status == 500 and _OLLAMA_TRUNCATION_MARKER in resp_text.lower():
        overflow = True
    if overflow:
        error = {"code": "CONTEXT_OVERFLOW", "message": message}
        ctx = _extract_ctx(resp_text)
        if ctx:
            error["ctx"] = ctx
        return {"ok": False, "error": error}
    return err(message)


def _extract_ctx(resp_text: str) -> dict:
    """尽力提取窗口元数据（provider 给了才带，不做猜测）。

    ollama /v1 的错误 message 内还嵌套一层 JSON 字符串（引号带反斜杠转义），
    故数字模式须兼容 `\"n_ctx\":4096` 形态。
    """
    ctx: dict = {}
    low = resp_text.lower()
    m = re.search(r'n_ctx\\?"?\s*[:=]\s*(\d+)', resp_text) or re.search(r"maximum context length is (\d+)", low)
    if m:
        ctx["limit"] = int(m.group(1))
    m = (
        re.search(r'n_prompt_tokens\\?"?\s*[:=]\s*(\d+)', resp_text)
        or re.search(r"you requested (\d+) tokens", low)
        or re.search(r"too long:\s*(\d+)\s*tokens", low)
    )
    if m:
        ctx["required"] = int(m.group(1))
    return ctx


def norm(content, calls, model, finish_reason, reasoning=None, usage=None, elapsed_ms=None) -> dict:
    """归一化成功响应。

    reasoning / usage / elapsed_ms 为追加的可选字段：按「有才写」处理，既有四字段
    与线契约逐字一致，未升级的消费者行为不变。
    """
    out = {"ok": True, "content": content, "tool_calls": calls, "model": model, "finish_reason": finish_reason}
    if reasoning:
        out["reasoning"] = reasoning
    if usage:
        out["usage"] = usage
    if elapsed_ms is not None:
        out["elapsed_ms"] = elapsed_ms
    return out


def map_usage(u: dict | None) -> dict:
    """OpenAI 兼容 usage → 互斥不相交计数（与 deepseek-harness 的 mapUsage 同约定）。

    上游 `prompt_tokens` 含缓存命中，故 input = prompt - cached，避免重复计数。
    """
    u = u if isinstance(u, dict) else {}
    prompt = u.get("prompt_tokens") or 0
    completion = u.get("completion_tokens") or 0
    details_in = u.get("prompt_tokens_details") if isinstance(u.get("prompt_tokens_details"), dict) else {}
    details_out = u.get("completion_tokens_details") if isinstance(u.get("completion_tokens_details"), dict) else {}
    cached = details_in.get("cached_tokens") or u.get("prompt_cache_hit_tokens") or 0
    reasoning = details_out.get("reasoning_tokens") or 0
    return {
        "input_tokens": max(int(prompt) - int(cached), 0),
        "output_tokens": int(completion),
        "cache_read_tokens": int(cached),
        "reasoning_tokens": int(reasoning),
    }


def as_object(args) -> dict:
    """OpenAI 兼容端点的 arguments 是 JSON 字符串，Anthropic 是对象——统一为对象。"""
    if isinstance(args, str):
        try:
            return json.loads(args)
        except json.JSONDecodeError:
            return {"_raw": args}
    return args if isinstance(args, dict) else {}


def require_httpx():
    try:
        import httpx  # noqa: F401
    except ImportError as exc:
        raise RuntimeError("httpx 未安装（pip install httpx），该 provider 不可用") from exc


class StreamSink:
    """流式旁路写入器：把生成增量按 JSONL 追加到宿主可读的旁路文件。

    行协议（宿主 tail 后原样转 SSE 事件）：
        {"type":"start",  "sid":.., "ts":..}
        {"type":"delta",  "sid":.., "ts":.., "kind":"reasoning"|"text", "text":".."}
        {"type":"end",    "sid":.., "ts":.., "usage":{..}, "elapsed_ms":..}
        {"type":"error",  "sid":.., "ts":.., "message":".."}

    `start` 以 "w" 打开（新一轮覆盖旧内容——历史轮次的最终内容已落 memory 日志，
    旁路只服务「边生成边看」，不参与持久化与刷新恢复）。
    flush 按 50ms 攒批：实测单条 append+flush ≈ 43µs，相对 LLM 20~100ms/token 可忽略，
    攒批仅为压掉高频小写的系统调用。
    """

    FLUSH_INTERVAL = 0.05

    def __init__(self, path: str | None, sid: str | None = None):
        self.path = path or None
        self.sid = sid
        self._fh = None
        self._last_flush = 0.0

    # -- 生命周期 ----------------------------------------------------------
    def start(self) -> None:
        if not self.path:
            return
        try:
            d = os.path.dirname(self.path)
            if d:
                os.makedirs(d, exist_ok=True)
            self._fh = open(self.path, "w", encoding="utf-8")
        except OSError:
            self._fh = None  # 旁路失败不得影响主链路
            return
        self._write({"type": "start"}, force=True)

    def close(self) -> None:
        if self._fh is None:
            return
        try:
            self._fh.flush()
        finally:
            try:
                self._fh.close()
            except OSError:
                pass
            self._fh = None

    # -- 事件 --------------------------------------------------------------
    def delta(self, kind: str, text: str) -> None:
        if self._fh is None or not text:
            return
        self._write({"type": "delta", "kind": kind, "text": text})

    def end(self, usage: dict | None, elapsed_ms: int) -> None:
        if self._fh is None:
            return
        self._write({"type": "end", "usage": usage, "elapsed_ms": elapsed_ms}, force=True)

    def error(self, message: str) -> None:
        if self._fh is None:
            return
        self._write({"type": "error", "message": message}, force=True)

    # -- 内部 --------------------------------------------------------------
    def _write(self, obj: dict, force: bool = False) -> None:
        obj["sid"] = self.sid
        obj["ts"] = int(time.time() * 1000)
        try:
            self._fh.write(json.dumps(obj, ensure_ascii=False) + "\n")
        except (OSError, TypeError, ValueError):
            return
        now = time.monotonic()
        if force or now - self._last_flush >= self.FLUSH_INTERVAL:
            try:
                self._fh.flush()
            except OSError:
                pass
            self._last_flush = now
