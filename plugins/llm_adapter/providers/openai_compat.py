"""OpenAI 兼容族（openai / DeepSeek / Moonshot 等——base_url 可换即接入）。

共享实现 `_chat_with(endpoint, payload)`；本模块只暴露 openai 专属端点解析，
ollama provider 复用同一实现（见 ollama.py）。
"""

import json
import os

from .base import as_object, err, norm, require_httpx


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


def _chat_with(endpoint: dict, payload: dict, provider_label: str) -> dict:
    import httpx

    body: dict = {"model": endpoint["model"], "messages": [to_openai_msg(m) for m in payload.get("messages", [])]}
    tools = payload.get("tools")
    if tools:
        body["tools"] = [
            {"type": "function", "function": {"name": t["name"], "description": t.get("description", ""), "parameters": t.get("parameters", {})}}
            for t in tools
        ]
    resp = httpx.post(f"{endpoint['base_url']}/chat/completions", json=body, headers=endpoint["headers"], timeout=120.0)
    if resp.status_code >= 400:
        return err(f"{provider_label} HTTP {resp.status_code}: {resp.text[:500]}")
    data = resp.json()
    msg = data["choices"][0]["message"]
    calls = [
        {"id": tc.get("id", ""), "name": tc["function"]["name"], "arguments": as_object(tc["function"].get("arguments"))}
        for tc in (msg.get("tool_calls") or [])
    ]
    finish = data["choices"][0].get("finish_reason", "stop")
    return norm(msg.get("content"), calls, data.get("model", endpoint["model"]), "tool_calls" if calls else finish)


def chat(payload: dict) -> dict:
    require_httpx()
    return chat_with(resolve_openai(), payload, "openai")


PROVIDER = {"name": "openai", "chat": chat, "requires_env": ["OPENAI_API_KEY"]}
