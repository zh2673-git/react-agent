"""mock provider：MOCK_SCRIPT（JSON 数组逐次弹出，耗尽后停在最后一个）；无脚本回 pong。

MOCK_CAPTURE（可选，e2e 诊断缝）：设为文件路径时，每次 chat 先把请求 payload
（messages/tools 等）以 JSONL 追加落盘，供测试断言 system 提示词组装链；
落盘失败静默忽略（诊断缝不影响主链路）。
"""

import json
import os

from .base import err, norm

_state = {"seq": 0}


def _capture(payload: dict) -> None:
    path = os.environ.get("MOCK_CAPTURE")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(payload, ensure_ascii=False) + "\n")
    except OSError:
        pass


def chat(payload: dict) -> dict:
    _capture(payload)
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


EMBED_DIM = 8


def embed(payload: dict) -> dict:
    """确定性 embed：同文本必得同向量，长度固定 EMBED_DIM。

    为什么是确定性而非随机：embed 的测试要断言「相同输入→相同输出」「不同输入→不同输出」，
    随机向量让这两条断言都失效。用字符码求和做伪哈希，够用且可读。

    MOCK_EMBED_FAIL=1 时返回错误，供测试覆盖失败分支（不靠网络造错误）。
    """
    from .base import err, norm_embed, require_inputs

    if os.environ.get("MOCK_EMBED_FAIL") == "1":
        return err("mock embed 失败（MOCK_EMBED_FAIL=1）")
    try:
        inputs = require_inputs(payload)
    except ValueError as exc:
        return err(str(exc), code="K400")
    vectors = []
    for text in inputs:
        seed = sum(ord(c) * (i + 1) for i, c in enumerate(text)) or 1
        vectors.append([((seed * (j + 1)) % 97) / 97.0 for j in range(EMBED_DIM)])
    return norm_embed(vectors, payload.get("model") or "mock-embed", dim=EMBED_DIM)


PROVIDER = {
    "name": "mock",
    "chat": chat,
    "models": models,
    "embed": embed,
    "requires_env": [],
}
