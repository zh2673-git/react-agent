"""provider pack 协议（规则层：唯一被 registry 认可的形状）。

每个 provider 模块必须暴露模块级常量 PROVIDER：

    PROVIDER = {
        "name": str,                     # 与 llm.chat 请求 payload["provider"] / LLM_PROVIDER 对应
        "chat": Callable[[dict], dict],  # payload -> 归一化响应（见下），异常一律抛出由分派层兜底
        "requires_env": list[str],       # 缺失时 chat 内自行报错；此处仅作文档
    }

归一化响应契约（线契约，见 03 §2.2，逐字不变）：
    {"ok": True,  "content": str|null, "tool_calls": [{"id","name","arguments":object}],
     "model": str, "finish_reason": str}
    {"ok": False, "error": {"code": str, "message": str}}
"""

import json


def err(message: str, code: str = "LLM_ERROR") -> dict:
    return {"ok": False, "error": {"code": code, "message": message}}


def norm(content, calls, model, finish_reason) -> dict:
    return {"ok": True, "content": content, "tool_calls": calls, "model": model, "finish_reason": finish_reason}


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
