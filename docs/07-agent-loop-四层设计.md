# 07 agent-loop 四层设计（编排层变更点）

> 锚定父本质：「在内核 InProcess 约束下，本模块的本质是 **ReAct 状态机 + 上下文组装器**——只做编排，不写业务能力。」Phase 1 变更集中在三处：提示词组装、保留名路由、steps 观测；状态机骨架不动。

## 1. 四层结构（变更点标注 ✏️）

| 层 | 设计 |
|---|---|
| 数据规范 | ChatReq / LlmChatResp / ToolSpec（contract.rs 不变）✏️ 新增 `StepRecord{round,tool,ms}` |
| 数据存储 | 局部变量 + memory 插件（不变，A1：本体 `&self` 无跨调用可变态） |
| 数据流转 | 状态机循环不变 ✏️ 三处：① chat 开始拉 skills.list（软）；② 系统提示词改组装链；③ act() 前置保留名判断 |
| 数据接口 | `agent.chat` ✏️ 响应新增 `steps` |

## 2. 变更点详设

### 2.1 提示词组装链（每 chat 一次）
```
resolve_system_prompt()  // env > SYSTEM.md > PROMPT 具名模板 > 内置缺省
  + "\n\n## Available skills\n- name: description ..."  // skills.list 成功时
```
- SYSTEM.md / env 读取失败 → 落回下一级，warn 日志（时间流不断）；
- 技能附录仅在列表非空时附加（空附录不花 token）。

### 2.2 保留名路由（act 内，优先于 tools 分发）
```rust
if tc.name == "load_skill" {
    // 路由 assets: {"op":"skills.load","name":tc.arguments["name"]}
    // assets 不可用 → 合成 ok:false 工具结果回喂（不中断循环）
} else { /* 原 tools.exec 分发 */ }
```
- `load_skill` 不下发到 tools.list（模型可见的来源是提示词附录，非工具清单）；
- 路由步走 assets deadline（5s，同 memory 档）。

### 2.3 steps 观测与逐轮回显
- 每个 act 完成（含保留名路由）记 `{round, tool, ms}`；随响应返回；只回传不持久化（事件日志 Phase 3 的对位预留）；
- **实时回显（Phase 1）**：agent-loop 在每轮 act 前后各发一条结构化 `tracing` 事件（`round_start{round,tool}` / `round_end{round,tool,ms}`）——因 agent-loop 为 InProcess（与 host 同进程），host REPL 配置紧凑格式订阅者即可在同步调用期间实时打印进度行（`第2轮 → bash ... ✓ 1.2s`），无需改 `agent.chat` 契约。Phase 3 事件日志落地后，此路径升级为跨进程事件流。

### 2.4 历史水位
- perceive 后按 `HISTORY_LIMIT` 保留最近 N 条（system 消息不占额）；缺省不限。

## 3. manifest 变更

| 项 | 现状 | Phase 1 |
|---|---|---|
| 硬依赖 | memory.session / llm.chat / tools.exec（hard） | 不变 |
| assets | 不存在 | **不声明依赖**（软依赖：dispatch 失败容忍，host 也不对 assets 做硬探测阻断——注册后探测仅 warn） |

## 4. 生命周期钩子

| 钩子 | 行为 |
|---|---|
| `init` | 取 HostApi 句柄（不变） |
| `start`/`stop` | 无（状态机按请求驱动） |
| `destroy` | 空 |

## 5. 验证点

- （I）mock 脚本 e2e 全绿：无 skills / 无 SYSTEM.md 环境下行为与现状逐字一致；
- （Q）`load_skill` 全链路：catalog 注入 → 激活 → 模型用 read_file 执行 skill 指引；
- （Q）`steps` 形状正确（round 递增、ms 为正、含保留名调用）；
- （I）`max_rounds` 末轮强制收敛行为不变。
