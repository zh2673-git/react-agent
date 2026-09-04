# 08 Web 配置中心与自扩展方案（Phase 4）

> 文档树：[01-项目方案](01-项目方案.md)（总纲）｜[02-架构设计](02-架构设计.md)｜[03-模块设计](03-模块设计.md)｜[04-07 分模块四层设计]。本篇为 Phase 4 增量方案，回答两个问题：
> ① Web 界面能否像 DeepSeek Harness 一样直接设置 LLM / UI / Skills？② 大模型能否在与用户交互中给自己写插件（自扩展）？
> 状态：**✅ 已实施**（2026-09-03，验收记录见 [01 §十](01-项目方案.md)）。

---

## 一、本质定义（时空契约校验）

**本质**：把「运行时配置」从 spawn 时冻结（env）升级为**活配置**（config.json 持久态 + configure op 热应用）；把「能力扩展」从开发期动作（改代码重发布）降维为**文件写入动作**（skill/tool 都是文件）——模型由此获得与用户同等的扩展权。

- **空间契约**：配置态仍是各插件自己的进程内状态（llm pack 运行时参数、tools 白名单、assets 扫描缓存），不引入共享配置中心；跨进程只传 `configure` op（线契约），无共享内存。
- **时间契约**：配置变更 = 请求级 op（立即生效，下一次 chat 即用新值）；持久化 = host 落 config.json（重启后 spawn 时还原）；自扩展 = 文件写入（write_file）→ 重扫（下次 list 可见）→ 下轮 chat 生效。全部复用既有「请求→观察→回跳」循环，无新执行流。
- **规则契约**：扩展权的边界不变——文件工具越界拦截（WORKSPACE_ROOT）、TOOLS_ENABLED 白名单、bash 沙箱 fail-closed 照常生效；**写插件 ≠ 启用插件**（两步分离，启用是显式审批点）。

### 竞品锚定

| 竞品 | 做法（推断） | 本项目对位 |
|---|---|---|
| DeepSeek Harness | Web 端可配 LLM/工具/技能；自扩展经「授权确认」落事件日志 | configure op + Web 设置面板；审批点=启用白名单，授权记录=session.trace（已具备） |
| pi | 无 Web；扩展靠文件（SYSTEM.md / skill 目录）+ 重启 | 同为「文件即扩展」，但本项目做到**不重启热生效**（list 重扫 + reload op） |
| opencode | provider 经 Models.dev 目录归一，配置文件热读 | config.json 思路同构；本项目多一层内核进程隔离 |

## 二、问题一：Web 配置中心

### 2.1 现状盘点（已核对代码）

| 项 | 现状 | 差距 |
|---|---|---|
| LLM 供应商 | `llm.chat` 的 `provider` 字段**已支持按请求覆盖**（llm_plugin.py L39：payload > env） | model / base_url / api_key 走 spawn 时 env，无法热改 |
| Skills | `assets.registry` 有 list/load；**仅 init 扫描一次并缓存** | 新文件不可见；无增删 API |
| Prompts | 同 assets（init 扫描） | 同上（本期仅做重扫，Web 管理面板不做） |
| 工具开关 | `TOOLS_ENABLED` spawn 时装配 | 无法运行时启停 |
| UI 设置 | 无 | 纯前端可解，零后端 |
| Web 路由 | 仅 `/`、`/api/events`、`/api/chat` | 需新增 config / skills 路由 |

### 2.2 设计

**配置流（两通道）**：

```
持久通道：config.json --host 启动读取--> spawn 时转 env 下发（现有机制，零改动）
热通道：  Web PUT /api/config --host 转发--> configure op（guest 进程内立即生效）+ host 落盘
```

**host 新增路由**（手写 HTTP 内追加，零新依赖）：

| 路由 | 行为 |
|---|---|
| `GET /api/config` | 回当前配置：llm（provider/model/base_url/**key 只回 `key_set:true`+尾4位，绝不回明文**）、tools.enabled、skills 计数 |
| `PUT /api/config` | body `{"llm"?:{provider?,model?,base_url?,api_key?},"tools"?:{enabled?:[str]}}` → 逐插件转发 `configure` op → 全成后落 config.json；字段级 400 |
| `GET /api/skills` | 转发 `skills.list` |
| `PUT /api/skills/{name}` | body `{"content":str}` → 校验 frontmatter（name 与 {name} 一致）→ 写 `SKILLS_DIR/{name}/SKILL.md` |
| `DELETE /api/skills/{name}` | 删目录（拒绝 `..` 等路径注入，realpath 前缀校验同 05） |

**guest 契约新增 op（只增不改）**：

| 插件 | 新 op | 语义 |
|---|---|---|
| llm-adapter | `{"op":"configure","provider"?,"model"?,"base_url"?,"api_key"?}` | 更新进程内运行时态；此后 chat 未显式指定 provider 时用新值（**显式指定仍最优先**，mock 测试不受影响）；api_key 存进程内存，不回显不落 trace |
| tools | `{"op":"configure","enabled":[...]}` | 运行时改白名单；未知工具名 → 字段级 400（列合法值） |
| assets | 无新 op | `skills.list` / `prompts.list` **改为每次重扫**（目录仅数文件，成本可忽略；frontmatter 坏目录照旧跳过） |

**新 env**：`CONFIG_FILE`（默认 `<workspace>/config.json`）。

**UI 设置**：主题/紧凑模式/字体存浏览器 localStorage，纯前端，不动后端、不进 config.json。

**web.html 面板**：顶栏加「设置」入口 → 抽屉面板三节（LLM / 工具开关 / 技能列表）。技能列表支持新建/编辑/删除（textarea 编辑 SKILL.md 原文）。

## 三、问题二：模型自写插件（自扩展三级）

| 级 | 机制 | 安全面 | 本期 |
|---|---|---|---|
| **L1 skills 自扩展** | 模型用现有 `write_file` 写 `SKILLS_DIR/<name>/SKILL.md` → assets 重扫 → **下轮 chat 系统提示词附录自动出现新技能** | 低：markdown 而已，执行仍走既有工具与沙箱 | ✅ Phase 4-2 |
| **L2 动态工具** | 模型写 `plugins/tools/tools/<mod>.py`（须暴露 `TOOLS` dict，与 files/bash/web 同规范）→ 新 op `tools.reload` 动态 import → **只装载不启用**；启用须显式 `configure`（或 Web 面板勾选）——**写与启用两步分离，启用即审批点** | 中：与 bash 等价（模型本可跑任意代码），但获 schema 常驻可见性；单模块 import 失败跳过并回字段级警告（fail-closed，旧表不受影响） | ✅ Phase 4-3（可选） |
| **L3 运行时新 guest 插件** | host 管理面运行时 spawn 新进程 + kernel register | 高：manifest 校验、进程生命周期、审批 UI 成本重 | ❌ 不做（收益不匹配；需时先评审） |

### L1 落点细节（90% 现成）

1. **提示词授权段**：agent-loop 内置系统提示词追加「技能自扩展」一节：告知模型可用 `write_file` 创建新技能（目录名=frontmatter name、必需 description、正文给执行指引）；写完后**下一轮对话自动可见**（无需 reload 调用）。
2. **天然容错**：frontmatter 写坏 → `_scan_skills` 已有的跳过行为兜底，最坏结果是技能不出现，不炸进程。
3. **授权记录**：无需新事件——`tool_call/tool_result` 已覆盖写入过程（dsh 的 authorization 与本项目 trace 同构，写文件本身即审计事实）。
4. **边界**：`SKILLS_DIR` 不在 WORKSPACE_ROOT 内时模型写不到——此时 L1 静默不可用（提示词授权段仅在可达时注入，agent-loop 探测规则：skills 根路径为 WORKSPACE_ROOT 前缀）。

### L2 落点细节

- `reload` 语义：扫描 `tools/` 目录 → 逐模块 importlib 动态加载 → 合并新表；**装载 ≠ 启用**：新模块工具进「可用池」，`TOOLS_ENABLED` 未含则不出现在 `tools.list`。
- 复用 05 的工具规范（ToolSpec 三元组 + `run(args)`），新增工具自动享有 scope 裁剪与字段级错误管道。

## 四、线契约变更清单（只增不改）

| 契约 | 变更 |
|---|---|
| `llm.chat` | **逐字不变**；同 capability 新增 `configure` op |
| `tools.exec` | list/call 不变；新增 `configure`、`reload`（L2）op |
| `assets.registry` | op 集不变；list 语义改重扫（契约文本不变） |
| `agent.chat` | 逐字不变（提示词授权段是组装层内容） |
| `memory.session` / `session.trace` | 逐字不变 |
| web 路由 | 新增 §2.2 五条；既有三条不变 |

## 五、分期实施

| 阶段 | 内容 | 预估 Diff |
|---|---|---|
| **Phase 4-1 配置中心** | config.json 持久化（host 启动读取/spawn 下发/落盘）；llm-adapter `configure` op；tools `configure` op；assets list 重扫；host 五条新路由；web.html 设置面板（LLM/工具/技能 CRUD） | ~450 行 |
| **Phase 4-2 L1 skills 自扩展** | 提示词授权段（含可达性探测）；e2e：模型 write_file 建 skill → 下轮 catalog 可见 | ~80 行 |
| **Phase 4-3 L2 动态工具（可选）** | tools `reload` op（装载≠启用、单模块失败跳过）；e2e：写坏工具文件 reload → 旧工具不受影响 | ~250 行 |

## 六、验证契约（P / Q / I）

| 维度 | 检查项 |
|---|---|
| **P 前置** | 现有 28 项测试全绿；不调用 configure 时 llm/tools 行为与现状逐字一致（env 兜底路径不变） |
| **Q 后置** | Web 改 provider/model → 下轮 chat 生效（mock 冒烟）；Web 新建 skill → 下轮 catalog 出现且 `load_skill` 可激活；模型 `write_file` 建 skill → 下轮可见；`GET /api/config` 永不回明文 key；L2：写坏工具文件 reload → 字段级警告 + 旧工具全数可用 |
| **I 不变量** | 五条既有线契约逐字不变；文件越界拦截不变（skills CRUD 同受 realpath 前缀校验）；assets 软依赖降级不变；「写插件 ≠ 启用插件」两步分离不被绕过（reload 后未 configure 的新工具不出现在 tools.list） |
