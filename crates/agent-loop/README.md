# agent-loop（Rust，InProcess 编排插件）

react-agent 的大脑：把"用户一句话"变成"多轮感知 → 规划 → 行动 → 观察 → 最终答案"的 ReAct 循环。
编译进宿主，是唯一的 InProcess 插件；所有跨插件调用都经 `HostApi::call_plugin`。

## 文件结构

| 文件 | 职责 |
|---|---|
| `lib.rs` | 编排主循环、ReAct 状态机、事件发射、上下文压缩、取消 / 重试 / 预算、技能自扩展、流式 `sid` / 旁路编排 |
| `contract.rs` | 跨边界数据形状：`MemoryMsg` / `ChatReq` / `LlmChatResp` / `ToolCall` / `ToolSpec` / `StepRecord` |
| `secrets.rs` | W18 密钥脱敏闸：`act_exec` 返回值（历史+事件+trace 共同源头）与 `act_begin` 的 tool_call args 统一脱敏——密钥清单 OnceLock 惰性收集（CONFIG_FILE config.json 通用规则：键名含 key/token/secret/password 的字符串值 + `OPENAI/ANTHROPIC_API_KEY`、`MEDIA_IMAGE/VIDEO_KEY` env 兜底，<6 字符不收集）；命中处替换 `«KEY_MASKED»`。威胁模型：文件/bash 工具可读 config.json 或 `printenv` 把明文 key 带进对话；LLM 从未见过明文就吐不出明文。诚实边界：历史 trace 不追溯清洗 |

## 契约与依赖

- **硬依赖**：`memory.session` / `llm.chat` / `tools.exec`（host 注册顺序 memory → llm-adapter → tools → agent-loop）。
- **软依赖**：`assets.registry`——不可用时降级：无技能附录、具名提示词模板不可用、`load_skill` 回字段级错误。
- 时间契约：主执行流是状态机循环，收敛于最终答案或 `max_rounds` 强制停止；循环状态只存局部变量与会话记忆，
  本体 `&self` 无跨调用可变态（`A1`）。
- 空间契约：跨插件通信只走 `call_plugin`，**不**碰别插件的状态。

## 核心循环

每轮一次 `llm.chat`（带当前 memory 全量历史 + 可用工具）：

- 返回 `tool_calls` → **并行**执行（`load_skill` 路由 assets，其余路由 tools，保留名 `task` 委派子代理）：
  全部 `tool_call` 事件按声明顺序先发，再按波次（`manifest.max_inflight`，缺省 4）并发执行，
  结果按声明顺序截断后写回 memory（`tool_call_id` 对应与 steps 顺序不变），进入下一轮；
- 返回纯文本 → 收敛，发 `assistant` 事件给用户，结束。**空答案视为失败**：`ok:true + answer:""`
  是假收敛，按错误 payload 收敛、不落 memory（PLAN R4）。

- `max_rounds` 来自构造参数（`MAX_ROUNDS` env 可每轮覆盖，默认见 `main.rs`），超过则强制收敛并返回错误
  payload（不抛异常）。
- **保留名路由**（03 §3）：`load_skill`（→ assets `skills.load`）、`task`（→ 子会话 `agent.chat`）。
- **委派深度随链传播**：`ChatReq.depth`（`0`=顶层会话，`1`=子代理，缺省 `0`）。子代理链内（depth ≥ 1）
  拒绝再嵌套 `task`；深度不是插件级共享计数——并发会话互不挤占委派额度（PLAN R1）。
- **瞬态重试（PLAN T3）**：`llm.chat` 失败若属瞬态（限流 429 / 超时 / 5xx / 连接类；内核 deadline 超时同判），
  按指数退避重试（`LLM_RETRY_ATTEMPTS` 次，基数 `LLM_RETRY_BASE_MS`，单次封顶 8s）；K400 等确定性失败立即返回。
  重试经 `retry` 事件落审计日志。
- **超限降级重试（PLAN P8）**：`CONTEXT_OVERFLOW`（上下文超限，确定性但可行动；provider 侧经 llm-adapter
  归一化，见 `plugins/llm_adapter/README.md`）特判——未降级过则窗口/限额减半重试**一次**
  （trace 落 `retry`，`reason=CONTEXT_OVERFLOW, degraded=true`）；再超 → 原错误收敛，不进重试风暴。
- **轮次边界停车检查**：每轮开头 / 工具波次后 / 强制收敛轮前依次检查「用户取消（`cancel` op 置位，K499）→
  时长预算 → token 预算（K508）」，命中即收敛返回。
- **总预算（PLAN T4）**：单次 chat 有墙钟（`CHAT_BUDGET_SECS`，缺省 900s，`0`=禁用）与 token
  （`CHAT_TOKEN_BUDGET`，input+output 累计，`0`=禁用）上限。子代理继承**衰减后**的剩余
  （`ChatReq.budget_ms_left` / `tokens_left` 随链携带）；轮内超支由单步 deadline（5s/60s/600s）封顶。
  LLM 步 600s 覆盖深度思考模型长推理（缺省预算 900s 须 ≥ 单步闸），须与 llm_adapter `PROVIDER_DEADLINE=590s` 配对（provider 先收敛 10s 报可重试 timeout）。

## 事件（发到 `memory.session.trace`，`type` 字段）

浏览器 SSE 就是轮询这些 trace 事件。**新增 / 改事件类型要同步前端 `render()`**：

- 全集（13 类，2026-09-10 修正：早期文档列出的 `chat_start`/`thinking`/`plan` 不存在——思考流由旁路 `stream_*` 承载）：
  `user` / `assistant` / `tool_call` / `tool_result` / `artifact` / `sources` / `file_change` / `compaction` / `subagent` / `skill_loaded` / `skill_installed` / `error` / `retry`
- `retry`（PLAN T3）：LLM 瞬态失败重试观测，带 `attempt` / `delay_ms` / `reason`；前端未知类型按前向兼容忽略。
- `tool_call` / `tool_result` 事件带 `id`（tool_call_id，审计对位）；`tool_result` 另带 `ms`（该工具
  自身耗时）、`ok`、`result_truncated`（2000 字符）、`memory_truncated`（回喂进 memory 的内容是否被截断）。
- 最终 `assistant` 事件承载完整答案，并附 `reasoning` / `usage` / `elapsed_ms`（多轮累计；
  字段缺省即 provider 未提供，旧消费者无感知）。
- `error` 事件含人类可读原因，前端据此以红条收尾。
- `artifact`（产物登记）：write_file/edit_file 成功 → `{"type":"artifact","path","tool","bytes"?}`；
  bash/最终答案文本经扩展名启发式扫描兜底（双闸：二进制成品任意位置放行；md/html/txt 等文本类
  仅产物目录 `AGENT_OUTPUT_DIR` 内放行）。前端渲染可点击文件卡片。
- `sources`（来源登记）：web_search 成功 → `{"type":"sources","tool","items":[{"title","url"}]}`
  （web_read 单 URL 同款）；URL http/https 白名单 + 去重 + 上限 10 条。
- `file_change`（执行可见性 V2 + W16，见下节）。
- `skill_loaded`：`load_skill` 成功（会话技能集重放推导依据）；`skill_installed`（R9）：技能安装编排
  完成，带 `skill` / `tools_loaded` / `tools_pending`——前端过程框内联卡「一键启用」数据源。

## 执行可见性：file_change 事件与 undo 快照（V2 + W15 + W16）

`tools_exec.rs::detect_file_changes` 在每个工具成功后从结果推导 `file_change` 事件落 trace
（只追加、SSE 透传、重放可见），是前端「📝 文件变更」chip / Monaco 双栏 diff / 回滚撤销的统一数据源：

- **write/edit**（V2）→ 单事件 `{"type":"file_change","path","op":"write"|"edit","round","undo"?}`。
- **bash**（W16）→ 结果带 `changes[]`（bash.py 快照区执行前后比对）→ **逐条展开**为
  `op:"bash"` 多事件（新增/修改/删除，抽屉徽章「脚本」），与 write/edit 同一 diff/回滚协议。
- **undo 引用**：结果内带 `undo:{id,created,deleted,bytes_*}`（files.py / bash.py 变更快照，
  原始字节落 `MEMORY_DATA_DIR/undo/<id>.{before,after}`）→ 原样透传。前端 diff 数据源三级降级：
  快照 → tool_call args 重构 → 单侧当前内容；回滚撤销经 host `/api/chat/rollback` 倒序恢复。
- 子代理变更经 trace 镜像汇入同池；纯观测不进模型上下文（事件不回喂 LLM）。

## 上下文体积管理（截断 + 压缩 + 窗口 + token 闸，四层）

| 层 | 闸门 | 默认 | 说明 |
|---|---|---|---|
| 单条 | `TOOL_RESULT_LIMIT` | 8000 字符（`0`=禁用） | 工具结果回喂前按字符截断，追加 `…[truncated]` 标记（模型可感知被裁剪）；**发生在入 memory 之前**——memory 即上下文来源，全文不入库 |
| 跨会话 | `COMPACT_TRIGGER` / `COMPACT_KEEP` | 40 / 10 条（`0`=禁用） | chat 开局按**全量历史**判断：超 TRIGGER 则旧史交 LLM 摘要，经 `memory.summarize` 落盘（防撕裂在 memory 侧）；任何失败 → 降级不压缩 |
| token 闸 | `LLM_CONTEXT_TOKENS` | `0`=禁用 | 模型上下文窗口（token）。**仅本地窗口型 provider 生效**（`LOCAL_WINDOW_PROVIDERS` 名单：ollama——显存受限、常需调小窗口换速度；新本地后端接入后加名；云端 API 窗口由服务端管理，本闸禁用、`num_ctx` 不下发，避免为本地调小的窗口误压云端历史）。发送预算 = 窗口 × 0.7（预留输出与工具 schema）；**压缩双闸之二**：估算工作集（CJK ≈ 1 token/字、其余 ≈ 4 字符/token，保守高估）超预算即触发压缩——单条大结果在条数闸之前就能撑爆窗口（PLAN R6）。窗口值还随 `llm.chat` payload 透传 `num_ctx`（ollama native 映射 `options.num_ctx`，本地估算闸与服务端窗口对齐；0/缺省不下发）。注意：provider 取 host 启动时 env，Web 热切换 provider 后判定滞后、重启校正 |
| 单次发送 | `HISTORY_LIMIT` | 无限制 | 只裁剪发给 LLM 的工作集（保留最近 N 条），**发生在压缩判断之后**——若截断在前，LIMIT < TRIGGER 时压缩永不触发（PLAN R3） |

- 压缩那次 LLM 调用**不带流式旁路**（不是给用户看的产出）。
- 顺序固定：全量拉取 → 压缩判断（双闸：条数 **或** token，可落盘）→ HISTORY_LIMIT 窗口 →
  **发送前逐级收紧**（PLAN P7：token 闸启用且估算仍超预算 → 窗口条数减半 → tool_result 限额减半 →
  仍超则返回 `CONTEXT_OVERFLOW` 错误 payload，**正文请求不发出**；ctx 元数据含 limit/budget/estimated）→
  进本轮循环。收紧只影响本轮工作集，memory 全量历史不受影响。

## 技能自扩展

- **L1（模型写入技能）**：当 `skills.list` 返回的 `root` 落在 `WORKSPACE_ROOT` 内，系统提示词追加
  "可读写 `<root>` 的 SKILL.md 自创技能"授权段。
- **L2（动态工具）**：模型 / Web 经 `tools.reload` + `tools.configure` 热加载新工具（装载 ≠ 启用）。

## 流式编排（旁路文件）

内核 guest 是 unary gRPC，插件处理中推不了事件。本插件负责"每轮生成 `sid` + 旁路路径"：

- `stream_file_for(session, round)`：生成 `.stream/<session>.jsonl` 绝对路径（session 名过安全校验）。
  **规则必须与 host 侧 `config.rs::stream_file` 完全一致**，否则前端读不到。
- 每轮 `llm.chat` 的 `payload` 带 `stream_path` + `sid`；llm-adapter 在生成过程中往旁路文件写
  `start`/`delta`/`end`/`error` 帧，host 网关 tail 后映射为 `stream_start`/`stream_delta`/
  `stream_end`/`stream_error` 推前端（`error` 与 trace 的 `error` 撞名故加前缀），最终 `assistant`
  事件带完整内容。
- 多轮 `usage` 在 `UsageAcc` 里累计（用户看总消耗）；`elapsed_ms` 取本轮 LLM 耗时。

> **sid 唯一性（已修复，2026-09-08）**：`sid` 形如 `{session}-r{N}`，N 为 per session
> 单调递增序号（首个以 unix 毫秒为种子，`next_sid`）——跨对话回合不再碰撞。旧实现
> N = 回合内 rounds（每对话回合重置），连续两轮 rounds 相同会生成相同 sid，前端
> doneSids 去重可能误吞后一回合的流式动画（直接显示最终答案，无打字效果）。
> 形状保持 `{session}-r{digits}`：llm-adapter `_SID_RE`（`^(.*)-r\d+$` 反解会话 id 供
> abort/cancel 对位）与 host `stream_file` 均无需改动。

## 改动时的联动点

- 数据形状 ↔ `contract.rs`（被 host / frontend / memory 镜像消费）。
- 事件 `type` ↔ 前端 `crates/host/web-dist/index.html::render()`。
- 旁路路径规则 ↔ `crates/host/src/config.rs::stream_file`。
- 新增硬依赖 capability ↔ host 注册顺序（`crates/host/src/main.rs`）。

## 运行中断（停止）

ReAct 主循环在**轮次边界**轮询取消标志（PLAN P2/T1），运行中的对话可被用户中止：

- **agent-loop**：`cancels: Mutex<HashSet<String>>` 存被请求取消的 session id。`cancel` op 置位
  （缺 `session_id` → K400）；循环在轮次边界（每轮开头 / 工具波次后 / 强制收敛轮前）**take 命中即清**，
  发 `error`（where=cancel）事件并以 K499 收敛。语义为 **Concurrent**——Serial 会让 cancel dispatch
  在 per-plugin 锁后排队到 chat 结束之后（取消永远迟到）；`max_inflight=8` 预留 cancel 通道余量。
- **残留防护**：chat 开局 + 结束双保险清理同 session 残留标志——取消晚到（chat 已收敛）不误杀下轮对话。
- **host（R1 双通道）**：`POST /api/chat/cancel?session=` 转发 `cancel` op 的**同时**并行 dispatch
  llm-adapter `abort` op（该插件 Concurrent 语义可即时受理）——流式生成逐帧检查命中即关流，
  **单轮长生成无需等轮次边界**；abort dispatch 失败仅 warn 降级（轮次边界取消仍有效）。
- **前端**：发送期间显示「停止」按钮（与发送互斥），点击发取消请求并以状态条反馈。

**粒度边界**：轮次边界取消（K499）+ 流式逐帧中断（R1）双通道后，进行中的 LLM 流式可即时中断；
工具执行段自 T7 起可中断——cancel op 联动向 tools 插件发 `abort` op（即发即忘，2s 上限），
按会话终止运行中的工具子进程（覆盖技能工具 `_exec_command` 路径；bash/web 内建工具自带
≤60s timeout，泄漏线程由 serve(max_workers=16) 纵深兜底）。tools 插件已开 Concurrent
（T6，并发安全已审查），abort 与在途 call 并行受理不再排队。
同族停车检查还有总预算（时长/token，K508，见「核心循环」）。
