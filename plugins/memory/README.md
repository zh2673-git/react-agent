# memory（TypeScript guest）

会话的**模型上下文**与**事件日志**两件事都在本插件，但它们是两个不同的关注点：

- **模型上下文**（`memory.session`）：给 LLM 看的历史消息，可被压缩。
- **事件日志**（`session.trace`）：给审计 / 恢复 / UI 重放看的只追加事件流，**不进模型上下文**。

设计原则（dsh：Model-visible means logged）：凡是模型看得见的东西，都要能被记录。

## 文件结构

| 路径 | 职责 |
|---|---|
| `memory_plugin.ts` | 全部 op 实现 + 落盘 |
| `guest_sdk.ts` | TS guest SDK 副本（`serve` / `Plugin` 类型） |
| `data/sessions/*.json` | 会话消息（每会话一个 JSON 文件） |
| `data/traces/*.jsonl` | 事件日志（每会话一个 JSONL，只追加） |

落盘根目录可用 `MEMORY_DATA_DIR` 覆盖，缺省为插件目录下的 `data/`。

## op 契约

```jsonc
{"op":"append","session_id","messages":[Msg]}         → {"ok":true,"count":int}
{"op":"get","session_id","limit"?}                     → {"ok":true,"messages":[Msg]}
{"op":"clear","session_id"}                            → {"ok":true}
{"op":"summarize","session_id","summary","keep_last"?} → {"ok":true,"count":int}
{"op":"trace.append","session_id","events":[Event]}    → {"ok":true,"count":int}
{"op":"trace.read","session_id","after"?}              → {"ok":true,"events":[Event],"next":int}
{"op":"rollback","session_id","upto_user_index":int}   → {"ok":true,"removed_messages":int,"removed_events":int}
```

- `Msg = {role, content?, tool_calls?, tool_call_id?, attachments?}`——与 `crates/agent-loop/src/contract.rs`
  的 `MemoryMsg` **严格镜像，改形状必须两边同步**。`attachments` 是 R3 多模态图片附件
  （`[{name,mime,data_b64}]`，仅图片；文本文件已在 agent-loop 构造时拼入 content），纯追加字段。
- **rollback（R2 回滚）**：物理截断语义——按「第 `upto_user_index` 条 user 消息」（0 基，压缩标记
  不计入轮次）定位切点，**消息与 trace 双侧同源截断**（任一侧越界即整体失败不落盘，保证 UI 重放
  与 LLM 上下文对齐）。trace 侧按 JSONL 字节偏移 truncate，容忍半行写入。
- `Event` 是任意 JSON 对象，约定带 `type` 与 `ts`。
- 所有 op 都要求 `session_id`，缺失直接返回 `{"ok":false,"error":{"message":"session_id is required"}}`。
- 未知 op 返回错误，**不抛异常**（gRPC 层异常会转成 INTERNAL）。

## 存储

- 内存 `Map<sessionId, Msg[]>` + 每会话一个 JSON 文件；每次写操作即时落盘。
- `Semantics::Serial` 保证同步读写安全，插件内不需要锁。
- session id 只作文件名：非 `[A-Za-z0-9._-]` 的字符一律替换为 `_`（防路径穿越）。
- `clear` 后消息为空时会删除对应文件。

## trace 语义（改这里要小心——UI 依赖它）

- **只追加、不可变**。
- `trace.read` 整体重读文件后按行切事件；`after=N` 返回第 N 条之后的事件，`next` 是新游标（UI 增量拉取）。
- 容忍半行 / 损坏行：解析失败的行跳过，不整体失败（写入中途被读到是正常情况）。
- **trace 是 UI 的唯一持久化事件源**：浏览器 SSE（`crates/host/src/frontend.rs` 的 `sse_events`）
  就是轮询 `trace.read`，`after=0` 起全量重放 = 刷新恢复。
- LLM 的流式增量走旁路文件（见 [llm_adapter](../llm_adapter/README.md#流式旁路)），**不写进 trace**；
  只有最终 `assistant` 事件会落 trace。所以刷新后看到的是完整消息，而不是当时的逐字过程——这是刻意设计。

## summarize（上下文压缩）

- 由 `agent-loop` 在历史超过 `COMPACT_TRIGGER`（默认 40 条，`0` = 禁用）时调用。
- 结果 = 一条压缩标记消息 + 最近 `keep_last` 条（默认 10）。
- **防撕裂**：切片后丢弃开头的孤儿 `tool` 消息（其 `assistant(tool_calls)` 载体已被裁掉），
  否则历史里会出现没有归属的 tool 结果，模型会困惑。

## 改动时的联动点

- `Msg` 形状 ↔ `crates/agent-loop/src/contract.rs::MemoryMsg`（必须镜像）。
- 新增 op ↔ `agent-loop` 的调用处。
- 事件类型 ↔ 前端 `crates/host/web-dist/index.html` 的 `render()`
  （前端对未知类型直接忽略，属前向兼容；但新类型要显示就必须加 case）。
