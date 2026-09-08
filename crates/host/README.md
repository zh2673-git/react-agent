# react-agent-host（Rust 宿主）

react-agent 的运行时：拉起全部 guest 插件、暴露 Web UI 与 SSE、把内核事件转成浏览器能消费的事件流，
并把"边生成边显示"的 LLM 旁路文件 tail 出来推给前端。

## 文件结构

| 文件 | 职责 |
|---|---|
| `main.rs` | 二进制薄入口：`assemble`（spawn + 探测 + 注册）、CLI 参数、会话 id、启动 Web |
| `lib.rs` | 模块重导出（config / frontend / web / manifests / spawn），供 e2e 测试复用 |
| `config.rs` | `HostConfig`：env 解析、持久配置（`config.json`）、`llm_env()` / `passthrough_env()` / `stream_file()` / `skills_dir()` / `workspace_root()` / `ALL_TOOL_NAMES` / `resolve_bash_sandbox`（fail-closed） |
| `frontend.rs` | Web 服务入口：HTTP 监听、静态资源（`web-dist/`）路由到 `web/` |
| `web/mod.rs` | 路由分发（`match (method, route)`）+ SSE：`gateway::sse_events` 每 50~300ms 轮询合并 trace + 流式旁路（含子旁路 `#sub-*.jsonl`） |
| `web/api.rs` | 配置 / 会话 / 模型 / 技能 CRUD 等处理器（`/api/config`、`/api/skills/*`、`/api/chat*`、`/api/models`、`/api/presets`、`/api/reveal`…） |
| `web/files.rs` | 文件通道：`/files/{path}`（mime 服务 + 下载）、`/api/tree`（W14 工作区文件树，深度/条目双上限 + 剪枝）、`/api/fc-snapshot`（W15 变更快照读取，id 严格格式校验防穿越） |
| `web/gateway.rs` | SSE 核心：trace 重放 / 实时分阶段（replaying 门闩）、流式旁路 tail（主 + 子）、心跳 5s |
| `manifests.rs` | `guest_manifest`：Process 域 manifest 构造（api_version 必须 0.1） |
| `spawn.rs` | guest 子进程 spawn（python / node），注入 `PYTHONPATH` 与 provider env |
| `bin/sandbox-run.rs` | bash 沙箱助手（与宿主二进制同目录；`tools` 插件的 `BASH_SANDBOX=on` 依赖它） |
| `web-dist/` | 前端三件套 `index.html + style.css + app.js`（原生 JS 无构建）+ `vendor/`（Monaco 本地资源，运行时 serve 非内嵌）；`render()` 等全局函数在 `app.js` |

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
- `/api/*`：配置读写、会话、技能 CRUD、chat（含 R3 attachments 透传校验）/ cancel（R1 双通道）/ rollback（R2 回滚转发 + W15 文件撤销：截断前收集区间内带 undo 引用的 file_change 事件，倒序恢复快照，冲突跳过报告）/ fc-snapshot（W15 变更快照读取）等（见 `web/mod.rs`；API 表见根 README）。
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

## /api/* 一览（web 网关契约，host 级非插件）

跨插件 payload 契约见各插件 README（tools / llm_adapter / memory / assets / agent-loop）；本节是宿主 HTTP 层契约。

### 会话与事件

- `GET /` → 单页（Cursor 暖色系事件流式会话：米色纸感底 + 半透明炭黑 CTA，主题 token 见 PLAN.md W1；左侧会话栏持久化、工具调用状态点卡片、富 markdown 代码块复制 + 头部 📁 文件树 / ⚙ 设置面板：LLM / 工具 / 技能 / Agent；文件变更 chip 在每轮答案下方）
- `GET /api/events?session=&after=` → SSE（从 0 全量重放 + 实时增量）
- `POST /api/chat` body `{"session_id":str,"message":str,"attachments"?:[{"name","mime","data_b64"}]}` → 阻塞至收敛，回 agent.chat 响应（attachments 可选：图片走多模态映射、文本文件内嵌 content；上限 4 个、单个 ≤2MB，host 校验形状与体量，非法即 K400）
- `POST /api/chat/cancel?session=` → 取消运行中的 chat：agent-loop `cancel`（工具波次间 + 轮次边界收敛 K499）+ llm-adapter `abort`（流式逐帧检查命中即关流，单轮长生成无需等轮次边界），立即返回
- `POST /api/chat/rollback` body `{"session_id":str,"upto_user_index":int}` → R2 回滚：memory 消息与 trace 事件**同源物理截断**到第 N 条 user 消息之前（0 基）。**计数口径以 trace user 事件为准（UI 真相源）**；memory 经压缩只剩「标记 + 最近 K 条」，两侧按**尾部对齐**（压缩只裁头部）定消息切点：回滚点在保留区 → 保压缩标记、截到该轮前；落在摘要区 → 标记与消息全清（摘要与回滚区间重叠，保留即上下文残留）；无 trace 文件的纯 memory 会话按 memory 侧计数，标记随截断一并丢弃。越界整体失败不落盘。**W15 文件撤销**：截断前读 trace 收集区间内带 undo 引用的 `file_change` 事件，截断成功后**倒序恢复**——created（新建）删除文件、deleted（W16 bash 删除）在文件仍不存在时还原 before 快照字节（被重建则跳过）、覆盖/编辑还原 before 快照字节；冲突检测：当前文件与 after 快照逐字节不一致（agent 写完后又被人改过）→ 跳过并报告，绝不硬覆盖；bash 间接改文件经 W16 快照同样记事件（op=bash）并随回滚撤销（`BASH_WRITE_TRACE=off` 关闭）。body 可选 `undo_files:false` 显式关闭；响应新增 `undone[]`/`skipped[]`；技能目录与产物登记不受影响；前端 user 气泡 hover「⤺ 回滚」（确认框预列将被撤销的文件）、答案 hover「↻ 重新生成」（= 回滚该问题 + 自动重发原文与附件）
- `POST /api/reveal` → 在系统文件管理器中定位工作区文件（前端产物卡/文件树「打开所在文件夹」）

### 文件通道

- `GET /files/{path}[?download=1]` → 按扩展名 mime 服务工作区内任意文件（realpath ⊆ WORKSPACE_ROOT，64MB 上限）：产物卡与文件树预览/下载共用通道
- `GET /api/tree?limit=` → 工作区文件清单（W14 文件树数据源）：递归扫 WORKSPACE_ROOT（剪枝噪音/点目录，不跟 symlink），深度 6 / 条目 3000（≤20000）双上限 + `truncated` 标记；扁平 `[{path,size}]` 正斜杠输出，与 `/files` 同一 WORKSPACE_ROOT 口径
- `GET /files/` 路径解析失败且包含「工作区目录名/」时截前缀重试（恢复历史错误路径的可下载性）
- `GET /api/fc-snapshot?id=&side=before|after` → W15 变更快照读取（write/edit/bash 落在 `MEMORY_DATA_DIR/undo/<id>.{before,after}` 的原始字节）；id 严格格式校验（13 位毫秒时间戳 + 8 位小写 hex）防路径穿越；前端 diff 视图数据源
- `GET /vendor/{path}` → 静态资源树（Monaco 编辑器本地化于 `web-dist/vendor/`）：路径段校验（无 `..`/反斜杠/空段）+ 扩展名白名单（js/css/json/ttf/woff/woff2）防穿越；产物文件卡预览走「本地 vendor → CDN → 纯文本」三级降级链

### 配置与模型

- `GET /api/config` → 配置视图（llm：config.json > env 缺省，key 只回 key_set+尾 4 位；tools 三池聚合数组——每项 `{name,enabled,pool:"builtin"|"skill"|"mcp",description,parameters}` 附 `skill`/`mcp_server` 来源字段，前端按 pool 分组渲染：内置平铺，技能/MCP 池按来源折叠分组 + 组头总开关（三态），MCP 组 ready 在前 failed 垫底；`mcp:{servers, declared}`——servers 为运行状态，declared 为 config.json 声明视图（command/cwd + env 各键脱敏为 `key_set`/`key_tail`），key 配置内嵌于前端「MCP 外接」各服务折叠组内（填 Key → 保存仅落盘 → 重启 host 生效）；skills_count；agent 参数视图）
- `PUT /api/config` → 分段合并落盘 + 热应用：`llm` 逐字段（null 不覆盖，key 热应用 env）；`tools.enabled` 白名单整体替换（configure 热生效）；`agent` 逐字段（null 不覆盖）；`mcp_servers` 按 server 名合并——command/cwd 未传保留原值，env 逐键合并（空串=不动），**仅落盘**（server 子进程生命周期归插件 init/destroy，改后需重启 host）
- `GET /api/models` → 转发 llm-adapter `models.list`，返回当前 provider 可用模型 id（前端「拉取模型」按钮；配好 base_url/key 后自动填充 model 下拉；ollama 额外透传 `models_meta` 原生窗口元数据——前端下拉展示 `模型名 · 256k`，Agent 页 `llm_context_tokens` 提示原生窗口并可一键填入）
- `GET /api/presets` → 转发 llm-adapter `presets.list`，OpenAI 兼容站点预设清单（数据源 plugins/llm_adapter/presets.py——前端「站点」下拉一键切换：选站自动填 base_url、per-site key 由 localStorage 记忆带出，保存走 configure 热应用零重启）

### 技能 CRUD

- `GET /api/skills` → assets skills.list（实时目录；R9：合入 `tools_detail`——tools.skill_tools all=true 按 skill 分组的配套工具视图，含未启用项 + enabled 标记，前端技能 tab 就地启停数据源；tools 不可用 → 静默省略）
- `GET /api/skills/{name}` → `{"ok":true,"name","content"}`（SKILL.md 原文，编辑用）
- `PUT /api/skills/{name}` body `{"content":SKILL.md全文}` → 写入（frontmatter name 须与目录名一致；名字仅字母数字/_/-）；写入自动打标 `origin: user`
- `DELETE /api/skills/{name}` → 删除技能目录；出厂件（frontmatter 显式 `origin: preset`，预置技能归 git 管理）拒删 K403，缺省视为用户件可删

## 流式目录

- `AGENT_STREAM_DIR`（默认 `<项目根>/.stream`）：宿主 `main.rs` 创建并下发，agent-loop 据此生成旁路路径。
- `config.rs::stream_file(session)` 与 agent-loop 的 `stream_file_for` **规则必须一致**（session 名净化防穿越）。
- 该目录已加 `.gitignore`，运行时产物不进版本库。

## 改动时的联动点

- 新插件环境变量 ↔ `config.rs::llm_env()` / `passthrough_env()` ↔ `plugins/README.md` 环境变量总表。
- 旁路路径规则 ↔ `crates/agent-loop/src/lib.rs::stream_file_for`。
- 事件类型 ↔ `web-dist/app.js::render()`（对未知类型前向兼容忽略，新类型要显示须加 case）。
- `ALL_TOOL_NAMES` ↔ `plugins/tools/README.md`（工具增删要同步）。
- 设置面板字段 ↔ `plugins/llm_adapter/README.md`（provider 切换自适应）。

## 已知限制

- **运行中断（停止）**：已支持（P2/T1 + R1 补强）。`POST /api/chat/cancel?session=` 并行双通道：
  转发 agent-loop `cancel` op（轮次边界 K499 收敛）+ dispatch llm-adapter `abort` op（流式循环逐帧
  检查命中即关流，**单轮长生成可即时中断**；llm-adapter 已为 Concurrent 语义可即时受理，abort
  dispatch 失败仅 warn 降级——轮次边界取消仍有效）。前端发送期间显示「停止」按钮。详见
  agent-loop README「运行中断（停止）」与 llm_adapter README「abort」。
- **sid 已修复（历史限制）**：sid 现形如 `{session}-r{N}`（N 为 per session 单调序号，unix
  毫秒种子），相邻对话回合不再碰撞。旧版 rounds 每回合重置导致 doneSids 去重误吞流式动画的
  问题已消除，细节见 `crates/agent-loop/README.md`「流式编排」。
