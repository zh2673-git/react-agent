# react-agent-host（Rust 宿主）

react-agent 的运行时：拉起全部 guest 插件、暴露 Web UI 与 SSE、把内核事件转成浏览器能消费的事件流，
并把"边生成边显示"的 LLM 旁路文件 tail 出来推给前端。

## 文件结构

| 文件 | 职责 |
|---|---|
| `main.rs` | 二进制薄入口：`assemble`（spawn + 探测 + 注册）、CLI 参数、会话 id、启动 Web |
| `lib.rs` | 模块重导出（config / frontend / manifests / spawn），供 e2e 测试复用 |
| `config.rs` | `HostConfig`：env 解析、持久配置（`config.json`）、`llm_env()` / `passthrough_env()` / `stream_file()` / `skills_dir()` / `workspace_root()` / `ALL_TOOL_NAMES` |
| `frontend.rs` | Web 服务（HTTP + SSE）、静态资源（`web-dist/`）、`sse_events` 合并 trace + 流式旁路 |
| `manifests.rs` | `guest_manifest`：Process 域 manifest 构造（api_version 必须 0.1） |
| `spawn.rs` | guest 子进程 spawn（python / node），注入 `PYTHONPATH` 与 provider env |
| `bin/sandbox-run.rs` | bash 沙箱助手（与宿主二进制同目录；`tools` 插件的 `BASH_SANDBOX=on` 依赖它） |

## 启动流程（`assemble`）

1. 探测 `node`（>= 22.6，strip-types）与 `python`（有 grpcio）。
2. 按序 spawn：**memory(ts) → llm-adapter(py) → tools(py) → assets(py)**，每个先 spawn 再探测，
   失败即退出并给可读原因（`Kernel::register` 对 K302 静默失败，所以必须先探测）。
3. agent-loop 是 InProcess，在 crate 内自注册。
4. 起 Web：`frontend::run`。

## 配置

- 来源优先级：CLI `--local` / `--port` / `--session` > env > `config.json`（持久项，宿主启动时会写回 3 项）。
- `config.rs::llm_env()` / `passthrough_env()`：`LLM_*`、`OLLAMA_HOST`、`OLLAMA_ENDPOINT`、`ANTHROPIC_*`、`SEARCH_*`、`BASH_SANDBOX` 等，
  **新增 provider 环境变量要在这里下发**（否则 guest 收不到）。
- `config.json` 持久化项与前端 `/api/config` 读写对应（工具白名单、模型、provider 等）。

## Web 与 SSE

- 静态根：`crates/host/web-dist/`（运行时 serve，非编译内嵌——改 HTML 刷新即生效）。
- `/api/*`：配置读写、会话、技能 CRUD 等（见 `frontend.rs`）。
- `/api/events`（SSE）：`sse_events` 每 50~300ms（流式进行中）/ 300ms（空闲）轮询一次：
  - `memory.session.trace.read`（`after` 游标推进）→ 转成 `trace_*` 事件；
  - `.stream/<session>.jsonl` 旁路 → 转成 `stream_*` 事件（`start` / `delta` / `end` / `error`，
    加前缀避免与 trace 撞名），按 `sid` 对位到同一气泡。
- 前端靠 SSE 全量重放（`after=0`）实现刷新恢复；trace 是唯一持久化事件源，流式旁路只服务"边生成边看"。

### 数据流设计要点（断线 / 刷新不重绘、不重复）

- **重放 vs 实时分阶段**：首屏 / 会话切换 `after=0` 全量重放时，`sse_events` 在 trace 还没 catch up 前
  （`replaying=true`）只推持久 `trace_*` 事件、**不推 `stream_*`**（最终 `assistant` 已含完整内容）；
  一旦某批 `trace.read` 返回空（已追平），`replaying=false`，此后开始 tail 流式旁路。
- **前端增量续传**：`EventSource` 断线时 `onerror` 走 `connect(lastAfter)`（持久事件数即游标），
  **不清空 DOM、只补新增**；`lastAfter` 仅在收到非 `stream_*` 的持久事件时自增，
  因此断连重连不会叠加整段、也不会整段重绘。
- **心跳 5s**：`: ping` 注释行防中间层空闲断连（此前 15s 在弱网易被掐）。
- **`doneSids` 去重（前端）**：某 sid 的最终 `assistant` 事件渲染后，记入 `doneSids`；
  其后重连推来的同 sid `stream_*` 帧一律忽略——避免「已完成回合被重连的流式尾部覆盖成只剩最后几个字」。
  收到新 `user` 消息或首屏清空时作废旧标记，防止同 round 的 sid 碰撞误伤下一回合流式动画。
- 响应头 `content-type: text/event-stream; charset=utf-8`（EventSource 本强制 UTF-8，显式声明让 DevTools 等工具正确显示中文）。

## 流式目录

- `AGENT_STREAM_DIR`（默认 `<项目根>/.stream`）：宿主 `main.rs` 创建并下发，agent-loop 据此生成旁路路径。
- `config.rs::stream_file(session)` 与 agent-loop 的 `stream_file_for` **规则必须一致**（session 名净化防穿越）。
- 该目录已加 `.gitignore`，运行时产物不进版本库。

## 改动时的联动点

- 新插件环境变量 ↔ `config.rs::llm_env()` / `passthrough_env()` ↔ `plugins/README.md` 环境变量总表。
- 旁路路径规则 ↔ `crates/agent-loop/src/lib.rs::stream_file_for`。
- 事件类型 ↔ `web-dist/index.html::render()`。
- `ALL_TOOL_NAMES` ↔ `plugins/tools/README.md`（工具增删要同步）。
- 设置面板字段 ↔ `plugins/llm_adapter/README.md`（provider 切换自适应）。

## 已知限制

- **运行中断（停止）**：已支持（P2/T1）。`POST /api/chat/cancel?session=` 转发 agent-loop `cancel` op
  （置位取消标志，立即返回）；前端发送期间显示「停止」按钮；循环在轮次边界命中即以 K499 收敛
  （半轮不中断——进行中的 LLM 流式与工具执行照常完成）。详见 agent-loop README「运行中断（停止）」。
- **sid 跨对话不保证唯一**：同 round 的不同对话回合 sid 相同，重连去重（`doneSids`）可能误忽略后一回合的
  流式动画（直接显示最终答案）。属已知退化，不影响最终内容完整性。
