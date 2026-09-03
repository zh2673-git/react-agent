# 04 llm-adapter 四层设计（provider pack 化）

> 锚定父本质：「在内核插件契约的时空约束下，本模块的本质是**供应商协议差异的归一化器**——固定归一化契约（essence），灵活各家实现（form）。」

## 1. 四层结构

| 层 | 设计 |
|---|---|
| 数据规范 | Provider 协议（base.py）：`name() -> str`；`chat(payload) -> 归一化响应`；归一化响应形状 = `{"ok":true,"content":str|null,"tool_calls":[{"id","name","arguments":object}],"model":str,"finish_reason":"stop"\|"tool_calls"}`（与线契约一致）；错误统一 `{"ok":false,"error":{"code","message"}}` |
| 数据存储 | **无状态**。唯一例外：mock pack 的脚本序列指针（pack 内局部变量，init 重置） |
| 数据流转 | 请求级顺序管道：op 校验 → registry 查 pack（payload.provider 覆盖 env 缺省）→ pack.chat → 异常捕获落 payload |
| 数据接口 | `llm.chat`（线契约逐字不变）；pack 间互不可见 |

## 2. 递归子模块（providers/）

| Pack | 覆盖范围 | 关键归一化点 |
|---|---|---|
| `openai_compat.py` | OpenAI / DeepSeek / Moonshot 等（`LLM_BASE_URL` 换端点即接入） | `tool_calls[].function.arguments` JSON 字符串 → object |
| `anthropic.py` | Anthropic Messages 协议 | system 前置合并；tool 结果 → `tool_result` block；`tool_use.input` 已是 object |
| `ollama.py` | 本机 Ollama（OpenAI 兼容端点） | base_url 由 `OLLAMA_HOST` 推导，无鉴权头 |
| `mock.py` | 离线测试 | `MOCK_SCRIPT` JSON 数组逐次弹出；无脚本回 `pong` |

`registry.py`：启动时扫描 pack 模块的 `PROVIDER` 导出，按名索引；**名字冲突或形状不符在导入期抛错**（规则契约：不合形状的 pack 无法被选中）。新供应商 = 新增一个文件，零改动已有代码（开闭验收项）。

## 3. 生命周期钩子

| 钩子 | 行为 |
|---|---|
| `init` | 重置 mock 序列指针；空扫描校验（至少存在 mock pack） |
| `start`/`stop` | 无（Serial 语义，无常驻资源） |
| `destroy` | 空（无持久的网络/文件资源） |

## 4. 升级路径（单 pack → 独立 guest）

当某 pack 需要独立生命周期（如本地 Ollama 常驻、重依赖隔离）：按同一 `base.py` 协议实现 guest 入口 → host 以新 capability（如 `llm.chat.ollama`）注册 → `llm.chat` 线契约与 agent-loop 零改动。本 Phase 不实施，仅保留协议兼容。

## 5. 验证点（Q）

- 既有 mock 脚本回放 e2e 全绿（I：契约不变）；
- 新增 pack 演练：复制 `openai_compat.py` 改名换端点 → registry 自动发现 → chat 走通；
- 归一化单测：三家响应形状断言（content/tool_calls/finish_reason）。
