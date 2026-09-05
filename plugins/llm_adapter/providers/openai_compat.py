"""OpenAI 兼容族（openai / DeepSeek / Moonshot 等——base_url 可换即接入）。

共享实现 `_chat_with(endpoint, payload)`；本模块只暴露 openai 专属端点解析，
ollama provider 复用同一实现（见 ollama.py）。
"""

import json
import os
import time

from .base import StreamSink, as_object, err, err_from_resp, map_usage, norm, require_httpx


def resolve_openai() -> dict:
    return {
        "base_url": os.environ.get("LLM_BASE_URL", "https://api.openai.com/v1").rstrip("/"),
        "model": os.environ.get("LLM_MODEL", "gpt-4o-mini"),
        "headers": {"Authorization": f"Bearer {os.environ.get('OPENAI_API_KEY', '')}"} if os.environ.get("OPENAI_API_KEY") else {},
    }


def to_openai_msg(m: dict) -> dict:
    role = m.get("role", "user")
    out: dict = {"role": role, "content": m.get("content")}
    if m.get("tool_calls"):
        out["tool_calls"] = [
            {
                "id": tc["id"],
                "type": "function",
                "function": {"name": tc["name"], "arguments": json.dumps(tc.get("arguments", {}))},
            }
            for tc in m["tool_calls"]
        ]
    if m.get("tool_call_id"):
        out["tool_call_id"] = m["tool_call_id"]
    return out


async_compat_chat = None  # 占位说明：同步实现，Serial 语义


def _build_body(endpoint: dict, payload: dict) -> dict:
    body: dict = {"model": endpoint["model"], "messages": [to_openai_msg(m) for m in payload.get("messages", [])]}
    tools = payload.get("tools")
    if tools:
        body["tools"] = [
            {"type": "function", "function": {"name": t["name"], "description": t.get("description", ""), "parameters": t.get("parameters", {})}}
            for t in tools
        ]
    return body


def _chat_with(endpoint: dict, payload: dict, provider_label: str) -> dict:
    """共享实现：payload 带 stream_path 走流式（增量写旁路），否则走原有一次请求。

    两条路径返回完全同形状的归一化响应——流式只是「边生成边外抛」，不改变契约。
    """
    require_httpx()
    body = _build_body(endpoint, payload)
    stream_path = payload.get("stream_path")
    if stream_path:
        return _chat_stream(endpoint, body, stream_path, payload.get("sid"), provider_label)
    return _chat_once(endpoint, body, provider_label)


def _chat_once(endpoint: dict, body: dict, provider_label: str) -> dict:
    import httpx

    start = time.monotonic()
    resp = httpx.post(f"{endpoint['base_url']}/chat/completions", json=body, headers=endpoint["headers"], timeout=120.0)
    elapsed_ms = int((time.monotonic() - start) * 1000)
    if resp.status_code >= 400:
        return err_from_resp(provider_label, resp.status_code, resp.text)
    data = resp.json()
    msg = data["choices"][0]["message"]
    calls = [
        {"id": tc.get("id", ""), "name": tc["function"]["name"], "arguments": as_object(tc["function"].get("arguments"))}
        for tc in (msg.get("tool_calls") or [])
    ]
    finish = data["choices"][0].get("finish_reason", "stop")
    # 非流式思考通道：DeepSeek 官方返回 reasoning_content；部分网关仅在流式下给。
    reasoning = msg.get("reasoning_content") or msg.get("reasoning") or None
    return norm(
        msg.get("content"),
        calls,
        data.get("model", endpoint["model"]),
        "tool_calls" if calls else finish,
        reasoning=reasoning,
        usage=map_usage(data["usage"]) if data.get("usage") else None,
        elapsed_ms=elapsed_ms,
    )


def _chat_stream(endpoint: dict, body: dict, stream_path: str, sid, provider_label: str) -> dict:
    """流式：逐 chunk 拆 thinking / text 并增量写旁路，返回与 `_chat_once` 同形状响应。

    SSE 解析遵循 deepseek-harness 的约定：首 chunk 常带空 `reasoning_content`（不得据此开块），
    思考先于正文；`finish` 以 `[DONE]` 为准，usage 由末帧给出。

    sink 在此创建并贯穿整轮（含降级重试），保证 start 行只写一次、前端气泡不重开。
    """
    sink = StreamSink(stream_path, sid)
    sink.start()
    try:
        return _stream_once(endpoint, body, sink, provider_label, include_usage=True)
    finally:
        sink.close()


def _stream_once(endpoint: dict, body: dict, sink: StreamSink, provider_label: str, include_usage: bool) -> dict:
    """单次流式请求。`include_usage=False` 即去掉 stream_options 的降级重试。"""
    import httpx

    stream_body = dict(body)
    stream_body["stream"] = True
    if include_usage:
        stream_body["stream_options"] = {"include_usage": True}

    content_parts: list = []
    reasoning_parts: list = []
    calls: dict = {}
    finish = "stop"
    model = endpoint["model"]
    usage_raw = None
    start = time.monotonic()
    try:
        with httpx.stream(
            "POST",
            f"{endpoint['base_url']}/chat/completions",
            json=stream_body,
            headers=endpoint["headers"],
            timeout=120.0,
        ) as resp:
            if resp.status_code >= 400:
                resp.read()
                if include_usage:
                    # 部分兼容层（自托管网关等）不认 stream_options → 去掉重试一次。
                    # 复用同一 sink：start 行不重复写，前端不会重开气泡。
                    return _stream_once(endpoint, body, sink, provider_label, False)
                payload = err_from_resp(provider_label, resp.status_code, resp.text)
                sink.error(payload["error"]["message"])
                return payload
            for raw in resp.iter_lines():
                line = raw.strip() if isinstance(raw, str) else raw.decode("utf-8", "replace").strip()
                if not line:
                    continue
                if line.startswith("data:"):
                    line = line[5:].strip()
                if line == "[DONE]":
                    break
                try:
                    data = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(data.get("usage"), dict):
                    usage_raw = data["usage"]
                if data.get("model"):
                    model = data["model"]
                choices = data.get("choices") or []
                if not choices:
                    continue
                ch0 = choices[0] or {}
                if ch0.get("finish_reason"):
                    finish = ch0["finish_reason"]
                delta = ch0.get("delta") or {}
                thought = delta.get("reasoning_content")
                if thought is None:
                    thought = delta.get("reasoning")
                if thought:  # 空串/None 不开思考块
                    reasoning_parts.append(thought)
                    sink.delta("reasoning", thought)
                piece = delta.get("content")
                if piece:
                    content_parts.append(piece)
                    sink.delta("text", piece)
                for tc in delta.get("tool_calls") or []:
                    cur = calls.setdefault(tc.get("index", 0), {"id": "", "name": "", "arguments": ""})
                    if tc.get("id"):
                        cur["id"] = tc["id"]
                    fn = tc.get("function") or {}
                    if fn.get("name"):
                        cur["name"] = fn["name"]
                    if fn.get("arguments"):
                        cur["arguments"] += fn["arguments"]
    except Exception as exc:  # noqa: BLE001 - 业务错误一律落 payload
        message = f"{provider_label} 流式失败: {type(exc).__name__}: {exc}"
        sink.error(message)
        return err(message)

    elapsed_ms = int((time.monotonic() - start) * 1000)
    # 未上报 usage → None（前端据此不显示统计条，而不是显示一串 0）
    usage = map_usage(usage_raw) if usage_raw else None
    sink.end(usage, elapsed_ms)

    call_list = [{"id": v["id"], "name": v["name"], "arguments": as_object(v["arguments"])} for v in calls.values() if v["name"]]
    return norm(
        "".join(content_parts) or None,
        call_list,
        model,
        "tool_calls" if call_list else finish,
        reasoning="".join(reasoning_parts) or None,
        usage=usage,
        elapsed_ms=elapsed_ms,
    )


def chat(payload: dict) -> dict:
    require_httpx()
    return _chat_with(resolve_openai(), payload, "openai")


def models(payload: dict) -> dict:
    require_httpx()
    import httpx

    ep = resolve_openai()
    url = f"{ep['base_url']}/models"
    try:
        resp = httpx.get(url, headers=ep["headers"], timeout=30.0)
    except Exception as exc:  # noqa: BLE001
        return err(f"拉取模型列表失败: {type(exc).__name__}: {exc}")
    if resp.status_code >= 400:
        return err(f"获取模型列表 HTTP {resp.status_code}: {resp.text[:500]}")
    try:
        data = resp.json()
    except Exception as exc:
        return err(f"模型列表响应非法 JSON: {exc}")
    items = data.get("data", []) if isinstance(data, dict) else []
    ids = [m.get("id") for m in items if isinstance(m, dict) and m.get("id")]
    return {"ok": True, "models": ids}


PROVIDER = {"name": "openai", "chat": chat, "models": models, "requires_env": ["OPENAI_API_KEY"]}
