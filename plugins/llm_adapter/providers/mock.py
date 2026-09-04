"""mock provider：MOCK_SCRIPT（JSON 数组逐次弹出，耗尽后停在最后一个）；无脚本回 pong。"""

import json
import os

from .base import err, norm

_state = {"seq": 0}


def chat(payload: dict) -> dict:
    script = os.environ.get("MOCK_SCRIPT")
    if not script:
        return norm("pong", [], "mock", "stop")
    seq = json.loads(script)
    if not isinstance(seq, list) or not seq:
        return err("MOCK_SCRIPT must be a non-empty JSON array")
    i = min(_state["seq"], len(seq) - 1)
    _state["seq"] += 1
    item = seq[i]
    tool_calls = item.get("tool_calls", [])
    return norm(
        item.get("content"),
        tool_calls,
        item.get("model", "mock"),
        "tool_calls" if tool_calls else "stop",
    )


def models(payload: dict) -> dict:
    return {"ok": True, "models": ["mock-1"]}


PROVIDER = {"name": "mock", "chat": chat, "models": models, "requires_env": []}
