"""llm-adapter guest 插件（Python，Process 域 gRPC）——瘦分派层。

本文件只做 op 分派 + provider registry 查找；每家供应商的实现位于 providers/ 下的
独立 pack（协议见 providers/base.py）。新增供应商 = 新增一个 pack 文件，不改本文件。

线契约（03 §2.2，冻结）：
  req:  {"op":"chat","provider"?:"openai"|"anthropic"|"ollama"|"mock","messages":[...],"tools"?:[...]}
  resp: {"ok":true,"content":str|null,"tool_calls":[{"id","name","arguments":object}],"model":str,
         "finish_reason":str} | {"ok":false,"error":{"code","message"}}

provider 按请求 payload["provider"] 覆盖，否则环境变量 LLM_PROVIDER（缺省 mock）。
配置一律走环境变量：LLM_PROVIDER / LLM_MODEL / LLM_BASE_URL / OLLAMA_HOST /
OPENAI_API_KEY / ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL / MOCK_SCRIPT
"""

import os

from agent_kernel.guest import serve

from providers.registry import resolve


def _err(message: str, code: str = "LLM_ERROR") -> dict:
    return {"ok": False, "error": {"code": code, "message": message}}


class LlmAdapterPlugin:
    def manifest(self) -> dict:
        return {"id": "llm-adapter", "version": "0.1.0", "api_version": "0.1"}

    def init(self, config) -> None:
        pass

    def on_event(self, envelope: dict) -> dict:
        payload = envelope.get("payload") or {}
        op = payload.get("op", "chat")
        if op != "chat":
            return _err(f"unknown op: {op}", code="K400")
        name = payload.get("provider") or os.environ.get("LLM_PROVIDER", "mock")
        provider = resolve(name)
        if provider is None:
            return _err(f"unknown provider: {name}（可用: {', '.join(sorted_providers())}）", code="K400")
        try:
            return provider["chat"](payload)
        except Exception as exc:  # noqa: BLE001 - 业务错误一律落 payload，不中断编排层
            return _err(f"{type(exc).__name__}: {exc}")

    def destroy(self) -> None:
        pass


def sorted_providers() -> list:
    from providers.registry import PROVIDERS

    return sorted(PROVIDERS)


if __name__ == "__main__":
    serve(LlmAdapterPlugin())
