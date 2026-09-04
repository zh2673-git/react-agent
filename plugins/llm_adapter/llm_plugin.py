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

configure op（08 §2.2，Phase 4-1 运行时热配置——provider pack 均为请求时读 env，
故热配置 = 更新本进程 os.environ，对全部 pack 一致生效）：
  {"op":"configure","provider"?,"model"?,"base_url"?,"api_key"?}
  → {"ok":true,"applied":{...}}（api_key 只回 api_key_set:true，明文不回显不落日志）
  显式 per-request provider 仍最优先；重启后由 host 以 config.json 还原 env。
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
        if op == "configure":
            return self._configure(payload)
        if op == "models.list":
            return self._models_list(payload)
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

    def _configure(self, payload: dict) -> dict:
        applied: dict = {}
        provider = payload.get("provider")
        if provider is not None:
            if not isinstance(provider, str) or resolve(provider) is None:
                return _err(
                    f"unknown provider: {provider}（可用: {', '.join(sorted_providers())}）",
                    code="K400",
                    field="provider",
                )
            os.environ["LLM_PROVIDER"] = provider
            applied["provider"] = provider
        # base_url / api_key 按「本次配置后的生效 provider」路由到对应 env
        #   openai 兼容 → LLM_BASE_URL / OPENAI_API_KEY
        #   anthropic   → ANTHROPIC_BASE_URL / ANTHROPIC_API_KEY
        #   ollama      → OLLAMA_HOST（本地免 key，忽略 api_key）
        effective = applied.get("provider") or os.environ.get("LLM_PROVIDER", "mock")
        if effective == "anthropic":
            base_env, key_env = "ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY"
        elif effective == "ollama":
            base_env, key_env = "OLLAMA_HOST", None
        else:
            base_env, key_env = "LLM_BASE_URL", "OPENAI_API_KEY"
        model = payload.get("model")
        if model:
            if not isinstance(model, str):
                return _err("model 必须是字符串", code="K400", field="model")
            os.environ["LLM_MODEL"] = model
            applied["model"] = model
        base_url = payload.get("base_url")
        if base_url:
            if not isinstance(base_url, str):
                return _err("base_url 必须是字符串", code="K400", field="base_url")
            os.environ[base_env] = base_url
            applied["base_url"] = base_url
        api_key = payload.get("api_key")
        if api_key:
            if key_env is None:
                return _err(f"provider {effective} 不需要 api_key（如 ollama 本地）", code="K400", field="api_key")
            if not isinstance(api_key, str):
                return _err("api_key 必须是字符串", code="K400", field="api_key")
            os.environ[key_env] = api_key
            applied["api_key_set"] = True  # 明文绝不回显
        return {"ok": True, "applied": applied}

    def _models_list(self, payload: dict) -> dict:
        name = payload.get("provider") or os.environ.get("LLM_PROVIDER", "mock")
        provider = resolve(name)
        if provider is None:
            return _err(f"unknown provider: {name}（可用: {', '.join(sorted_providers())}）", code="K400")
        fn = provider.get("models")
        if fn is None:
            return _err(f"provider {name} 不支持 models.list", code="K400")
        try:
            return fn(payload)
        except Exception as exc:  # noqa: BLE001
            return _err(f"{type(exc).__name__}: {exc}")

    def destroy(self) -> None:
        pass


def sorted_providers() -> list:
    from providers.registry import PROVIDERS

    return sorted(PROVIDERS)


if __name__ == "__main__":
    serve(LlmAdapterPlugin())
