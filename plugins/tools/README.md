# tools（Python guest）

模型"动手"的那一层：文件读写编辑、列目录、bash、联网搜索与读取、正则内容搜索、语法级符号检索。
本插件**只做分派与授权**，工具实现在 `tools/` 包里。

## 文件结构

| 文件 | 职责 |
|---|---|
| `tools_plugin.py` | 瘦分派层：init 装配 scope → list 过滤 → call 校验分发 → `ToolError` 转字段级错误 |
| `tools/__init__.py` | `ToolError`（唯一受控错误通道）、`workspace_root()`、`require()`、`optional_int()` |
| `tools/files.py` | `read_file` / `write_file` / `edit_file` / `list_dir`（`_guard` 越界拦截、`_guard_write` v6 核心写保护、`_snapshot_change` W15 变更快照、undo 落盘） |
| `tools/bash.py` | `bash`（沙箱策略 + W16 快照区执行前后比对 → `changes[]` 追溯） |
| `tools/web.py` | `web_search` / `web_read` |
| `tools/grep.py` | `grep`（正则内容搜索，复用 files 的 `_guard`/`_display`/`_decode`） |
| `tools/symbols.py` | `symbols_search`（tree-sitter 语法级符号检索；缺依赖时本模块不装载） |

## op 契约（线契约 03 §2.3 + R9 扩展）

```jsonc
{"op":"list"}                          → {"ok":true,"tools":[{name,description,parameters}]}
{"op":"list","all":true}               → 含未启用工具，每项附 "enabled":bool（配置中心视图）
{"op":"call","name","args":object}     → {"ok":true,"result":any}
{"op":"configure","enabled":[str,...]} → {"ok":true,"enabled":[...]}   // 运行时整体替换白名单
{"op":"reload"}                        → {"ok":true,"loaded":[...],"added":[...],"skipped":[...]}
{"op":"install","path","skill"?}       → {"ok":true,"skill","loaded","skipped","pending"}   // R9 技能工具定点装载
{"op":"skill_tools","skills"?,"all"?}  → {"ok":true,"tools":[...]}                          // R9 装配/配置视图
```

错误码：`K400`（未知 op / 参数不合规）、`UNKNOWN_TOOL`、`TOOL_DISABLED`、`BAD_ARGS`、
`MISSING_ARG`、`BAD_ARG`、`TOOL_ERROR`、`TOOL_TIMEOUT`（技能工具超时）、`TOOL_EXEC_ERROR`
（技能工具执行体失败）。带 `field` 时宿主面板可定位到具体参数。

## 工具清单（9 个）

| 工具 | 文件 | 要点 |
|---|---|---|
| `read_file` | files | 行号化输出，限工作区内 |
| `write_file` | files | 整体覆盖写，父目录自动创建 |
| `edit_file` | files | 精确字符串替换（`old_string` 必须完全匹配，含空白） |
| `list_dir` | files | 支持 glob |
| `bash` | bash | 受沙箱策略约束（见下）；W16 起执行前后自动快照比对，间接改文件同样可追溯（见「bash 文件追溯」） |
| `web_search` | web | 返回 `[{title,url,snippet}]`，后端可配 |
| `web_read` | web | 网页转 markdown 文本 |
| `grep` | grep | 正则内容搜索 `path:line: text`，剪枝噪音/二进制/大文件 |
| `symbols_search` | symbols | 语法级定义检索（function/method/class/…），query=名字子串大小写不敏感 + kind/language 过滤；依赖 `tree-sitter-language-pack<1.0`（1.x 改按需下载，禁用），缺依赖自动降级为 8 件 |

全集名同时硬编码在 `crates/host/src/config.rs::ALL_TOOL_NAMES`——**增删工具必须同步改那里**。

## 授权：白名单与「装载 ≠ 启用」

- `TOOLS_ENABLED` 是白名单（逗号分隔）。**未列出的工具 Schema 与实现双不可见**：
  `list` 不返回，`call` 直接 `TOOL_DISABLED`。
- 该变量含未知工具名时不再启动失败（R9 延迟启用）：挂入 `_DEFERRED_ENABLED`，
  `install` 同名技能工具时自动转入启用集（config.json 持久化授权先于装载到达的场景）。
- `reload` 动态装载 `tools/*.py` 里的新工具：新工具**进可用池但不进白名单**，
  必须显式 `configure` 才会出现在 `list`。写文件与启用是两步，别指望放进去就生效。
- 动态装载是 **fail-closed**：单模块 import 失败 → 跳过并回 `skipped`，该模块旧工具原样保留；
  **内置工具名不可被覆盖**（重名检查 + `_BUILTIN_MODULES`）。
- 保留名 `load_skill` 不在本注册表，由 `agent-loop` 路由给 assets。

## 核心写保护（v6 自扩展安全边界）

`_guard_write`（write_file / edit_file 专用，在越界拦截之后）：核心黑名单**默认拒绝**，报
`CORE_PROTECTED`——agent 自身运行体不可被模型改动：`crates/`、`plugins/memory`、
`plugins/llm_adapter`、`plugins/assets`（skills 根内豁免）、`plugins/tools/tools/`、
`plugins/tools/tools_plugin.py`、`config.json`、`.git/`。

- **自扩展合法途径**（白名单）：skills 目录 SKILL.md / 技能 tools.json / `plugins/tools/` 顶层
  新 .py / config.json `mcp_servers`。
- **逃生舱**：env `ALLOW_CORE_WRITE=1`（config.json `tools.allow_core_write: true` 持久通道，改后重启
  host）。确需改核心时模型应向用户说明，由用户自行修改或显式开启。
- **诚实边界**：bash 间接写不经此闸——但 W16 起自动快照留痕（含核心区路径），随回滚一并撤销
  （SYSTEM.md 纪律 + `BASH_SANDBOX` 沙箱兜底 + 留痕三防线）。

## bash 文件追溯（W16）

bash 对文件的间接修改（curl 下载、mv 重命名、脚本生成等）无法像 write/edit 那样预知路径，
采用**执行前后快照区比对**实现同一追溯链路：

- 快照区 = 工作区 − 噪音目录/`.stream`/产物目录/`MEMORY_DATA_DIR`/`.git`；执行前 pre-copy
  （上限 32MB / 4000 文件，超限退化为 stat 比对、变更无 undo 引用），执行后逐文件比对内容。
- 变更（新增/修改/删除）→ 结果附 `changes[]`（`{path, kind, undo?}`，undo 引用
  `_snapshot_change` 落 `MEMORY_DATA_DIR/undo/<id>.{before,after}` 原始字节，删除型 before 有值
  after 为 None）→ agent-loop 逐条转发为 op=bash 的 `file_change` 事件 → 与 write/edit 同一
  diff 视图 / 回滚撤销链路（见 agent-loop README「执行可见性」）。
- **关闭**：`BASH_WRITE_TRACE=off`。**诚实边界**：工作区外改动不追踪；同 mtime+size 的
  内容替换不检测。

## 新增工具

**内置（推荐，随版本发布）**

1. 在 `tools/files.py` / `bash.py` / `web.py` / `grep.py` / `symbols.py` 里写实现函数 `_(args)`。
2. 在同文件的 `TOOLS` 字典登记：`"name": {"description","parameters","run"}`，
   `parameters` 是 JSON Schema。
3. 同步 `crates/host/src/config.rs::ALL_TOOL_NAMES`。
4. 参数校验用 `require()` / `optional_int()`，失败抛 `ToolError(message, code, field)`——
   **message 要写明"下一步怎么改"**，因为它是回喂给模型的。

**动态（L2 自扩展，运行时热加载）**

1. 在 `tools/` 下放 `<mod>.py`，暴露 `TOOLS: dict[name, {"name","description","parameters","run"}]`
   （四键齐全，`run` 可调用）。
2. 调 `{"op":"reload"}`，从返回里确认 `added`。
3. 再调 `{"op":"configure","enabled":[...,新工具]}` 才真正启用。

## 环境变量

| 变量 | 作用 |
|---|---|
| `WORKSPACE_ROOT` | 越界拦截根（缺省取进程 cwd，宿主会显式下发） |
| `TOOLS_ENABLED` | 白名单，逗号分隔 |
| `ALLOW_CORE_WRITE` | `1` 放行核心写保护（缺省 `0` 拒绝报 `CORE_PROTECTED`；持久通道 config.json `tools.allow_core_write`） |
| `BASH_SANDBOX` | `on`（缺省，需 sandbox-run 助手；助手缺失则 fail-closed 直接把 bash 移出白名单）/ `off`（显式豁免） |
| `BASH_WRITE_TRACE` | on（缺省）= bash 执行前后快照区比对追溯；off=关闭快照 |
| `SEARCH_BACKEND` / `SEARCH_REGION` | 搜索后端与区域 |
| `BOCHA_API_KEY` / `BAIDU_API_KEY` / `TAVILY_API_KEY` | 搜索服务鉴权 |

## 改动时的联动点

- 工具增删 ↔ `crates/host/src/config.rs::ALL_TOOL_NAMES` ↔ 前端设置面板的工具列表
  （经 `/api/config` 的 `tools.enabled` 读写）。
- `bash` 的沙箱助手 `sandbox-run` 需与宿主二进制同目录（源码 `crates/host/src/bin/sandbox-run.rs`）。
- 工具失败要**回喂给模型**而不是中断循环：因此 `call` 里捕获所有异常转成 `ok:false`，
  让模型看到错误后自行改参数重试。
