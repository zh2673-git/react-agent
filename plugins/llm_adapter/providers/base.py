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
import time

import json


def err(message: str, code: str = "LLM_ERROR") -> dict:
    return {"ok": False, "error": {"code": code, "message": message}}


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
