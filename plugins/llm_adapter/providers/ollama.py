"""ollama provider：原生 /api/chat 传输（L2+L3，PLAN「方案（L2+L3）」）。

/v1 兼容层两大语义损失（运行时观测 2026-09-06）：
- 无法 per-request 传 num_ctx——P7 的本地窗口估算与服务端真实窗口脱节；
- 输入超窗口时服务端**从前往后截断**（system/user 先被丢弃），报
  `HTTP 500 "no user query found in messages"`——文案指向「缺 user 消息」，
  真实原因是窗口装不下，且 500 形态连错误归一化的状态码闸都接不住。

原生端点解法：`options.num_ctx` per-request 控制窗口（治本），错误统一
`{"error": ...}` 形态 + `prompt_eval_count`/`eval_count` 统计（归一化友好）。
流式为 NDJSON 行协议（每行一 JSON，末行 `done:true` 附统计），比 SSE 更简单。

OLLAMA_ENDPOINT = native（缺省）| v1（回退旧行为：复用 openai_compat 共享实现）。

已知限制（PLAN 记录在案）：
- native 无 tool_call_id，tool 消息按 tool_name 对位（从配对 assistant 反查；
  同名多调用对位精度下降）；
- 思考链：推理模型的 `message.thinking` 已入契约（非流式 → reasoning 字段；
  流式 → sink.delta("reasoning")），前端「思考过程」块依赖此通道。
"""

import json
import os
import time

from .base import (
    StreamSink,
    abort_requested,
    as_object,
    consume_abort,
    err,
    err_from_resp,
    norm,
    require_httpx,
    stream_checkpoint,
)
from .openai_compat import _chat_with


def resolve_ollama() -> dict:
    base = os.environ.get("OLLAMA_HOST", "localhost:11434").rstrip("/")
    return {
        "base_url": f"http://{base}/v1",        # v1 回退通道
        "api_url": f"http://{base}/api/chat",   # native 端点
        "model": os.environ.get("LLM_MODEL", "qwen2.5:7b"),
        "headers": {},
    }


def endpoint_mode() -> str:
    return os.environ.get("OLLAMA_ENDPOINT", "native").strip().lower() or "native"


# ── 归一化形状 → native 请求映射 ─────────────────────────────────────────

def to_native_messages(msgs: list) -> list:
    """归一化 messages → native messages。

    tool 消息按 tool_name 对位：从配对 assistant 的 tool_calls 反查 id → name；
    反查不到（孤儿）置空串。assistant.tool_calls 的 arguments 归一化侧已是 object，直接放。
    R3：图片附件 → `images` 数组（ollama 要求裸 base64，与 data_b64 约定一致）。
    """
    out: list = []
    id2name: dict = {}
    for m in msgs:
        role = m.get("role", "user")
        if role == "assistant":
            for tc in m.get("tool_calls") or []:
                if tc.get("id"):
                    id2name[tc["id"]] = tc.get("name", "")
            entry: dict = {"role": "assistant", "content": m.get("content")}
            if m.get("tool_calls"):
                entry["tool_calls"] = [
                    {"function": {"name": tc["name"], "arguments": tc.get("arguments") or {}}}
                    for tc in m["tool_calls"]
                ]
            out.append(entry)
        elif role == "tool":
            out.append({"role": "tool", "content": m.get("content"), "tool_name": id2name.get(m.get("tool_call_id") or "", "")})
        else:
            entry = {"role": role, "content": m.get("content")}
            images = [
                a["data_b64"]
                for a in (m.get("attachments") or [])
                if isinstance(a, dict) and a.get("data_b64")
            ]
            if images:
                entry["images"] = images
            out.append(entry)
    return out


def _build_body(endpoint: dict, payload: dict, stream: bool) -> dict:
    body: dict = {
        "model": endpoint["model"],
        "messages": to_native_messages(payload.get("messages", [])),
        "stream": stream,
    }
    tools = payload.get("tools")
    if tools:
        body["tools"] = [
            {"type": "function", "function": {"name": t["name"], "description": t.get("description", ""), "parameters": t.get("parameters", {})}}
            for t in tools
        ]
    # L3：统一窗口透传——payload.num_ctx（agent-loop 从 LLM_CONTEXT_TOKENS 下发，0/缺省不带）
    num_ctx = payload.get("num_ctx")
    if isinstance(num_ctx, int) and not isinstance(num_ctx, bool) and num_ctx > 0:
        body["options"] = {"num_ctx": num_ctx}
    return body


# ── native 响应 → 归一化形状映射 ─────────────────────────────────────────

def map_native_usage(data: dict) -> dict | None:
    """native 统计 → 互斥计数（prompt_eval_count / eval_count；无缓存与思考分项，置 0）。"""
    prompt = data.get("prompt_eval_count")
    completion = data.get("eval_count")
    if prompt is None and completion is None:
        return None
    return {
        "input_tokens": int(prompt or 0),
        "output_tokens": int(completion or 0),
        "cache_read_tokens": 0,
        "reasoning_tokens": 0,
    }


def _native_calls(msg: dict) -> list:
    return [
        {"id": "", "name": tc["function"]["name"], "arguments": as_object(tc["function"].get("arguments"))}
        for tc in (msg.get("tool_calls") or [])
        if isinstance(tc, dict) and isinstance(tc.get("function"), dict)
    ]


def _native_once(endpoint: dict, body: dict, provider_label: str) -> dict:
    import httpx

    start = time.monotonic()
    resp = httpx.post(endpoint["api_url"], json=body, headers=endpoint["headers"], timeout=120.0)
    elapsed_ms = int((time.monotonic() - start) * 1000)
    if resp.status_code >= 400:
        return err_from_resp(provider_label, resp.status_code, resp.text)
    data = resp.json()
    msg = data.get("message") or {}
    calls = _native_calls(msg)
    finish = data.get("done_reason") or ("stop" if data.get("done") else "stop")
    return norm(
        msg.get("content") or None,
        calls,
        data.get("model", endpoint["model"]),
        "tool_calls" if calls else finish,
        reasoning=msg.get("thinking") or None,
        usage=map_native_usage(data),
        elapsed_ms=elapsed_ms,
    )


def _native_stream(endpoint: dict, body: dict, stream_path: str, sid, provider_label: str) -> dict:
    """NDJSON 流式：每行一个 JSON 对象，末行 done:true 附统计。返回与 _native_once 同形状。"""
    sink = StreamSink(stream_path, sid)
    sink.start()
    try:
        return _native_stream_once(endpoint, body, sink, provider_label)
    finally:
        sink.close()


def _native_stream_once(endpoint: dict, body: dict, sink: StreamSink, provider_label: str) -> dict:
    import httpx

    content_parts: list = []
    reasoning_parts: list = []
    calls: list = []
    finish = "stop"
    model = endpoint["model"]
    final_data: dict | None = None
    start = time.monotonic()
    start_ts = stream_checkpoint(payload_sid := sink.sid)  # R1：流式启动检查点（消费陈旧取消信号）
    try:
        with httpx.stream("POST", endpoint["api_url"], json=body, headers=endpoint["headers"], timeout=120.0) as resp:
            if resp.status_code >= 400:
                resp.read()
                payload = err_from_resp(provider_label, resp.status_code, resp.text)
                sink.error(payload["error"]["message"])
                return payload
            for raw in resp.iter_lines():
                # R1：逐帧取消检查——命中立即关流收敛（K499），不再消费剩余增量。
                if abort_requested(payload_sid, start_ts):
                    consume_abort(payload_sid)
                    message = "已被用户取消"
                    sink.error(message)
                    return err(message, code="K499")
                line = raw.strip() if isinstance(raw, str) else raw.decode("utf-8", "replace").strip()
                if not line:
                    continue
                try:
                    data = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if data.get("model"):
                    model = data["model"]
                # 尽力而为：部分错误形态可能在 200 流中以 {"error": ...} 行出现
                if data.get("error"):
                    payload = err_from_resp(provider_label, 500, json.dumps(data, ensure_ascii=False))
                    sink.error(payload["error"]["message"])
                    return payload
                msg = data.get("message") or {}
                # 思考链：推理模型（qwen3/deepseek-r1 等）经 message.thinking 分帧输出
                thought = msg.get("thinking")
                if thought:
                    reasoning_parts.append(thought)
                    sink.delta("reasoning", thought)
                piece = msg.get("content")
                if piece:
                    content_parts.append(piece)
                    sink.delta("text", piece)
                calls.extend(_native_calls(msg))
                if data.get("done"):
                    finish = data.get("done_reason") or finish
                    final_data = data
    except Exception as exc:  # noqa: BLE001 - 业务错误一律落 payload
        message = f"{provider_label} 流式失败: {type(exc).__name__}: {exc}"
        sink.error(message)
        return err(message)

    elapsed_ms = int((time.monotonic() - start) * 1000)
    usage = map_native_usage(final_data) if final_data else None
    sink.end(usage, elapsed_ms)
    return norm(
        "".join(content_parts) or None,
        calls,
        model,
        "tool_calls" if calls else finish,
        reasoning="".join(reasoning_parts) or None,
        usage=usage,
        elapsed_ms=elapsed_ms,
    )


# ── 分派 ────────────────────────────────────────────────────────────────

def chat(payload: dict) -> dict:
    require_httpx()
    ep = resolve_ollama()
    if endpoint_mode() == "v1":
        return _chat_with(ep, payload, "ollama")
    body = _build_body(ep, payload, stream=bool(payload.get("stream_path")))
    if payload.get("stream_path"):
        return _native_stream(ep, body, payload["stream_path"], payload.get("sid"), "ollama")
    return _native_once(ep, body, "ollama")


def _native_ctx_limit(host: str, name: str) -> int | None:
    """单模型原生上下文窗口：POST /api/show → model_info 的 `<family>.context_length`。

    取不到（单模型失败 / 字段缺失 / 非法值）返回 None → 调用方省略 ctx_limit 键，
    探测不致命（models.list 主契约不受影响）。
    """
    import httpx

    try:
        resp = httpx.post(f"http://{host}/api/show", json={"model": name}, timeout=30.0)
        if resp.status_code >= 400:
            return None
        info = (resp.json() or {}).get("model_info") or {}
        for k, v in info.items():
            if k.endswith(".context_length") and isinstance(v, int) and not isinstance(v, bool) and v > 0:
                return v
    except Exception:  # noqa: BLE001 - 单模型探测失败只降级，不拖垮列表
        return None
    return None


def models(payload: dict) -> dict:
    require_httpx()
    import httpx

    host = os.environ.get("OLLAMA_HOST", "localhost:11434").rstrip("/")
    url = f"http://{host}/api/tags"
    try:
        resp = httpx.get(url, timeout=30.0)
    except Exception as exc:  # noqa: BLE001
        return err(f"拉取模型列表失败: {type(exc).__name__}: {exc}")
    if resp.status_code >= 400:
        return err(f"获取模型列表 HTTP {resp.status_code}: {resp.text[:500]}")
    try:
        data = resp.json()
    except Exception as exc:
        return err(f"模型列表响应非法 JSON: {exc}")
    items = data.get("models", []) if isinstance(data, dict) else []
    names = [m.get("name") for m in items if isinstance(m, dict) and m.get("name")]
    # 可选扩展字段 models_meta：逐模型探测原生窗口（供前端展示与 llm_context_tokens 对齐），
    # 其他 provider 不带该字段；单模型探测失败省略 ctx_limit 键。
    # /api/show 单次 ~2s（ollama 侧基本无缓存），模型多时串行过慢 → 线程池并行（I/O 密集）
    meta: list = []
    if names:
        from concurrent.futures import ThreadPoolExecutor

        with ThreadPoolExecutor(max_workers=min(8, len(names))) as pool:
            for name, ctx in zip(names, pool.map(lambda n: _native_ctx_limit(host, n), names)):
                meta.append({"name": name, **({"ctx_limit": ctx} if ctx else {})})
    return {"ok": True, "models": names, "models_meta": meta}


PROVIDER = {"name": "ollama", "chat": chat, "models": models, "requires_env": []}
