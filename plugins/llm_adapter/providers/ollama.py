"""ollama provider：复用 openai_compat 的共享实现，仅端点解析不同（本地 Ollama，免 key）。"""

import os

from .base import require_httpx
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
    return chat_with(resolve_ollama(), payload, "ollama")


PROVIDER = {"name": "ollama", "chat": chat, "requires_env": []}
