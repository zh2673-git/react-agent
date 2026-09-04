# agent-loop（Rust，InProcess 编排插件）

react-agent 的大脑：把"用户一句话"变成"多轮感知 → 规划 → 行动 → 观察 → 最终答案"的 ReAct 循环。
编译进宿主，是唯一的 InProcess 插件；所有跨插件调用都经 `HostApi::call_plugin`。

## 文件结构

| 文件 | 职责 |
|---|---|
| `lib.rs` | 编排主循环、ReAct 状态机、事件发射、上下文压缩、技能自扩展、流式 `sid` / 旁路编排 |
| `contract.rs` | 跨边界数据形状：`MemoryMsg` / `ChatReq` / `LlmChatResp` / `ToolCall` / `ToolSpec` / `StepRecord` |

## 契约与依赖

- **硬依赖**：`memory.session` / `llm.chat` / `tools.exec`（host 注册顺序 memory → llm-adapter → tools → agent-loop）。
- **软依赖**：`assets.registry`——不可用时降级：无技能附录、具名提示词模板不可用、`load_skill` 回字段级错误。
- 时间契约：主执行流是状态机循环，收敛于最终答案或 `max_rounds` 强制停止；循环状态只存局部变量与会话记忆，
  本体 `&self` 无跨调用可变态（`A1`）。
- 空间契约：跨插件通信只走 `call_plugin`，**不**碰别插件的状态。

## 核心循环

每轮一次 `llm.chat`（带当前 memory 全量历史 + 可用工具）：

- 返回 `tool_calls` → 逐条分发执行（`load_skill` 路由 assets，其余路由 tools），结果写回 memory，进入下一轮；
- 返回纯文本 → 收敛，发 `assistant` 事件给用户，结束。

- `max_rounds` 来自构造参数（默认见 `main.rs`），超过则强制收敛并返回错误 payload（不抛异常）。
- **保留名路由**（03 §3）：`load_skill`（→ assets `skills.load`）、`task`（→ 子会话 `agent.chat`，委派深度 >1 拒绝再嵌套）。

## 事件（发到 `memory.session.trace`，`type` 字段）

浏览器 SSE 就是轮询这些 trace 事件。**新增 / 改事件类型要同步前端 `render()`**：

- `chat_start` / `user` / `thinking`（规划摘要）/ `plan` / `tool_call` / `tool_result` / `assistant` / `error`
- 最终 `assistant` 事件承载完整答案，并附 `reasoning` / `usage` / `elapsed_ms`（多轮累计；
  字段缺省即 provider 未提供，旧消费者无感知）。
- `error` 事件含人类可读原因，前端据此以红条收尾。

## 上下文压缩（summarize）

- 历史超过 `COMPACT_TRIGGER`（默认 40 条，`0` = 禁用）且本次 LLM 调用返回 `tool_calls` 时触发。
- 调 `memory.summarize`，保留最近 `COMPACT_KEEP`（默认 10）条；防撕裂逻辑在 memory 插件侧。
- 压缩那次 LLM 调用**不带流式旁路**（不是给用户看的产出）。

## 技能自扩展

- **L1（模型写入技能）**：当 `skills.list` 返回的 `root` 落在 `WORKSPACE_ROOT` 内，系统提示词追加
  "可读写 `<root>` 的 SKILL.md 自创技能"授权段。
- **L2（动态工具）**：模型 / Web 经 `tools.reload` + `tools.configure` 热加载新工具（装载 ≠ 启用）。

## 流式编排（旁路文件）

内核 guest 是 unary gRPC，插件处理中推不了事件。本插件负责"每轮生成 `sid` + 旁路路径"：

- `stream_file_for(session, round)`：生成 `.stream/<session>.jsonl` 绝对路径（session 名过安全校验）。
  **规则必须与 host 侧 `config.rs::stream_file` 完全一致**，否则前端读不到。
- 每轮 `llm.chat` 的 `payload` 带 `stream_path` + `sid`；首个 `thinking` / `plan` 阶段即 `startStream`，
  后续 `assistant` 增量经 `stream_delta` 显示，最终 `assistant` 事件带完整内容。
- 多轮 `usage` 在 `UsageAcc` 里累计（用户看总消耗）；`elapsed_ms` 取本轮 LLM 耗时。

> **sid 唯一性（已知限制）**：当前 `sid` 形如 `s-{rand}-{round}`，`round` 是 ReAct 轮次，**跨不同对话回合
> 不保证唯一**——同一会话连续两轮若都只有 1 轮 ReAct，会生成相同 sid（如 `s-xxx-r1`）。前端按 sid 去重时
> 可能误把后一回合的流式动画当作「已完成重连帧」忽略（直接显示最终答案，无打字效果）。若要全局唯一，
> 建议 sid 改为含会话内自增序号或时间戳（如 `s-{rand}-t{mono_inc}`），并同步 host `stream_file` 规则。

## 改动时的联动点

- 数据形状 ↔ `contract.rs`（被 host / frontend / memory 镜像消费）。
- 事件 `type` ↔ 前端 `crates/host/web-dist/index.html::render()`。
- 旁路路径规则 ↔ `crates/host/src/config.rs::stream_file`。
- 新增硬依赖 capability ↔ host 注册顺序（`crates/host/src/main.rs`）。

## 运行中断（停止）—— 当前未支持

ReAct 主循环从 `llm.chat` 阻塞直到收敛，循环内**没有取消检查**；`chat` 事件发射完才结束。
`POST /api/chat` 也是阻塞式（host 侧等 agent.chat 返回）。因此运行中的一轮**无法中途打断**——
关闭页面或杀进程即整体终止。要支持「停止」按钮，需要三处联动改造：

1. **agent-loop**：引入取消令牌（如 `Arc<AtomicBool>` 或 `oneshot::Receiver`），在每轮迭代开头、
   LLM 流式 `delta` 消费处、以及 `tools.exec`（长命令）处轮询；置位即中断循环、发 `error`/截断 `assistant` 事件。
2. **host**：增加取消接口（如 `POST /api/chat/cancel?session=` 写全局/按 session 的取消标志，
   或持有 `JoinHandle` 改为可中止），让取消信号能穿透到阻塞的 `agent.chat`。
3. **前端**：`index.html` 加「停止」按钮（与发送互斥），点击发取消请求，并撤销 / 标灰该轮未完成的流式气泡。

这是一个独立功能，不在当前增量 SSE 修复范围内。
