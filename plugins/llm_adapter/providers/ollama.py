"""ollama provider：复用 openai_compat 的共享实现，仅端点解析不同（本地 Ollama，免 key）。"""

import os

from .base import require_httpx, err
from .openai_compat import _chat_with


def resolve_ollama() -> dict:
    base = os.environ.get("OLLAMA_HOST", "localhost:11434").rstrip("/")
    return {
        "base_url": f"http://{base}/v1",
        "model": os.environ.get("LLM_MODEL", "qwen2.5:7b"),
        "headers": {},
    }


def chat(payload: dict) -> dict:
    require_httpx()
    return _chat_with(resolve_ollama(), payload, "ollama")


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
    return {"ok": True, "models": names}


PROVIDER = {"name": "ollama", "chat": chat, "models": models, "requires_env": []}
