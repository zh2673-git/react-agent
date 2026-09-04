# llm-adapter（Python guest）

把"跟哪家大模型说话"收口在一个插件里：`agent-loop` 只认 `llm.chat` 这一个 capability，
供应商差异（OpenAI 兼容 / Anthropic / ollama / mock）全部在本目录消化。

## 文件结构

| 文件 | 职责 |
|---|---|
| `llm_plugin.py` | 瘦分派层：op 分派 + provider registry 查找。**新增供应商不改这个文件** |
| `providers/base.py` | pack 协议 + 公共工具：`err` / `norm` / `as_object` / `require_httpx` / `map_usage` / `StreamSink` |
| `providers/registry.py` | 导入期自动发现 `providers/*.py`，校验 `PROVIDER` 形状，重名即拒 |
| `providers/openai_compat.py` | OpenAI 兼容族共享实现（openai / DeepSeek / Moonshot…），`_chat_with` 是唯一入口 |
| `providers/ollama.py` | ollama：复用 `openai_compat._chat_with`，只换端点解析（`OLLAMA_HOST` → `/v1`），本地免 key |
| `providers/anthropic.py` | Anthropic 原生协议 |
| `providers/mock.py` | `MOCK_SCRIPT` 脚本化响应（测试 / 演示） |

## op 契约（线契约 03 §2.2）

```jsonc
// chat
{"op":"chat","provider"?,"messages":[...],"tools"?:[...],"stream_path"?,"sid"?}
→ {"ok":true,"content":str|null,"tool_calls":[{"id","name","arguments":object}],
   "model":str,"finish_reason":str,
   // 追加可选字段：缺省即 provider 未提供，旧消费者无感知
   "reasoning"?:str, "usage"?:{...}, "elapsed_ms"?:int}
→ {"ok":false,"error":{"code","message"}}

// configure（运行时热配置）
{"op":"configure","provider"?,"model"?,"base_url"?,"api_key"?}
→ {"ok":true,"applied":{...}}   // api_key 只回 api_key_set:true，明文不回显、不落日志

// models.list
{"op":"models.list","provider"?} → {"ok":true,"models":[str]}
```

## 新增一个 provider（标准步骤）

1. 新建 `providers/<name>.py`，暴露模块级常量：

   ```python
   PROVIDER = {"name": "<name>", "chat": chat, "models": models, "requires_env": [...]}
   ```

   - `chat(payload) -> dict` 签名**必须只有一个参数**（`registry._validate` 会校验）。
   - 异常直接抛出，由分派层兜底成 `{"ok":false,...}`。
2. 缺 env 在 `chat` 内自行报错（`requires_env` 只是文档）。
3. `registry` 在导入期自动发现——**不需要改 `llm_plugin.py`**。
4. 若引入新的环境变量，同步更新：
   - `llm_plugin._configure` 的 env 路由表
   - `crates/host/src/config.rs` 的 `llm_env()` / `passthrough_env()`
   - `crates/host/web-dist/index.html` 的设置面板（provider 切换时字段自适应）

## provider 选路与 base_url 路由

provider 优先级：**请求 `payload["provider"]` > 环境变量 `LLM_PROVIDER` > `mock`**。

`_configure` 按**本次配置后的生效 provider** 把 `base_url` 路由到不同 env：

| provider | base_url → env | api_key → env |
|---|---|---|
| `openai` 兼容（可指 DeepSeek 等） | `LLM_BASE_URL` | `OPENAI_API_KEY` |
| `anthropic` | `ANTHROPIC_BASE_URL` | `ANTHROPIC_API_KEY` |
| `ollama` | `OLLAMA_HOST`（默认 `localhost:11434`） | 不接受（传了报字段级 400） |

## 流式旁路

内核 guest 是 unary gRPC：插件在处理过程中**无法**向宿主推任何东西。
所以"边生成边显示"走**文件旁路**：

```
llm-adapter（子进程）                              host（本进程）
  httpx.stream 逐 chunk ─▶ StreamSink.write ─▶ .stream/<session>.jsonl ─▶ SSE tail ─▶ 浏览器
                                                    流式期 50ms 轮询，空闲 300ms
```

- **触发条件**：`payload` 带 `stream_path`（绝对路径）+ `sid`。不带就是流式改造前的行为（一次请求一次响应）。
- 路径与 sid 由 `agent-loop` 生成（`crates/agent-loop/src/lib.rs` 的 `stream_file_for`），
  session 名过安全校验（防路径穿越）。host 侧同规则见 `crates/host/src/config.rs` 的 `stream_file`——**两边规则必须一致**。
- 旁路行协议（`base.StreamSink`）：

  ```jsonc
  {"type":"start","sid":..,"ts":..}
  {"type":"delta","sid":..,"ts":..,"kind":"reasoning"|"text","text":".."}
  {"type":"end","sid":..,"ts":..,"usage":{..},"elapsed_ms":..}
  {"type":"error","sid":..,"ts":..,"message":".."}
  ```

- host 侧读取在 `crates/host/src/frontend.rs` 的 `sse_events`，会把 `type` 加 `stream_` 前缀后推送
  （避免与 trace 事件撞名，例如 `error`）。
- **旁路不落持久化**：刷新页面靠 memory 的 trace 重放——最终 `assistant` 事件含完整
  answer + reasoning + usage + elapsed_ms，与流式所见一致；前端靠 `sid` 复用同一气泡，不会重复渲染。
- 写盘成本：实测 append+flush ≈ **43µs/条**，相对 LLM 20~100ms/token 可忽略；`StreamSink` 按 50ms 攒批 flush。
- 新一轮以 `"w"` 覆盖重写（历史轮次内容已在 trace 里，旁路只服务"边生成边看"）。

### SSE 解析规则（改这里前先看 deepseek-harness 的约定）

- 思考通道字段：`reasoning_content`（DeepSeek）或 `reasoning`（部分网关）。
- **首 chunk 常带空串 `reasoning_content: ""`，不得据此开思考块**——空值一律跳过。
- 顺序：思考先、正文后；各自独立块。
- `finish` 以 `[DONE]` 为准，不是见 `finish_reason` 就结束。
- usage 取末帧（靠 `stream_options:{"include_usage":true}`）。**部分自托管网关不认该参数**：
  遇 400 时自动去掉重试一次（`_stream_once(include_usage=False)`），
  重试复用同一 sink，因此 `start` 行不会重复写、前端气泡不会重开。

## usage 归一化（`base.map_usage`）

与 deepseek-harness 的 `mapUsage` 同约定——**计数互斥不相交**：

```jsonc
上游: {prompt_tokens: 283, completion_tokens: 69,
       prompt_tokens_details:{cached_tokens:256},
       completion_tokens_details:{reasoning_tokens:24}}
映射: {input_tokens: 27,   // 283 - 256（上游 prompt_tokens 含缓存命中）
       output_tokens: 69, cache_read_tokens: 256, reasoning_tokens: 24}
```

**provider 未上报 usage 时返回 `None`**，而不是全 0——前端据此不显示统计条。
多轮 ReAct 的累计在 `agent-loop` 的 `UsageAcc` 里做（用户要看的是总消耗）。

## 已知坑（都踩过，别再踩）

1. **`_chat_with` vs `chat_with`**：共享实现叫 `_chat_with`（下划线开头）。
   `openai_compat.chat` 和 `ollama.chat` 都曾误写成 `chat_with`，结果是启动探测就 `NameError`，进程直接退出。
2. **`err` 未导入**：`ollama.py` 用到 `err` 就必须 `from .base import err`，
   否则只在真的出错/拉模型时才炸（`NameError`），静态检查发现不了。
3. **非思考模型没有 `reasoning_content`**：完全没该字段 = 非思考模式，不开思考块，属正常现象，不是 bug。
4. **reasoning-only 的 assistant 消息**：回传历史时 `content` 必须是 `""` 不能用 `null`
   （deepseek-harness 的血泪教训：`null` 会毒化会话日志，后续每轮都废）。
5. **ollama 的 OpenAI 兼容端点**：`base_url` 是 `http://<OLLAMA_HOST>/v1`，本地免 key。
