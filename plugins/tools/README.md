# tools（Python guest）

模型"动手"的那一层：文件读写编辑、列目录、bash、联网搜索与读取。
本插件**只做分派与授权**，工具实现在 `tools/` 包里。

## 文件结构

| 文件 | 职责 |
|---|---|
| `tools_plugin.py` | 瘦分派层：init 装配 scope → list 过滤 → call 校验分发 → `ToolError` 转字段级错误 |
| `tools/__init__.py` | `ToolError`（唯一受控错误通道）、`workspace_root()`、`require()`、`optional_int()` |
| `tools/files.py` | `read_file` / `write_file` / `edit_file` / `list_dir` |
| `tools/bash.py` | `bash` |
| `tools/web.py` | `web_search` / `web_read` |

## op 契约（线契约 03 §2.3）

```jsonc
{"op":"list"}                          → {"ok":true,"tools":[{name,description,parameters}]}
{"op":"list","all":true}               → 含未启用工具，每项附 "enabled":bool（配置中心视图）
{"op":"call","name","args":object}     → {"ok":true,"result":any}
{"op":"configure","enabled":[str,...]} → {"ok":true,"enabled":[...]}   // 运行时整体替换白名单
{"op":"reload"}                        → {"ok":true,"loaded":[...],"added":[...],"skipped":[...]}
```

错误码：`K400`（未知 op / 参数不合规）、`UNKNOWN_TOOL`、`TOOL_DISABLED`、`BAD_ARGS`、
`MISSING_ARG`、`BAD_ARG`、`TOOL_ERROR`。带 `field` 时宿主面板可定位到具体参数。

## 工具清单（7 个）

| 工具 | 文件 | 要点 |
|---|---|---|
| `read_file` | files | 行号化输出，限工作区内 |
| `write_file` | files | 整体覆盖写，父目录自动创建 |
| `edit_file` | files | 精确字符串替换（`old_string` 必须完全匹配，含空白） |
| `list_dir` | files | 支持 glob |
| `bash` | bash | 受沙箱策略约束（见下） |
| `web_search` | web | 返回 `[{title,url,snippet}]`，后端可配 |
| `web_read` | web | 网页转 markdown 文本 |

全集名同时硬编码在 `crates/host/src/config.rs::ALL_TOOL_NAMES`——**增删工具必须同步改那里**。

## 授权：白名单与「装载 ≠ 启用」

- `TOOLS_ENABLED` 是白名单（逗号分隔）。**未列出的工具 Schema 与实现双不可见**：
  `list` 不返回，`call` 直接 `TOOL_DISABLED`。
- 该变量含未知工具名时，插件 `init` 直接 `SystemExit`（fail-fast，不静默忽略）。
- `reload` 动态装载 `tools/*.py` 里的新工具：新工具**进可用池但不进白名单**，
  必须显式 `configure` 才会出现在 `list`。写文件与启用是两步，别指望放进去就生效。
- 动态装载是 **fail-closed**：单模块 import 失败 → 跳过并回 `skipped`，该模块旧工具原样保留；
  **内置工具名不可被覆盖**（重名检查 + `_BUILTIN_MODULES`）。
- 保留名 `load_skill` 不在本注册表，由 `agent-loop` 路由给 assets。

## 新增工具

**内置（推荐，随版本发布）**

1. 在 `tools/files.py` / `bash.py` / `web.py` 里写实现函数 `_(args)`。
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
| `BASH_SANDBOX` | `on`（缺省，需 sandbox-run 助手；助手缺失则 fail-closed 直接把 bash 移出白名单）/ `off`（显式豁免） |
| `SEARCH_BACKEND` / `SEARCH_REGION` | 搜索后端与区域 |
| `BOCHA_API_KEY` / `BAIDU_API_KEY` / `TAVILY_API_KEY` | 搜索服务鉴权 |

## 改动时的联动点

- 工具增删 ↔ `crates/host/src/config.rs::ALL_TOOL_NAMES` ↔ 前端设置面板的工具列表
  （经 `/api/config` 的 `tools.enabled` 读写）。
- `bash` 的沙箱助手 `sandbox-run` 需与宿主二进制同目录（源码 `crates/host/src/bin/sandbox-run.rs`）。
- 工具失败要**回喂给模型**而不是中断循环：因此 `call` 里捕获所有异常转成 `ok:false`，
  让模型看到错误后自行改参数重试。
