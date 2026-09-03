"""llm-adapter guest 插件（Python，Process 域 gRPC）。

线契约（见下游项目 README「Contracts」）：
  req:  {"op":"chat","provider"?:"openai"|"anthropic"|"ollama"|"mock","messages":[...],"tools"?:[...]}
  resp: {"ok":true,"content":str|null,"tool_calls":[{"id","name","arguments":object}],"model":str,
         "finish_reason":"stop"|"tool_calls"} | {"ok":false,"error":{"code","message"}}

provider 按请求 `payload["provider"]` 覆盖，否则读环境变量 LLM_PROVIDER（缺省 mock）。
配置一律走环境变量（内核 Init 只送 None）：
  LLM_PROVIDER / LLM_MODEL / LLM_BASE_URL / OLLAMA_HOST / OPENAI_API_KEY /
  ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL / MOCK_SCRIPT（JSON 数组，逐次弹出；离线测试用）

归一化：OpenAI 兼容的 tool_calls[].function.arguments 是 JSON 字符串，Anthropic 的
tool_use.input 是对象——统一输出为对象 arguments。
"""

import json
import os

try:
    import httpx
except ImportError:  # pragma: no cover - mock provider 不需要 httpx
    httpx = None

from agent_kernel.guest import serve


def _err(message: str, code: str = "LLM_ERROR") -> dict:
    return {"ok": False, "error": {"code": code, "message": message}}


class LlmAdapterPlugin:
    def manifest(self) -> dict:
        return {"id": "llm-adapter", "version": "0.1.0", "api_version": "0.1"}

    def init(self, config) -> None:
        self._mock_seq = 0
        self._mock_last = None

    # ---- op 分派 ----------------------------------------------------------

    def on_event(self, envelope: dict) -> dict:
        payload = envelope.get("payload") or {}
        op = payload.get("op", "chat")
        if op != "chat":
            return _err(f"unknown op: {op}", code="K400")
        provider = payload.get("provider") or os.environ.get("LLM_PROVIDER", "mock")
        try:
            if provider == "mock":
                return self._mock()
            if provider == "anthropic":
                return self._anthropic(payload)
            if provider in ("openai", "ollama"):
                return self._openai_compat(payload, provider)
            return _err(f"unknown provider: {provider}", code="K400")
        except Exception as exc:  # noqa: BLE001 - 业务错误一律落 payload
            return _err(f"{type(exc).__name__}: {exc}")

    # ---- mock -------------------------------------------------------------

    def _mock(self) -> dict:
        script = os.environ.get("MOCK_SCRIPT")
        if not script:
            return self._norm("pong", [], "mock", "stop")
        seq = json.loads(script)
        if not isinstance(seq, list) or not seq:
            return _err("MOCK_SCRIPT must be a non-empty JSON array")
        i = min(self._mock_seq, len(seq) - 1)
        self._mock_seq += 1
        item = seq[i]
        tool_calls = item.get("tool_calls", [])
        return self._norm(
            item.get("content"),
            tool_calls,
            item.get("model", "mock"),
            "tool_calls" if tool_calls else "stop",
        )

    # ---- OpenAI 兼容（openai / ollama / DeepSeek 等） ----------------------

    def _openai_compat(self, payload: dict, provider: str) -> dict:
        if provider == "ollama":
            base = os.environ.get("OLLAMA_HOST", "localhost:11434").rstrip("/")
            base_url = f"http://{base}/v1"
            model = os.environ.get("LLM_MODEL", "qwen2.5:7b")
            headers = {}
        else:
            base_url = os.environ.get("LLM_BASE_URL", "https://api.openai.com/v1").rstrip("/")
            model = os.environ.get("LLM_MODEL", "gpt-4o-mini")
            key = os.environ.get("OPENAI_API_KEY", "")
            headers = {"Authorization": f"Bearer {key}"} if key else {}

        body: dict = {"model": model, "messages": [self._to_openai_msg(m) for m in payload.get("messages", [])]}
        tools = payload.get("tools")
        if tools:
            body["tools"] = [
                {"type": "function", "function": {"name": t["name"], "description": t.get("description", ""), "parameters": t.get("parameters", {})}}
                for t in tools
            ]

        resp = httpx.post(f"{base_url}/chat/completions", json=body, headers=headers, timeout=120.0)
        if resp.status_code >= 400:
            return _err(f"{provider} HTTP {resp.status_code}: {resp.text[:500]}")
        data = resp.json()
        msg = data["choices"][0]["message"]
        raw_calls = msg.get("tool_calls") or []
        calls = [
            {
                "id": tc.get("id", ""),
                "name": tc["function"]["name"],
                "arguments": self._as_object(tc["function"].get("arguments")),
            }
            for tc in raw_calls
        ]
        finish = data["choices"][0].get("finish_reason", "stop")
        return self._norm(msg.get("content"), calls, data.get("model", model), "tool_calls" if calls else finish)

    @staticmethod
    def _to_openai_msg(m: dict) -> dict:
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

    # ---- Anthropic ---------------------------------------------------------

    def _anthropic(self, payload: dict) -> dict:
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
            return _err(f"anthropic HTTP {resp.status_code}: {resp.text[:500]}")
        data = resp.json()
        text, calls = [], []
        for block in data.get("content", []):
            if block.get("type") == "text":
                text.append(block.get("text", ""))
            elif block.get("type") == "tool_use":
                calls.append({"id": block["id"], "name": block["name"], "arguments": block.get("input", {})})
        finish = data.get("stop_reason", "stop")
        return self._norm("\n".join(text) or None, calls, data.get("model", model), "tool_calls" if calls else finish)

    # ---- 归一化 ------------------------------------------------------------

    @staticmethod
    def _as_object(args) -> dict:
        if isinstance(args, str):
            try:
                return json.loads(args)
            except json.JSONDecodeError:
                return {"_raw": args}
        return args if isinstance(args, dict) else {}

    @staticmethod
    def _norm(content, calls, model, finish_reason) -> dict:
        return {"ok": True, "content": content, "tool_calls": calls, "model": model, "finish_reason": finish_reason}

    def destroy(self) -> None:
        pass


if __name__ == "__main__":
    serve(LlmAdapterPlugin())
