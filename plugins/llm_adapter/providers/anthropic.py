"""anthropic provider：Messages API → 归一化契约。"""

import os

from .base import err, norm, require_httpx


def chat(payload: dict) -> dict:
    require_httpx()
    import httpx

    base = os.environ.get("ANTHROPIC_BASE_URL", "https://api.anthropic.com").rstrip("/")
    key = os.environ.get("ANTHROPIC_API_KEY", "")
    model = os.environ.get("LLM_MODEL", "claude-3-5-sonnet-latest")
    headers = {"x-api-key": key, "anthropic-version": "2023-06-01"}

    system_parts, messages = [], []
    for m in payload.get("messages", []):
        role, content = m.get("role"), m.get("content")
        if role == "system":
            if content:
                system_parts.append(content)
        elif role == "tool":
            messages.append({"role": "user", "content": [{"type": "tool_result", "tool_use_id": m.get("tool_call_id", ""), "content": content or ""}]})
        elif m.get("tool_calls"):
            blocks = []
            if content:
                blocks.append({"type": "text", "text": content})
            blocks += [{"type": "tool_use", "id": tc["id"], "name": tc["name"], "input": tc.get("arguments", {})} for tc in m["tool_calls"]]
            messages.append({"role": "assistant", "content": blocks})
        else:
            messages.append({"role": role or "user", "content": content or ""})

    body: dict = {"model": model, "max_tokens": 4096, "messages": messages}
    if system_parts:
        body["system"] = "\n".join(system_parts)
    tools = payload.get("tools")
    if tools:
        body["tools"] = [{"name": t["name"], "description": t.get("description", ""), "input_schema": t.get("parameters", {})} for t in tools]

    resp = httpx.post(f"{base}/v1/messages", json=body, headers=headers, timeout=120.0)
    if resp.status_code >= 400:
        return err(f"anthropic HTTP {resp.status_code}: {resp.text[:500]}")
    data = resp.json()
    text, calls = [], []
    for block in data.get("content", []):
        if block.get("type") == "text":
            text.append(block.get("text", ""))
        elif block.get("type") == "tool_use":
            calls.append({"id": block["id"], "name": block["name"], "arguments": block.get("input", {})})
    finish = data.get("stop_reason", "stop")
    return norm("\n".join(text) or None, calls, data.get("model", model), "tool_calls" if calls else finish)


_ANTHROPIC_MODELS = [
    "claude-3-5-sonnet-latest",
    "claude-3-5-haiku-latest",
    "claude-3-opus-latest",
    "claude-3-haiku-20240307",
]


def models(payload: dict) -> dict:
    return {"ok": True, "models": list(_ANTHROPIC_MODELS)}


PROVIDER = {"name": "anthropic", "chat": chat, "models": models, "requires_env": ["ANTHROPIC_API_KEY"]}
