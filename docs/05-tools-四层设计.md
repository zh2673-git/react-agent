# 05 tools 四层设计（生产级工具集）

> 锚定父本质：「在内核插件契约的时空约束下，本模块的本质是**模型行动空间的守门人**——决定模型能看到哪些工具（规则）、能触到哪片空间（越界拦截）、以及失败后如何回喂（时间流不断）。」

## 1. 四层结构

| 层 | 设计 |
|---|---|
| 数据规范 | ToolSpec 三元组：`name / description / parameters(json-schema)`；模型可见 Schema 与运行时实现（execute、超时、边界）分离（dsh 三层：系统有哪些 / 当前可见哪些 / 以什么形式给模型） |
| 数据存储 | 注册表常驻内存（init 装配）；无持久化 |
| 数据流转 | `list`：scope 过滤后返回 Schema → `call`：查表 → 越界校验 → execute → 异常捕获回喂（不中断 ReAct 循环） |
| 数据接口 | `tools.exec`（线契约不变；错误升级字段级：`{"code","message","field"?}`） |

## 2. 工具清单（7 件，替换 calculator/current_time/http_get）

| 工具 | 参数 | 行为规范 | 拦截规则 |
|---|---|---|---|
| `read_file` | path, offset?=0, limit?=2000 | 返回带行号文本；超 256KB 截断并提示 | realpath ∈ WORKSPACE_ROOT |
| `write_file` | path, content | 全量写入；父目录自动创建 | 同上；越界报字段级错误（路径 + 合法根） |
| `edit_file` | path, old_string, new_string, replace_all?=false | str_replace 语义 | 同上；old_string 命中 0 处或 >1 处且未 replace_all → 报错并给命中数 |
| `list_dir` | path, glob?="*" | 名称 + 类型（file/dir）；glob 过滤 | 同上 |
| `bash` | command, timeout_ms?=30000（上限 60000） | 捕获 stdout/stderr 合并返回；超 64KB 截断 | **受限令牌沙箱（宿主层 fail-closed，§2.1）**：默认经 `sandbox-run` 助手执行；助手不可用即拒绝（移出 scope），绝不静默直跑；`BASH_SANDBOX=off` 显式豁免（描述如实声明无沙箱） |
| `web_search` | query, max_results?=5（1-10） | 返回 `[{title,url,snippet}]`，必带来源 URL | 后端链见 §3 |
| `web_read` | url | markdown 正文，超 32KB 截断 | scheme 白名单 http/https；15s 超时 |

## 3. bash 进程沙箱（宿主层 fail-closed，Phase 2-1）

**原则（dsh）**：提示词约束≠执行边界。沙箱在**宿主层**装配与决策（host 是策略点，tools 插件只执行）：

| 角色 | 实现 |
|---|---|
| 助手二进制 | `crates/host/src/bin/sandbox-run.rs`（与宿主同目录分发）：`probe` 探测；`exec <timeout_ms> <command>` 在 SAFER `NORMALUSER` 受限令牌下经 `cmd /c` 执行（`CreateProcessAsUserW`，从调用者自身令牌派生，无需管理员特权），stdout 输出单行 JSON `{"exit_code","timeout","output_b64"}` |
| 边界实测 | 剥离高危特权（SeShutdown/SeTimeZone/SeUndock/SeIncreaseWorkingSet）；管理组 deny-only（管理员专有文件读取被拒）；**非文件系统容器**（用户可写区仍可写，如实声明） |
| fail-closed | 助手缺失/探测失败/令牌创建失败 → host 把 `bash` 移出传给 tools 的 `TOOLS_ENABLED`（拒绝执行）；tools 侧助手异常 → `SANDBOX_FAILED` 字段级错误，**任何失败路径都不回退为无沙箱直跑** |
| 显式豁免 | `BASH_SANDBOX=off`：不传 `SANDBOX_HELPER`，bash 无沙箱直跑，工具描述如实声明 |

## 4. web 后端链（零 key 直连优先 + 区域选路 + 反爬纪律）

**可达性事实**：DuckDuckGo / Brave / Google 在中国大陆不可直连；**Bing、搜狗、百度国内直连**。国内零 key MCP 生态（ai-search-mcp / Open-WebSearch / agent-search-mcp 等）的共识做法全是「HTML 抓取公开引擎结果页 + 多引擎故障转移」——本方案直接以 tools 内部引擎链实现同等能力，不引入 MCP。

`SEARCH_REGION=cn`（国内默认，全程零 key）：

| 优先级 | 引擎 | 说明 |
|---|---|---|
| 1 | Bing HTML（cn.bing.com） | 直连、反爬最温和、中英皆可 |
| 2 | 搜狗 HTML | 直连、中文强（agent-search-mcp 的中文主力引擎） |
| 3 | 百度 HTML | 直连、反爬较强（低频调用可用） |
| 4 | 字段级错误 | 附**已试引擎列表** + 解决提示 |

`SEARCH_REGION=global`（海外默认）：`ddgs` 库（可选依赖）→ DuckDuckGo HTML → Bing HTML → 字段级错误。

**有 key 自动提升**（付费/免费额度服务，结果质量更稳）：`BOCHA_API_KEY`（博查，DeepSeek 官方搜索引擎）＞ `BAIDU_API_KEY`（千帆 AI 搜索，**每日 100 次免费**）＞ `TAVILY_API_KEY`（每月 1000 次免费）。`SEARCH_BACKEND` 可强制指定单一引擎。

**反爬纪律**（free-search-mcp 的失败经验）：不绕验证码/PoW（合规底线，引擎挑战即降级下一引擎）；每引擎简单冷却限速；错误返回结构为 `{tried:[...], hint:"..."}`。

`web_read`：优先 Jina Reader，**域名按区域切换**——cn 默认 `https://r.jinaai.cn/<url>`（国内镜像，认证与行为不变），global 用 `r.jina.ai`；非 200 回退本地直抓 + 去标签（国内站点直抓天然可达）；15s 超时、32KB 截断、必带来源 URL。Phase 2 垂直增强：CSDN 文章 / GitHub README 正文抓取（Open-WebSearch 的 `fetchCsdnArticle` / `fetchGithubReadme` 对位）。

**明确不引入的引擎**：Exa（国内不可直连、无备案、中文索引弱——国内对标品即博查，已由 key 提升链覆盖）。

## 5. scope 控制

- `TOOLS_ENABLED=read_file,bash,...`：白名单裁剪，未授权工具**Schema 与实现双不可见**（不只藏描述）；bash 沙箱 fail-closed 时由宿主改写该白名单；
- 保留名 `load_skill` 不在本注册表（归 agent-loop 路由，见 03 §3）。

## 6. 生命周期钩子

| 钩子 | 行为 |
|---|---|
| `init` | 读 env 装配：WORKSPACE_ROOT 规范化、TOOLS_ENABLED 解析、搜索后端探测（ddgs 可用性） |
| `start`/`stop` | 无 |
| `destroy` | 空（无持久资源） |

## 7. 递归子模块（tools/ 目录）

| 子模块 | 职责 | 时空约束 |
|---|---|---|
| `tools/files.py` | 4 个文件工具 + realpath 越界校验（唯一拦截点） | 拦截点=操作点，空间防越界 |
| `tools/bash.py` | 命令执行（沙箱/直跑双路径）+ 超时 + 截断 | 相对 deadline ≤ TOOLS_DEADLINE |
| `tools/web.py` | 搜索后端链 + web_read | scheme 白名单；结果必带来源 URL |

## 8. 验证点（Q）

- 越界路径 100% 拦截（含 `..`、符号链接、Windows 盘符变体）；（I）
- `edit_file` 唯一性校验：0 命中 / 多命中两分支均返回命中数；（Q）
- `web_search` 真实联网返回带 URL 结果；`ddgs` 缺失走 HTML 兜底；（Q/P）
- `TOOLS_ENABLED` 过滤后 list 与 call 行为一致。（Q）
- bash 沙箱：助手 probe 通过；沙箱内工作区可写、管理员专有文件被拒；助手缺失 → `SANDBOX_FAILED`（fail-closed 不回退直跑）。（Q/P）
