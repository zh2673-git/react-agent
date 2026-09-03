# react-agent

基于 [agent-kernel](https://github.com/zh2673-git/agent-kernel)（v0.1.0，git 依赖）构建的下游 agent 项目：**Rust InProcess 编排 + Python/TS 跨语言插件（gRPC Process 域）** 的 ReAct 式 agent。

```
宿主(装配内核+前端) ──dispatch──> agent-loop(Rust, InProcess)
                                      │ call_plugin（按 Envelope.target 路由，跨域）
        ┌──────────────────┬──────────┼──────────────────┐
        ▼                  ▼          ▼                  ▼
  llm-adapter(Python)   tools(Python) assets(Python)   memory(TypeScript)
  provider pack 注册表   生产级 7 工具  skills/prompts   会话消息+事件日志
  (openai/anthropic/    +越界拦截     注册表(渐进披露)  (JSON/JSONL)
   ollama/mock)         +免费搜索链
```

- 内核只做：插件装载隔离、执行编排、契约/权限校验；一切能力皆为插件
- ReAct 循环：感知(读记忆)→规划(LLM+工具清单)→行动(执行工具/保留名路由)→观察(写回记忆)→收敛；全程发射事件（trace）+ 逐轮进度回显
- 三家 LLM 全覆盖：OpenAI 兼容（可换 base_url 适配 DeepSeek 等）、Anthropic、Ollama；另带 **mock** provider 供离线测试
- 生产级工具 7 件：read_file / write_file / edit_file / list_dir / bash / web_search / web_read（全部免费默认无 key）
- 双前端：REPL（默认）/ Web 网关（HTTP+SSE，dsh 风格事件流式会话，刷新恢复=日志重放）
- subagent：保留工具 `task` 委派子任务（新 session 复用全链路，深度防嵌套）

## 目录

```
crates/agent-loop        ReAct 编排插件（InProcess，仅依赖 agent-kernel-sdk）
crates/host              宿主二进制：装配、spawn、探测、双前端、sandbox-run 助手
plugins/llm_adapter      LLM 适配器（Python guest，providers/ 按 vendor 分 pack）
plugins/tools            工具注册与执行（Python guest，纯 stdlib，files/bash/web 分文件）
plugins/assets           skills/prompts 注册表（Python guest，开放标准 SKILL.md）
plugins/memory           会话记忆 + 事件日志（TS guest，strip-types）
docs/                    方案与设计文档（01 总纲 / 02 架构 / 03 模块契约 / 04-07 分模块四层设计）
```

## 环境准备（一次）

```bash
pip install grpcio httpx                                  # python guest 需要
cd ../agent-kernel/bindings/typescript && npm install     # TS guest 从内核仓解析 @grpc 依赖
```

- 前置：Rust、Python 3、Node ≥ 22.6（`--experimental-strip-types`）
- 内核 checkout 默认取本目录旁的 `../agent-kernel`，可用 `AGENT_KERNEL_REPO` 覆盖
- 本地开发内核：取消 `Cargo.toml` 里 `[patch]` 段注释

## 运行

```bash
# 离线（默认 mock provider，无需任何 key）
cargo run -p react-agent-host -- "用 bash 算一下 128*64"
cargo run -p react-agent-host                          # 无参数进入 REPL

# Web 前端（dsh 风格事件流式会话，浏览器打开 http://127.0.0.1:8710）
REACT_FRONTEND=web cargo run -p react-agent-host

# Ollama（本机，推荐支持工具调用的模型如 qwen2.5 / llama3.1+）
LLM_PROVIDER=ollama LLM_MODEL=qwen2.5:7b cargo run -p react-agent-host -- "..."

# OpenAI 兼容（OpenAI/DeepSeek 等）
LLM_PROVIDER=openai LLM_BASE_URL=https://api.deepseek.com/v1 \
OPENAI_API_KEY=sk-xxx LLM_MODEL=deepseek-chat cargo run -p react-agent-host -- "..."

# Anthropic
LLM_PROVIDER=anthropic ANTHROPIC_API_KEY=sk-ant-xxx cargo run -p react-agent-host -- "..."
```

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `LLM_PROVIDER` | `mock` | mock / openai / anthropic / ollama（也可按请求覆盖） |
| `LLM_MODEL` | 按 provider | 模型名 |
| `LLM_BASE_URL` | `https://api.openai.com/v1` | openai 兼容端点 |
| `OLLAMA_HOST` | `localhost:11434` | ollama 地址 |
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | — | 密钥（经子进程 env 传递，不落 manifest） |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Anthropic 端点 |
| `MOCK_SCRIPT` | — | mock provider 脚本（JSON 数组，逐次弹出） |
| `MAX_ROUNDS` | `8` | ReAct 最大轮数 |
| `SESSION_ID` | `default` | REPL/单轮会话 id |
| `AGENT_KERNEL_REPO` | `../agent-kernel` | 内核 checkout（PYTHONPATH/TS shim 解析） |
| `PLUGINS_DIR` | `<workspace>/plugins` | guest 脚本目录 |
| `MEMORY_DATA_DIR` | `plugins/memory/data` | memory 会话与事件日志持久化目录 |
| `WORKSPACE_ROOT` | 进程 cwd | 文件工具越界拦截根（realpath 前缀校验） |
| `TOOLS_ENABLED` | 全开 | 逗号分隔白名单，如 `read_file,write_file,bash` |
| `SEARCH_REGION` | `cn` | cn（Bing→搜狗→百度零 key 直连）/ global（ddgs→DDG→Bing） |
| `SEARCH_BACKEND` | 自动 | 强制指定引擎：bing / sogou / baidu / ddgs / duckduckgo / bocha / baidu_ai / tavily |
| `BOCHA_API_KEY` / `BAIDU_API_KEY` / `TAVILY_API_KEY` | — | 可选升级搜索后端 |
| `SKILLS_DIR` / `PROMPTS_DIR` | `<workspace>/plugins/assets/{skills,prompts}` | 资产目录 |
| `AGENT_SYSTEM_PROMPT` / `SYSTEM.md` / `PROMPT` | 内置缺省 | 提示词覆盖优先级：env > SYSTEM.md > 具名模板 > 内置 |
| `HISTORY_LIMIT` | 不限 | 每轮回传历史条数上限 |
| `COMPACT_TRIGGER` / `COMPACT_KEEP` | `40` / `10` | 上下文压缩触发阈值（0=禁用）/ 保留最近条数 |
| `BASH_SANDBOX` | `on` | on=sandbox-run 受限令牌沙箱（探测失败 fail-closed 移除 bash）；off=显式豁免直跑 |
| `REACT_FRONTEND` | `repl` | 前端选择：repl / web |
| `WEB_ADDR` | `127.0.0.1:8710` | web 网关监听地址 |
| `RUST_LOG` | `warn,react_agent_host=info` | 日志 |

## 测试

```bash
cargo test -p react-agent-agent-loop   # 纯 Rust mock 测试（无需 python/node）
cargo test --workspace                 # 全量（含跨语言 e2e，缺解释器自动 skip）
```

e2e：tools(7 工具往返)、memory(append/get/clear/summarize)、llm(mock 脚本)、**全链路 ReAct**、上下文压缩、web 网关、subagent（委派+嵌套拒绝）。

> 注意：若测试失败提前退出，guest 子进程可能残留（占用内存无害）；可用 `Get-Process python,node` 检查清理。测试内已将 guest stderr 指向 null，cargo 不会再被泄漏进程扣住。

## Wire 契约（Contracts）

跨插件 payload 均为 JSON；业务错误走 payload 内 `{"ok":false,"error":{...}}`，`KernelError` 仅承载传输/生命周期失败。路由按 `Envelope.target`；op 分派在 payload 的 `"op"` 字段。

**agent-loop**（`agent.chat`）
- req `{"op":"chat","session_id":str,"user_text":str}`
- resp `{"ok":true,"answer":str,"rounds":int,"steps":[{"round":int,"tool":str,"ms":int}],"session_id":str}` | `{"ok":false,"error":{...}}`
- 保留工具名（不进 tools.list，由 agent-loop 路由）：`load_skill`→assets `skills.load`；`task`→子代理（新 session 复用 agent.chat，深度防嵌套）

**llm-adapter**（`llm.chat`）
- req `{"op":"chat","provider"?:"openai"|"anthropic"|"ollama"|"mock","messages":[Msg],"tools"?:[ToolSpec]}`
- Msg = `{"role":"system"|"user"|"assistant"|"tool","content":str|null,"tool_calls"?:[{"id","name","arguments":object}],"tool_call_id"?:str}`
- ToolSpec = `{"name","description","parameters":json-schema}`
- resp `{"ok":true,"content":str|null,"tool_calls":[{"id","name","arguments":object}],"model":str,"finish_reason":"stop"|"tool_calls"}`

**tools**（`tools.exec`）
- `{"op":"list"}` → `{"ok":true,"tools":[ToolSpec]}`（生产级 7 件：read_file / write_file / edit_file / list_dir / bash / web_search / web_read，受 `TOOLS_ENABLED` 裁剪）
- `{"op":"call","name":str,"args":object}` → `{"ok":true,"result":any}` | `{"ok":false,"error":{"code","message","field"?}}`（字段级错误：哪个参数错、合法值是什么）

**assets**（`assets.registry`）
- `{"op":"skills.list"}` → `{"ok":true,"skills":[{"name","description"}]}`
- `{"op":"skills.load","name":str}` → `{"ok":true,"content":str}` | `{"ok":false,"error":{...}}`
- `{"op":"prompts.list"}` → `{"ok":true,"prompts":[{"name","description"}]}`
- `{"op":"prompts.get","name":str}` → `{"ok":true,"content":str}`

**memory**（`memory.session`）
- `{"op":"append","session_id":str,"messages":[Msg]}` → `{"ok":true,"count":int}`
- `{"op":"get","session_id":str,"limit"?:int}` → `{"ok":true,"messages":[Msg]}`
- `{"op":"clear","session_id":str}` → `{"ok":true}`
- `{"op":"summarize","session_id":str,"summary":str,"keep_last"?:int=10}` → `{"ok":true,"count":int}`（上下文压缩：历史替换为压缩标记 + 最近 keep_last 条，孤儿 tool 消息防撕裂）

**memory**（`session.trace`，只追加事件日志）
- `{"op":"trace.append","session_id":str,"events":[Event...]}` → `{"ok":true,"count":int}`
- `{"op":"trace.read","session_id":str,"after"?:int=0}` → `{"ok":true,"events":[Event],"next":int}`
- Event 建议形状 `{type, ts, ...}`；存储 `<MEMORY_DATA_DIR>/traces/<session>.jsonl`

**web 网关**（host 级，非插件）
- `GET /` → 单页（dsh 风格事件流式会话）
- `GET /api/events?session=&after=` → SSE（从 0 全量重放 + 实时增量）
- `POST /api/chat` body `{"session_id":str,"message":str}` → 阻塞至收敛，回 agent.chat 响应

## 架构要点（内核约束的落点）

- **agent-loop 必须在 InProcess 域**：Process guest 无 guest→host 回调，跨插件调用只有进程内 `HostApi::call_plugin` 可用（可跨域调 Process 插件）
- **注册顺序**：memory → llm-adapter → tools → assets → agent-loop。内核 `register` 对「硬依赖无 provider」静默失败（K302），故 host 对每个 provider 先探测再注册编排插件
- **guest api_version 必须 (0,1)**：gRPC 握手要求 guest major==host major 且 guest minor ≥ host minor
- **配置走环境变量**：内核 Init 不传业务配置，子进程继承宿主 env
- **循环无跨调用可变态**：ReAct 状态在局部变量 + memory 插件，插件本体 `&self`（A1）；每步转发带 deadline（A2）
- Rust 插件仅依赖 `agent-kernel-sdk`；仅 host 依赖 kernel（process feature）+ process —— 规则防穿透
