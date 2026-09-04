# plugins/ —— guest 插件总览

本目录是所有 **Process 域 guest 插件**的家：每个插件跑在独立子进程里，经 gRPC 与内核通信。
唯一的例外是 `agent-loop`——它是 InProcess 插件（编译进宿主），不在本目录。

## 架构位置

```
浏览器 ──HTTP/SSE──▶ host (crates/host) ──▶ Kernel
                                              │
        ┌──────────────┬──────────────┬───────┴────────┬──────────────────┐
        ▼              ▼              ▼                ▼                  ▼
   memory(ts)   llm-adapter(py)   tools(py)      assets(py)      agent-loop(InProcess)
  memory.session     llm.chat      tools.exec    assets.registry      （编排者）
  session.trace
```

- 跨插件通信**一律走 `HostApi::call_plugin`**，按 `Envelope.target` 路由，不直接触碰彼此状态。
- 插件之间**没有横向调用通道**：guest 只能"收一个请求 → 回一个响应"（见下方协议约束）。

## 插件清单

| 插件 | 语言 | capability | 入口 | 依赖性质 |
|---|---|---|---|---|
| [memory](./memory/README.md) | TS | `memory.session`、`session.trace` | `memory/memory_plugin.ts` | 硬 |
| [llm-adapter](./llm_adapter/README.md) | Python | `llm.chat` | `llm_adapter/llm_plugin.py` | 硬 |
| [tools](./tools/README.md) | Python | `tools.exec` | `tools/tools_plugin.py` | 硬 |
| [assets](./assets/README.md) | Python | `assets.registry` | `assets/assets_plugin.py` | 软（起不来仅 warn，不阻断） |

注册顺序固定为 **memory → llm-adapter → tools → assets → agent-loop**
（`crates/host/src/main.rs` 的 `assemble`）。原因：`Kernel::register` 对 K302 是静默失败，
所以每个 provider spawn 后宿主会**先探测再注册**，失败即退出并给出可读原因。

## 通用 guest 协议（改任何插件前必读）

- **传输**：子进程在 `127.0.0.1:0` 起 gRPC，首行 stdout 打印 `PORT=<n>`，内核据此回连。
- **语义：unary**。一次 `OnEvent` = 一次响应，没有 server-streaming，也没有 plugin→host 的反向回调。
  - 直接后果：插件**无法**在处理过程中向宿主推事件（连别的插件也调不到）。
  - 需要"边生成边外抛"（如 LLM token 流）时走旁路——见 [llm_adapter 的流式旁路](./llm_adapter/README.md#流式旁路)。
- **Python 运行时**：`PYTHONPATH=<内核仓>/bindings/python`。内核仓默认在 `react-agent` 同级的
  `agent-kernel`，可用 `AGENT_KERNEL_REPO` 覆盖。需 `grpcio`（llm-adapter 还需 `httpx`）。
- **TS 运行时**：`node --experimental-strip-types`（需 >= 22.6）。
- **Manifest**：`api_version` 必须是 `0.1`（握手要求 major 相同且 guest minor >= host minor）；
  `Semantics::Serial`——同步处理，插件内共享可变状态是安全的。host 侧构造见 `crates/host/src/manifests.rs`。
- 插件需实现四个方法：`manifest()` / `init(config)` / `on_event(envelope)` / `destroy()`。

## 错误约定

业务错误**一律落在 payload 内**，不用 gRPC 状态码：

```jsonc
{"ok": false, "error": {"code": "K400", "message": "...", "field": "..."}}
```

`field` 用于字段级定位（宿主设置面板据此高亮对应输入框）。`KernelError` 只承载传输/生命周期失败。

## 环境变量总表

细节见各插件 README。

| 变量 | 作用 | 读取方 |
|---|---|---|
| `AGENT_KERNEL_REPO` | 内核仓路径（Python SDK / TS shim 解析） | host |
| `PLUGINS_DIR` | 插件目录 | host |
| `LLM_PROVIDER` / `LLM_MODEL` / `LLM_BASE_URL` | LLM 选路与端点 | llm-adapter |
| `OLLAMA_HOST` | ollama 地址（默认 `localhost:11434`） | llm-adapter |
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` | 鉴权与端点 | llm-adapter |
| `MOCK_SCRIPT` | mock provider 脚本化响应 | llm-adapter |
| `AGENT_STREAM_DIR` | 流式旁路目录（默认 `<项目根>/.stream`） | host 下发 → agent-loop |
| `MEMORY_DATA_DIR` | memory 落盘目录 | memory |
| `TOOLS_ENABLED` | 工具白名单（逗号分隔） | tools |
| `WORKSPACE_ROOT` | 文件工具越界拦截根 | tools / assets |
| `BASH_SANDBOX` | bash 沙箱策略（缺省 on，fail-closed） | tools |
| `SEARCH_BACKEND` / `SEARCH_REGION` / `BOCHA_API_KEY` / `BAIDU_API_KEY` / `TAVILY_API_KEY` | 联网搜索 | tools |
| `SKILLS_DIR` / `PROMPTS_DIR` | 资产目录 | assets |

## 新增一个插件

1. 在 `plugins/<id>/` 建入口脚本，实现四方法（照抄任一现有插件的骨架）。
2. 在 `crates/host/src/main.rs` 的 `assemble` 里 spawn + 探测 + 注册，capability 名与调用方保持一致。
3. 若被 `agent-loop` 依赖，把插件 id 加到 `crates/agent-loop/src/lib.rs` 顶部常量。
4. 在本目录的插件下补 `README.md`，并登记到上表。
