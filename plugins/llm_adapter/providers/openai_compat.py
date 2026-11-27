"""OpenAI 兼容族（openai / DeepSeek / Moonshot 等——base_url 可换即接入）。

共享实现 `_chat_with(endpoint, payload)`；本模块只暴露 openai 专属端点解析，
ollama provider 复用同一实现（见 ollama.py）。
"""

import json
import os
import time

from .base import (
    PROVIDER_DEADLINE,
    StreamSink,
    abort_requested,
    as_object,
    consume_abort,
    err,
    err_from_resp,
    map_usage,
    norm,
    require_httpx,
    stream_checkpoint,
)


def resolve_openai() -> dict:
    return {
        "base_url": os.environ.get("LLM_BASE_URL", "https://api.openai.com/v1").rstrip("/"),
        "model": os.environ.get("LLM_MODEL", "gpt-4o-mini"),
        "headers": {"Authorization": f"Bearer {os.environ.get('OPENAI_API_KEY', '')}"} if os.environ.get("OPENAI_API_KEY") else {},
    }


def to_openai_msg(m: dict) -> dict:
    role = m.get("role", "user")
    content = m.get("content")
    # R3 图片附件 → content 数组（image_url data URI）。文本附件由 agent-loop 内嵌进
    # content，不在此处理。无附件保持纯字符串（不破坏既有请求形状）。
    images = [a for a in (m.get("attachments") or []) if isinstance(a, dict) and a.get("data_b64")]
    if images:
        parts: list = [{"type": "text", "text": content or ""}]
        parts += [
            {"type": "image_url", "image_url": {"url": f"data:{a.get('mime', 'image/png')};base64,{a['data_b64']}"}}
            for a in images
        ]
        out: dict = {"role": role, "content": parts}
    else:
        out = {"role": role, "content": content}
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


def _build_body(endpoint: dict, payload: dict) -> dict:
    body: dict = {"model": endpoint["model"], "messages": [to_openai_msg(m) for m in payload.get("messages", [])]}
    tools = payload.get("tools")
    if tools:
        body["tools"] = [
            {"type": "function", "function": {"name": t["name"], "description": t.get("description", ""), "parameters": t.get("parameters", {})}}
            for t in tools
        ]
    # 逃生舱：LLM_EXTRA_BODY（JSON）整体合并进请求体——网关要求私有字段
    # （如 enable_thinking / chat_template_kwargs）时无需改代码。非法 JSON 打警告忽略。
    extra = os.environ.get("LLM_EXTRA_BODY", "").strip()
    if extra:
        try:
            body.update(json.loads(extra))
        except ValueError:
            print(f"[llm_adapter] LLM_EXTRA_BODY 不是合法 JSON，已忽略: {extra[:100]}", flush=True)
    return body


def _chat_with(endpoint: dict, payload: dict, provider_label: str) -> dict:
    """共享实现：payload 带 stream_path 走流式（增量写旁路），否则走原有一次请求。

    两条路径返回完全同形状的归一化响应——流式只是「边生成边外抛」，不改变契约。
    """
    require_httpx()
    body = _build_body(endpoint, payload)
    # 诊断开关：LLM_DEBUG=1 时把最终请求体落盘到旁路目录（.stream/last-request.json），
    # 用于核对「直连有思考、过 host 无思考」一类请求差异问题。
    if os.environ.get("LLM_DEBUG") == "1" and payload.get("stream_path"):
        try:
            dbg = os.path.join(os.path.dirname(payload["stream_path"]), "last-request.json")
            with open(dbg, "w", encoding="utf-8") as f:
                json.dump(body, f, ensure_ascii=False, indent=2)
        except OSError as exc:
            print(f"[llm_adapter] LLM_DEBUG 请求转储失败: {exc}", flush=True)
    stream_path = payload.get("stream_path")
    if stream_path:
        return _chat_stream(endpoint, body, stream_path, payload.get("sid"), provider_label)
    return _chat_once(endpoint, body, provider_label)


def _chat_once(endpoint: dict, body: dict, provider_label: str) -> dict:
    import httpx

    # P3：provider 内幂等重试。过去一次 POST 定生死（429/5xx 直接落错误，重试全赖上游
    # agent-loop 整轮 T1-T3，代价是整轮重来）。此处只重试「连接已建立」的单请求：
    # POST /chat/completions 非流式在拿到响应前无已生成内容，重试安全；流式路径不经过
    # 这里（流式中断含已外抛增量，重试会造成重复气泡，交给上游按轮处理）。
    # 429 尊重 Retry-After（上限 5s）；指数退避 1s/2s；总预算仍受 PROVIDER_DEADLINE 约束
    # （每次尝试独立计时，agent-loop 总闸兜底整轮）。
    attempts = 3
    last: dict | None = None
    for attempt in range(attempts):
        start = time.monotonic()
        try:
            resp = httpx.post(f"{endpoint['base_url']}/chat/completions", json=body, headers=endpoint["headers"], timeout=httpx.Timeout(PROVIDER_DEADLINE, connect=10.0))
        except Exception as exc:  # noqa: BLE001 - httpx 连接异常（ConnectError 等）非 OSError；响应未到达，重试安全
            last = err(f"{provider_label} 连接失败: {type(exc).__name__}: {exc}", code="LLM_TRANSIENT")
            # P5 观测：真实尝试次数写顶层键（agent-loop 读顶层 http_attempts）。
            last["http_attempts"] = attempt + 1
        else:
            elapsed_ms = int((time.monotonic() - start) * 1000)
            if resp.status_code < 400:
                data = resp.json()
                # 原成功路径主体（原样保留，仅包进循环提前返回）
                out = _chat_once_success(endpoint, body, provider_label, data, elapsed_ms, attempt + 1)
                # 200 空正文防护：网关（账号额度耗尽/登录过期/限流页）会回 200 + 空 content，
                # 旧实现 norm(ok=True) 让 agent-loop 拿到空答案，前端 app-stream 又丢弃空 answer
                # 事件 → 用户只看到空白。判为瞬时错误并重试，把「空答案」转成可恢复的故障转移。
                if (out.get("ok") and not (out.get("content") or "").strip()
                        and not out.get("tool_calls") and not out.get("reasoning")):
                    last = err(f"{provider_label} HTTP 200 但回复正文为空（账号额度/登录态/网关异常）: {json.dumps(data, ensure_ascii=False)[:200]}", code="LLM_TRANSIENT")
                    last["http_attempts"] = attempt + 1
                    if attempt < attempts - 1:
                        time.sleep(float(2 ** attempt))
                        continue
                    return last
                return out
            last = err_from_resp(provider_label, resp.status_code, resp.text)
            # P5 观测：HTTP 层失败同样上报真实尝试次数（含本次）。
            last["http_attempts"] = attempt + 1
            if last.get("error", {}).get("code") == "LLM_TRANSIENT" and attempt < attempts - 1:
                retry_after = 1.0
                try:
                    retry_after = min(5.0, float(resp.headers.get("Retry-After", "1")) or 1.0)
                except (TypeError, ValueError):
                    retry_after = 1.0
                time.sleep(retry_after if resp.status_code == 429 else float(2 ** attempt))
                continue
            return last
    return last or err(f"{provider_label} 重试耗尽", code="LLM_TRANSIENT")


def _chat_once_success(
    endpoint: dict, body: dict, provider_label: str, data: dict, elapsed_ms: int, http_attempts: int = 1
) -> dict:
    """_chat_once 的成功响应归一化段（原实现原样提取，行为不变）。"""
    # 200 异常态防护：部分网关（限流/瞬时故障）会在 200 里返回非 chat 形态（null / {"error":...}），
    # 硬下标 choices 会崩成 'NoneType' object is not subscriptable——统一收敛为业务错误。
    if not isinstance(data, dict) or not data.get("choices"):
        return err(f"{provider_label} HTTP 200 响应缺少 choices（网关异常响应）: {json.dumps(data, ensure_ascii=False)[:300]}")
    msg = data["choices"][0].get("message") or {}
    calls = [
        {"id": tc.get("id", ""), "name": tc["function"]["name"], "arguments": as_object(tc["function"].get("arguments"))}
        for tc in (msg.get("tool_calls") or [])
    ]
    finish = data["choices"][0].get("finish_reason") or "stop"
    # 非流式思考通道：DeepSeek 官方返回 reasoning_content；部分网关仅在流式下给。
    reasoning = msg.get("reasoning_content") or msg.get("reasoning") or None
    return norm(
        msg.get("content"),
        calls,
        data.get("model", endpoint["model"]),
        "tool_calls" if calls else finish,
        reasoning=reasoning,
        usage=map_usage(data["usage"]) if data.get("usage") else None,
        elapsed_ms=elapsed_ms,
        http_attempts=http_attempts,
    )


def _chat_stream(endpoint: dict, body: dict, stream_path: str, sid, provider_label: str) -> dict:
    """流式：逐 chunk 拆 thinking / text 并增量写旁路，返回与 `_chat_once` 同形状响应。

    SSE 解析遵循 deepseek-harness 的约定：首 chunk 常带空 `reasoning_content`（不得据此开块），
    思考先于正文；`finish` 以 `[DONE]` 为准，usage 由末帧给出。

    sink 在此创建并贯穿整轮（含降级重试），保证 start 行只写一次、前端气泡不重开。
    """
    sink = StreamSink(stream_path, sid)
    sink.start()
    # 瞬态重试：网关把上游 401/429/502 吞成 200+空流（LLM_EMPTY）或直接回 429/5xx
    # （LLM_TRANSIENT）时，重新请求同一网关 = 网关重新走 smart 路由、换一个账号。
    # 复用同一 sink：start 行只写一次、空流不写 end、重试不写 error，最终失败才写一次 error，
    # 前端气泡全程不重开、不闪错。
    # 尝试次数取 5：网关 auto 组可能多个账号同时故障（实测 5 个成员 4 个坏），
    # 每次请求 = 网关重新 smart 路由换号，5 次把撞到健康号的概率从 ~49% 提到 ~67%；
    # 退避递增让网关额度态/健康态有时间刷新。总时限受 PROVIDER_DEADLINE 守卫，不超内核 K502。
    attempts = 5
    last: dict | None = None
    stream_start = time.monotonic()
    try:
        for attempt in range(attempts):
            out = _stream_once(endpoint, body, sink, provider_label, include_usage=True, http_attempts=attempt + 1)
            code = (out.get("error") or {}).get("code")
            if code not in ("LLM_EMPTY", "LLM_TRANSIENT") or attempt == attempts - 1:
                if not out.get("ok"):
                    sink.error(out.get("error", {}).get("message", "流式请求失败"))
                return out
            last = out
            # 总时限守卫： retries 累计已接近 PROVIDER_DEADLINE 就停止重试，
            # 返回最后一次错误（避免越过内核 120s K502 闸导致 guest 线程泄漏）。
            if time.monotonic() - stream_start > PROVIDER_DEADLINE - 15:
                break
            retry_after = 1.0
            if "429" in out.get("error", {}).get("message", ""):
                retry_after = min(5.0, max(1.0, retry_after))
            time.sleep(retry_after * (attempt + 1))  # 1s / 2s 轻微退避，让网关额度态刷新
        if last is None:
            last = err("流式重试耗尽", code="LLM_TRANSIENT")
        if not last.get("ok"):
            sink.error(last.get("error", {}).get("message", "流式请求失败"))
        return last
    finally:
        sink.close()


def _stream_once(
    endpoint: dict,
    body: dict,
    sink: StreamSink,
    provider_label: str,
    include_usage: bool,
    http_attempts: int = 1,
) -> dict:
    """单次流式请求。`include_usage=False` 即去掉 stream_options 的降级重试。"""
    import httpx

    stream_body = dict(body)
    stream_body["stream"] = True
    if include_usage:
        stream_body["stream_options"] = {"include_usage": True}

    content_parts: list = []
    reasoning_parts: list = []
    calls: dict = {}
    finish = "stop"
    model = endpoint["model"]
    usage_raw = None
    start = time.monotonic()
    start_ts = stream_checkpoint(payload_sid := sink.sid)  # R1：流式启动检查点（消费陈旧取消信号）
    try:
        with httpx.stream(
            "POST",
            f"{endpoint['base_url']}/chat/completions",
            json=stream_body,
            headers=endpoint["headers"],
            timeout=httpx.Timeout(PROVIDER_DEADLINE, connect=10.0),
        ) as resp:
            if resp.status_code >= 400:
                resp.read()
                if include_usage:
                    # 部分兼容层（自托管网关等）不认 stream_options → 去掉重试一次。
                    # 复用同一 sink：start 行不重复写，前端不会重开气泡。
                    # P5 观测：降级重放是**真实的第二次** HTTP 请求，尝试数记 2。
                    return _stream_once(endpoint, body, sink, provider_label, False, http_attempts + 1)
                payload = err_from_resp(provider_label, resp.status_code, resp.text)
                payload["http_attempts"] = http_attempts
                # 不在此写 sink.error：LLM_TRANSIENT(429/5xx) 会由 _chat_stream 重试，
                # 多条 error 事件会让前端气泡闪错；只在最终失败时由外层写一次。
                return payload
            for raw in resp.iter_lines():
                # R1：逐帧取消检查——命中立即关流收敛（K499），不再消费剩余增量。
                if abort_requested(payload_sid, start_ts):
                    consume_abort(payload_sid)
                    message = "已被用户取消"
                    sink.error(message)
                    return err(message, code="K499")
                # 总时限守卫（K502 雪崩修复）：httpx 的 timeout 是 per-read 不是 total，
                # 涓流可无限续命；必须赶在内核 K502 abort（120s）前主动收敛成业务错误
                # （message 含 timeout → agent-loop T3 正常重试，且不泄漏 guest 线程）。
                if time.monotonic() - start > PROVIDER_DEADLINE:
                    message = (
                        f"{provider_label} provider timeout: 流式总时长超过 {PROVIDER_DEADLINE:.0f}s"
                        "（上游慢/限流），已在内核 K502 前主动收敛"
                    )
                    sink.error(message)
                    return err(message)
                line = raw.strip() if isinstance(raw, str) else raw.decode("utf-8", "replace").strip()
                if not line:
                    continue
                if line.startswith("data:"):
                    line = line[5:].strip()
                if line == "[DONE]":
                    break
                try:
                    data = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(data.get("usage"), dict):
                    usage_raw = data["usage"]
                if data.get("model"):
                    model = data["model"]
                choices = data.get("choices") or []
                if not choices:
                    continue
                ch0 = choices[0] or {}
                if ch0.get("finish_reason"):
                    finish = ch0["finish_reason"]
                delta = ch0.get("delta") or {}
                thought = delta.get("reasoning_content")
                if thought is None:
                    thought = delta.get("reasoning")
                if thought:  # 空串/None 不开思考块
                    reasoning_parts.append(thought)
                    sink.delta("reasoning", thought)
                piece = delta.get("content")
                if piece:
                    content_parts.append(piece)
                    sink.delta("text", piece)
                for tc in delta.get("tool_calls") or []:
                    cur = calls.setdefault(tc.get("index", 0), {"id": "", "name": "", "arguments": ""})
                    if tc.get("id"):
                        cur["id"] = tc["id"]
                    fn = tc.get("function") or {}
                    if fn.get("name"):
                        cur["name"] = fn["name"]
                    if fn.get("arguments"):
                        cur["arguments"] += fn["arguments"]
    except Exception as exc:  # noqa: BLE001 - 业务错误一律落 payload
        message = f"{provider_label} 流式失败: {type(exc).__name__}: {exc}"
        sink.error(message)
        return err(message)

    elapsed_ms = int((time.monotonic() - start) * 1000)
    # 碎片合并（write 反复 MISSING_ARG 的根因修复）：部分兼容网关会把 tool_call 的
    # arguments 尾部增量发在**无 name 的新 index** 下（name 只在首帧），按 index 分桶后
    # 具名桶只剩首帧残片（如 `{"backup": false`），无名桶被下方 `if v["name"]` 过滤丢弃
    # → arguments 残缺 → as_object 落 _raw → 工具 MISSING_ARG 反复重试。此处把无名碎片
    # 按出现顺序并回唯一具名调用（仅恰一个具名调用时合并，多调用场景不猜归属）。
    named = [v for v in calls.values() if v["name"]]
    if len(named) == 1:
        for v in calls.values():  # dict 保插入序，按到达顺序追加
            if not v["name"] and v["arguments"]:
                named[0]["arguments"] += v["arguments"]

    full_text = "".join(content_parts)
    full_reasoning = "".join(reasoning_parts)
    call_list = [{"id": v["id"], "name": v["name"], "arguments": as_object(v["arguments"])} for v in calls.values() if v["name"]]
    # 流式空正文防护：正文/思考/工具调用全空 → LLM_EMPTY，由 _chat_stream 重试
    # （网关把上游 401/429/502 吞成 200+空流时会命中；重试=重新请求=网关重新路由）。
    # 关键：在 sink.end() 之前判定——空流不写 end 事件，外层重试复用同一 sink 不串台。
    if not full_text.strip() and not call_list and not full_reasoning.strip():
        out = err(f"{provider_label} 流式响应正文为空（账号额度/登录态/网关异常）", code="LLM_EMPTY")
        out["http_attempts"] = http_attempts
        return out

    # 未上报 usage → None（前端据此不显示统计条，而不是显示一串 0）
    usage = map_usage(usage_raw) if usage_raw else None
    sink.end(usage, elapsed_ms)
    return norm(
        full_text or None,
        call_list,
        model,
        "tool_calls" if call_list else finish,
        reasoning=full_reasoning or None,
        usage=usage,
        elapsed_ms=elapsed_ms,
        http_attempts=http_attempts,
    )


def chat(payload: dict) -> dict:
    require_httpx()
    return _chat_with(resolve_openai(), payload, "openai")


def embed(payload: dict) -> dict:
    """OpenAI 兼容 /embeddings。走独立端点，不碰 chat 的 _build_body（形状不同）。"""
    require_httpx()
    import httpx

    from .base import norm_embed, require_inputs

    inputs = require_inputs(payload)
    ep = resolve_embed()
    body = {"model": payload.get("model") or ep["model"], "input": inputs}
    # 逃生舱：EMBEDDING_EXTRA_BODY（JSON）整体合并——部分网关要 dimensions/encoding_format
    extra = os.environ.get("EMBEDDING_EXTRA_BODY", "").strip()
    if extra:
        try:
            body.update(json.loads(extra))
        except ValueError:
            print(f"[llm_adapter] EMBEDDING_EXTRA_BODY 不是合法 JSON，已忽略: {extra[:100]}", flush=True)
    url = f"{ep['base_url']}/embeddings"
    try:
        resp = httpx.post(url, headers=ep["headers"], json=body, timeout=60.0)
    except Exception as exc:  # noqa: BLE001
        return err(f"embedding 请求失败: {type(exc).__name__}: {exc}")
    if resp.status_code >= 400:
        return err_from_resp("openai", resp.status_code, resp.text)
    try:
        data = resp.json()
    except Exception as exc:  # noqa: BLE001
        return err(f"embedding 响应非法 JSON: {exc}")
    items = data.get("data") if isinstance(data, dict) else None
    if not isinstance(items, list):
        return err("embedding 响应缺少 data 数组")
    # 按 index 排序：OpenAI 规范保证返回顺序，但网关实现未必，排序是零成本的稳健化
    items = sorted(
        [it for it in items if isinstance(it, dict) and isinstance(it.get("embedding"), list)],
        key=lambda it: it.get("index", 0),
    )
    if not items:
        return err("embedding 响应 data 为空")
    return norm_embed(
        [it["embedding"] for it in items],
        data.get("model") or body["model"],
        usage=data.get("usage") if isinstance(data.get("usage"), dict) else None,
    )


def resolve_embed() -> dict:
    """embedding 端点解析：EMBEDDING_* 优先，缺省回落 LLM_*（同网关同密钥时零配置可用）。"""
    base = os.environ.get("EMBEDDING_BASE_URL") or os.environ.get("LLM_BASE_URL", "https://api.openai.com/v1")
    key = os.environ.get("EMBEDDING_KEY") or os.environ.get("OPENAI_API_KEY", "")
    return {
        "base_url": base.rstrip("/"),
        "model": os.environ.get("EMBEDDING_MODEL") or "text-embedding-3-small",
        "headers": {"Authorization": f"Bearer {key}"} if key else {},
    }


def models(payload: dict) -> dict:
    require_httpx()
    import httpx

    ep = resolve_openai()
    url = f"{ep['base_url']}/models"
    try:
        resp = httpx.get(url, headers=ep["headers"], timeout=30.0)
    except Exception as exc:  # noqa: BLE001
        return err(f"拉取模型列表失败: {type(exc).__name__}: {exc}")
    if resp.status_code >= 400:
        return err(f"获取模型列表 HTTP {resp.status_code}: {resp.text[:500]}")
    try:
        data = resp.json()
    except Exception as exc:
        return err(f"模型列表响应非法 JSON: {exc}")
    items = data.get("data", []) if isinstance(data, dict) else []
    ids: list = []
    meta: list = []
    for m in items:
        if not isinstance(m, dict):
            continue
        mid = m.get("id")
        if not mid:
            continue
        ids.append(mid)
        # 平台归属：网关 /v1/models 每项带 owned_by（多平台聚合时它就是来源平台）；
        # 通用 OpenAI 兼容站点（DeepSeek/OpenRouter…）也给 owned_by，缺省时退化为 id 前缀。
        # 前端靠它做「按平台分组折叠」——分组本身不在前端硬编码平台名，纯数据驱动。
        plat = m.get("owned_by") or (mid.split("/", 1)[0] if "/" in mid else "")
        ent: dict = {"name": mid}
        if isinstance(plat, str) and plat:
            ent["platform"] = plat
        dn = m.get("display_name")
        if isinstance(dn, str) and dn and dn != mid:
            ent["display_name"] = dn
        ctx = m.get("context_length") or m.get("context_window")
        if isinstance(ctx, int) and not isinstance(ctx, bool) and ctx > 0:
            ent["ctx_limit"] = ctx
        if mid.startswith("auto/"):
            # 网关路由组模型（auto/<slug>）：UI 单列一个「auto（路由组）」栏，不与平台组混列
            # （owned_by 只是它默认落到的平台，不是分组归属）
            ent["route"] = "auto"
        meta.append(ent)
    out: dict = {"ok": True, "models": ids}
    if meta:
        out["models_meta"] = meta
    return out


PROVIDER = {
    "name": "openai",
    "chat": chat,
    "models": models,
    "embed": embed,
    "capabilities": {"stream": True, "tools": True, "images": True, "embed": True, "models": True},
    "requires_env": ["OPENAI_API_KEY"],
}
