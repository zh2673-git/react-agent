# tools 插件优化方案（模块级 PLAN）

> 定位：本文件是该插件的**演进轨迹**（审计结论 / 路线 / 迭代记录），随代码走。
> `docs/` 编号序列属构建期快照，优化期不新增；README 只在契约实际变更时同步最终形态。
> 方法论：`project-development-prompt.md`「优化模式」。
> 创建：2026-09-06 | 依据：技能自造闭环（R9）方案落点；同日 v2 修订（语言无关化）

## 一、现状审计（2026-09-06）

- 职责：生产级 7 工具 + `reload` 动态装载（fail-closed：单模块失败跳过、内置不可覆盖）
  + `configure` 白名单（**装载 ≠ 启用**）。
- 契约缺口：`reload` 只扫 `plugins/tools/` 全目录，且工具执行体只能是本进程 Python 函数
  ——技能包自包含工具（语言不限）无装载入口。
- 已知风险面（记录在案）：`reload` 装载即执行模块顶层代码（import 语义）。

## 二、方案：`tools.install` + 命令式技能工具（v2，语言无关）— 已实施（2026-09-06）

> 总纲见 `crates/agent-loop/PLAN.md` R9。核心抽象：**工具声明与执行体分离**——ToolSpec
> 三元组本就语言无关，语言绑定的只是执行体。内置 7 件保持函数池不动（平台通用能力，
> 冻结不扩展）；技能工具走**命令执行器**双通道。

- **新 op**：`{"op":"install","path":str}` → `{"ok":true,"loaded":[name]}` |
  `{"ok":false,"error":{...}}`
  - `path` 指向技能包内 `tools.json`（数组，每项 `{"name","description","parameters",
    "exec":{"cmd":[...],"cwd"?}}`）。
- **执行协议（语言无关）**：技能工具被 `tools.exec` 调用时起**子进程**执行 `exec.cmd`：
  - stdin 收 `{"args":{...}}`（模型 arguments 原样）；stdout 回
    `{"ok":true,"result":...}` | `{"ok":false,"error":{"code","message"}}`（与 Wire
    契约同形，任何语言 JSON 序列化即可实现工具）；
  - cwd 默认技能目录（相对路径资源可达）；受每步 deadline 约束（超时杀进程，字段级
    超时错误）；非零退出码 / stdout 非 JSON → 字段级错误回传。
- **装载语义（三层作用域，v2 修订）**：
  1. **装载**（install）→ 声明进进程级技能工具池；不可调用、不进任何会话清单；
  2. **启用**（configure，配置闸）→ 标记「允许生效」，全局持久（config.json）；
  3. **可见**（会话级，agent-loop 驱动）→ 仅 `load_skill(X)` 后 X 的已启用工具进入
     本会话 LLM 工具清单；`tools.list` 对外契约不暴露技能工具（清单组装在 agent-loop，
     见 R9），内置工具 tab 语义不变。
- **校验（fail-closed，任一失败拒绝且不影响既有池）**：
  1. tools.json realpath ⊆ WORKSPACE_ROOT 且 ⊆ skills root（技能目录内，防挪用）；
  2. JSON 可解析、每项 ToolSpec 三元组完整、exec.cmd 非空数组、name 合法且不与内置
     7 件 / 已装载技能工具重名（冲突拒绝，字段级错误）；
  3. 单项失败跳过不阻断其余（与 reload 纪律一致）。
- **与 reload 的关系**：reload 全目录扫描语义不变（函数池通道）；install 定点装载
  （命令通道）；二通道共用「进池不启用」纪律，互不可覆盖对方名称空间。

## 三、验证契约

- P：临时技能目录 + tools.json（合法/缺字段/重名/越界路径）+ 跨语言执行体各一
  （python 与 node 各写一个最小 echo 工具验证语言无关性）。
- Q：install 成功返回 loaded；`tools.exec` 调用技能工具走子进程并回 JSON 契约形状；
  超时/坏输出得到字段级错误；agent-loop 会话内 load_skill 后清单可见（R9 e2e）。
- I：越界/重名/坏声明被拒且既有池不受影响；未启用或未 load_skill 的技能工具调用被拒
  （字段级 400）；内置 7 件与 reload 行为不变（回归）。

## 四、风险记录（v2 收窄）

- **import 顶层代码执行面**：仅存在于函数池通道（reload），范围不变、维持后置。
- **命令执行面（v2 新增）**：技能工具 = 受控子进程执行，由三重既有机制缓解——装载
  路径 ⊆ skills root + 启用人工闸（config.json）+ deadline 超时；沙箱化（复用
  BASH_SANDBOX 受限令牌思路）记为后续增强，不阻塞主链路。
- 语言无关化的安全收益：执行体与 tools 进程隔离，插件进程不再执行技能包代码。

## 已评估不做

- 扩展 reload 支持额外扫描路径：reload 是全量扫描语义，混入技能目录扫描会让「装载池
  来源」不可预测；定点 install 更贴合技能包自包含形态，且校验面更小。
- 装载时自动启用 / install 携带 enabled 参数：违反装载 ≠ 启用，否决。
- **v1 Python-only 函数装载（importlib）**：用户指出技能工具语言不应受限；v2 修订为
  声明式命令工具，Python-only 形态作废（2026-09-06）。
- 技能工具注册为 gRPC guest（每语言常驻 guest）：重量级，技能目录动态起 guest 复杂度
  高；stdin/stdout JSON 子进程协议以最小成本达成跨语言，效果等价。

## 五、实施记录（2026-09-06）

- 落地：`install`（fail-closed：realpath ⊆ WORKSPACE_ROOT 且 ⊆ skills 根、name 合法且
  不冲突、单项失败跳过；附 `skill` 归属——agent-loop 显式传注册名，缺省回退目录名）+
  `skill_tools`（会话装配视图 / all 配置视图，不入 list 契约）+ `_exec_command`（子进程
  stdin/stdout，cwd 缺省技能目录、exec.cwd 越界拒绝、`SKILL_TOOL_TIMEOUT_SECS` 缺省 60s
  超时杀进程、stdout 契约形状 `{"ok":...}` 优先透传，否则 `TOOL_EXEC_ERROR` 附 stderr/
  stdout 尾部）。
- **延迟启用**（方案外新增，实施期发现的关键缺口）：config.json `tools.enabled` 持久化的
  技能工具名先于装载到达（重启场景），`_parse_enabled` 对未知名原会 `SystemExit` 致插件
  起不来——改为挂入 `_DEFERRED_ENABLED`，install 同名工具时自动转入启用集。
- `configure` 合法值扩为 内置/动态池 ∪ 技能工具池（启用闸对两通道一致）。
- 验证：python 行为断言 42 项全过（装载≠启用、未启用 `TOOL_DISABLED`、skill_tools 两种
  视图、configure 启用后子进程协议执行、契约违反 `TOOL_EXEC_ERROR`、重复 install pending
  收敛）；cargo 65 项全绿；浏览器 e2e 全链路通过（详见 agent-loop PLAN R9 实施记录）。

## 六、v3：内置工具能力增强（2026-09-06，用户决策后实施）

> 触发：用户提出「内置工具数量已够，要提升每件工具自身能力」。逆向审计结论：
> 搜索/查找是最大缺口；read_file 编码/二进制/超长行三大盲区；edit_file 无容错无 diff；
> write_file 非原子无备份；list_dir 不递归；bash 截断只留头部（报错常在尾部）；web_read
> 无分页无缓存。**用户决策：解冻新增第 8 件内置工具 grep；四个方向全做。**

### 方案（全部走「现有工具加参数/加智能」+ 一件新工具，ToolSpec 三元组不动）

1. **grep（新，第 8 件，tools/grep.py）**：工作区正则内容搜索。参数 pattern/path/glob/
   ignore_case/max_results(1-200,默认 50)。剪枝：点目录 + 噪音目录（.git/node_modules/
   target/__pycache__/.venv/venv/dist/build/.idea）；二进制（NUL 探测）与大文件（>1MB）
   跳过并计数；max_results 全局早停；输出 `path:line: text`（ripgrep 风格）+ 汇总行。
   复用 files 的 _guard/_display/_decode（唯一拦截点纪律）。
2. **read_file**：编码探测链 BOM(utf-8-sig/utf-16) → utf-8 严格 → gbk 严格 → utf-8
   replace（结果附编码标注）；图片/二进制 magic+NUL 识别（不再出乱码，图片提示走聊天
   附件路径）；超长行按 2000 字符截断并标注全长；空文件明确提示。
3. **edit_file**：精确 0 命中时降级「行尾空白 + CRLF/LF 容错」匹配（缩进不容错——重排
   风险高）；仍 0 命中给近似候选行线索；成功后返回 unified diff（n=1，4KB 截断）+
   match_mode 标注；写回按探测编码回写（GBK 文件不再被静默转码为 UTF-8）。
4. **write_file**：临时文件 + os.replace 原子写（失败清理 tmp）；覆盖已有文件默认写
   `name.bak`（backup 参数可关，.bak 只留最近一份）；返回 changes 行数统计（difflib）。
   **顺带修隐性 bug**：统一 `newline=""` 写入——原 write_text 默认平台翻译会把 LF 文件
   改写成 CRLF。
5. **list_dir**：recursive(默认 false) + max_depth(1-8,默认 3) 递归（文件平铺相对路径）；
   sort=name|size|mtime；每条附 mtime；max_entries(默认 500) 截断提示；递归时剪枝噪音/
   点目录（include_noise 关闭剪枝）。
6. **bash**：输出截断改「头部 40KB + 尾部 20KB」双保留（报错信息多在尾部）；后台作业
   （job 登记 + 轮询）改动面大，记为后续增强不在本轮。
7. **web_read**：offset/limit 字符分页（默认 0/32000，上限 64000），返回 total_chars/
   next_offset；URL 级 10 分钟进程内 LRU 缓存（32 条，>256KB 不缓存）。

配套：tools_plugin `_BUILTIN_MODULES` + grep（reload 保护）、manifest 0.3.0、
`tools/ 包 = files/bash/web/grep` 表述更新；README 契约同步（7 件 → 8 件）。

### 验证契约（P/Q/I）

- P：临时工作区夹具（utf-8/gbk/BOM/二进制/png/超长行/LF 文件/多级目录/噪音目录）。
- Q：约 45 项行为断言（grep 命中与剪枝计数、read 编码与二进制标注、edit 容错/diff/
  编码回写、write 原子/bak/统计/LF 保持、list_dir 递归排序截断、bash head+tail、
  web_read 缓存分页、plugin list=8/reload 保护/configure）。
- I：回归——纯文本读写编辑行为不变（除新增字段）、TOOLS_ENABLED 语义不变、reload
  fail-closed 不变、越界拦截不变；web_search 零改动。

## 七、v4：`symbols_search` 符号检索工具 ✅（2026-09-07，方案修订后实施）

> 依据：代码智能体方向决策——把「文本搜索」升级为「语法级符号地图」，作为 LSP 的先行层。
> 本节为方案总纲；配套行为层（代码 profile）见 `plugins/assets/PLAN.md` 五，接口层
> （Monaco 预览）见 `crates/host/PLAN.md` W11。

### 审计结论（缺口）

- grep（v3 第 8 件）是文本匹配：无法区分定义/调用/注释/字符串，模型需自行从命中噪声
  中判断哪一行是定义；无法回答结构化问题（「这个函数定义在哪」「仓库有哪些函数/类」）；
  原始文本行 token 效率低，且易在 `TOOL_RESULT_LIMIT` 截断中丢失关键行。

### 方案（tree-sitter 语法级符号查询）

- **ToolSpec**：`{"name":"symbols_search","parameters":{"query"?,"file"?,"path"?,"kind"?,"language"?}}`
  → 输出 `[{"kind","name","file","line","signature"?}]`（kind: function/class/method/…）。
- **query 语义（修订明确，2026-09-07）**：query = 符号名**子串匹配**（大小写不敏感）+
  kind/language 过滤；**不暴露 tree-sitter 原生 S-expression query**——模型手写该
  query 语法错误率高，且 Q 用例（`query:"chat_run"`）语义即名字级搜索。
- **技能工具超时契约扩展（随 tdd 需求引入，2026-09-07 用户确认；✅ 已实施，见下）**：
  tools.json exec 结构增可选 `"timeout_secs"`（`{"cmd":[...],"cwd"?,"timeout_secs"?}`）——
  缺省仍走全局 `SKILL_TOOL_TIMEOUT_SECS`（60s），声明则按工具覆盖（tdd 跑测试需长超时）。
  `_exec_command` 一处改动，对全部技能工具生效。
- **落点与通道**：`tools/symbols.py` 函数池通道（与 grep 同法）→ 拟为第 9 件内置工具。
  **需解冻决策**：内置 8 件冻结是既定惯例（v3 解冻 grep 有先例），待用户确认。
- **依赖（修订，2026-09-07 用户确认）**：`tree-sitter-language-pack` 单包（内含
  rust/python/typescript/tsx/javascript 等预编译 grammar，wheel 直装）。原案
  `tree-sitter` + 四官方语言包**作废**——0.21→0.22 绑定 API 断裂（`set_language`
  移除），逐包版本矩阵坑多；单包使 fail-closed 判定更清晰（缺一个包 = 全缺）。
  **fail-closed 降级**：依赖缺失时该模块跳过装载（与 reload 纪律一致），
  插件与其他 8 件不受影响。
- **时空契约**（方法论 §2.1）：空间 = 不持久化索引（即时解析，无跨调用可变态，A1
  精神）；时间 = 单次请求-响应（无循环无状态机，主循环 agent-loop 零改动）；规则 =
  复用 files `_guard` realpath 越界拦截（唯一拦截点纪律）+ 输出受既有截断闸管理。
- **语言支持**：rust/python/typescript/tsx/javascript（language-pack 单源 grammar，
  tsx 顺带覆盖，零额外成本）；未覆盖扩展名返回字段级错误（附支持清单）。

### 验证契约（P/Q/I）

- P：pip 依赖装齐；缺依赖场景插件可启动（symbols 不注册，其余 8 件正常）。
- Q：对本仓库自测——`symbols_search{query:"chat_run"}` 返回 lib.rs:1288 定义且
  kind=function；file/kind 过滤正确；空结果与未知语言给字段级错误。
- I：既有 8 件行为不变；reload/install/configure 语义不变；越界拦截不变；
  `cargo test --workspace` 全绿（本改动零 Rust 侧改动）。

### T0 实施记录：exec.timeout_secs 契约切片 ✅（2026-09-07）

- 落地：`_install_one` 校验 `exec.timeout_secs`（可选正数，bool 显式排除——bool 是 int
  子类否则 `true` 会被当 1.0 放行）并入池存储；`_exec_command` 优先读声明值、缺省回落
  全局 `SKILL_TOOL_TIMEOUT_SECS`；模块 docstring 契约同步。对全部技能工具生效，与
  symbols_search（v4）解耦可先行合入。
- 验证：python 行为断言 11 项全过——声明 6s 覆盖全局 2s（sleep 3 成功且实测 ≥3s）、
  缺省回落全局（sleep 3 → TOOL_TIMEOUT）、非法值 0/-1/"abc"/true 全部 fail-closed 拒绝
  且错误指向 timeout_secs、池内存储核验、configure 语义不变。

### v4 实施记录（2026-09-07）

- 落地：`tools/symbols.py`（tree-sitter 即时解析，无索引无跨调用状态）+ `_BUILTIN_MODULES`
  加 symbols + manifest 0.4.0 + `config.rs::ALL_TOOL_NAMES` 补齐 grep/symbols_search
  （**顺带修 v3 遗留缺口**：grep 上线时漏同步，host 工具清单仍为 7 件）。
- 实现要点（与方案的偏差与细化）：
  - kind 判定按节点形状：rust `function_item` 父为 declaration_list → method（impl/trait
    内），python `function_definition` 祖链为 class body → method，其余 function；
    rust 另出 struct/enum/trait/impl/module/typedef/const，ts/tsx/js 另出 class/method/
    interface/typedef/enum + `variable_declarator`（值为 arrow_function/function 才算
    function，排除 `const x = foo()` 调用结果）。
  - 输出为对象 `{"symbols":[{kind,name,file,line,signature?}],"total","truncated",
    "skipped"?}`（方案写数组——加元数据让模型感知截断/跳过计数，自描述更好）。
  - 参数闸：空 query / 未知 kind / 未知 language / file+path 同给 → 字段级错误；
    file 单文件与 path 目录二选一；越界走 `_guard`（唯一拦截点）；不存在路径 NOT_FOUND；
    空结果是合法观测（返回空数组，不报错）。
  - **依赖版本收窄（实施期发现）**：`tree-sitter-language-pack` 必须 pin **<1.0**——
    1.x 已重写为 Rust 核心 + **按需下载 grammar**（首次调用触发网络下载，离线即失败），
    0.13.0 才是 wheel 内置预编译 grammar 的最终版。缺依赖场景由 tools_plugin 顶层
    try/except 降级：仅 symbols_search 不注册，其余 8 件与 reload 正常。
  - Q 用例勘误：`query:"chat_run"` 的定义在 R12 拆分后位于 chat.rs:245 且 kind=method
    （impl 内），方案写的 lib.rs:1288 是拆分前坐标。
- 验证：python 行为断言 20 项全过（Q 用例/过滤/方法判定/参数闸/越界/空结果/list=9/
  reload 保护/manifest）+ 缺依赖降级断言（list=8、reload 正常）；`cargo test --workspace`
  71 项全绿（ALL_TOOL_NAMES 扩容后 e2e 均为成员断言不受影响）；README 同步最终契约
  （9 件 + R9 ops + 错误码 + 延迟启用语义）。

### 演进路径与 LSP 决策（已评估，暂不做）

| 阶段 | 范围 | 状态 |
|---|---|---|
| v4 | tree-sitter 语法级符号查询（本节） | ✅ 已实施 |
| v5 | 语法级引用查询（call_expression callee 语法上下文匹配） | 预留 |
| v6 | LSP 桥接工具（语言白名单渐进 rust→py→ts） | 已评估，暂不做 |

**v6 触发条件**（满足其一才重新评估）：Q 层实测 agent 定位失准率显著上升（宏/泛型重
仓库）；出现大规模 rename 类任务且编译器兜底验证成本过高。
**暂不做理由**：agent-loop 多轮迭代 + LLM 推理可补偿语法级模糊；漏改引用由 bash
编译/测试兜底（改后验证闭环）；LSP server 启动/内存/生命周期成本与「问一次就走」
使用模式不匹配。升级形式 = 新增 tools 模块（reload/install 装载），编排层零改动。

### 已知限制

- 语法级非语义级：不解析宏展开（macro_rules!/derive）与跨文件类型分发；
- 语义级缺口由 agent 的 bash 编译/测试验证闭环兜底（方案定位即如此，非缺陷）。

## 八、MCP stdio 适配：第三工具池 ✅（2026-09-07 实施）

### 审计结论（缺口）

- 工具来源仅两类：内置函数池（9 件冻结）+ 技能工具池（install 声明式）。用户「按需装
  外部能力」无入口——MCP 生态（文件系统 / GitHub / 浏览器等现成 server）接不进来，
  每个外部能力都得手写技能工具。
- MCP 本质 = 子进程 + JSON 协议，与技能工具 exec 协议同构；缺的只是协议映射与生命周期管理。

### 方案

- **落位**：tools_plugin 内实现 **mcp 池**（第三池：内置 ∪ 技能 ∪ mcp）。不另立插件——
  guest 不可互调，独立插件进不了统一工具池，agent-loop 契约就得改；本方案契约零改动。
- **配置**：server 清单 `{name, transport:"stdio", command, args?, env?, allowed_tools?}`，
  通道实施时择一（倾向 `MCP_SERVERS` env JSON，与 TOOLS_ENABLED 下发同风格；host 从
  config.json 读出后经 env 传入）。http/sse 传输为后续 Phase。
- **生命周期（时空闭环）**：装载后 initialize → tools/list → 映射为 ToolSpec 进池，
  name 加命名空间 `mcp__{server}__{tool}` 防撞名，description / inputSchema 透传；
  server 子进程随 tools_plugin 退出逆序回收（destroy）；配置热变更杀旧起新；崩溃标记
  池内失效并回字段级错误，重启带退避，不拖垮其余工具。
- **三层作用域复用（不新发明）**：装载 = server 已配置进池；启用 = TOOLS_ENABLED 勾选
  （config.json 持久化，人工闸——MCP server 等价任意代码执行面，与技能工具同级管控）；
  可见 = agent-loop 清单组装（MCP 工具不绑技能，常驻启用即全会话可见，与 agent-loop 对齐注入点）。
- **调用**：`tools.call` 命中 mcp 池 → JSON-RPC `tools/call` → 结果映射回
  `{"ok":true,"result":...}` 契约形（content blocks 取 text 拼接；错误码透传为字段级错误）；
  复用现有 deadline 超时杀进程机制。
- **Windows**：command 解析复用 npm.cmd 先例（npx.cmd / cmd /c 包装）。
- **前端增量（随本 Phase）**：工具 tab 增「MCP 服务」区块——server 表单（name/command/args/env）
  + 运行状态灯 + 启用勾选；数据走现有 /api/config 通道（config.json `mcp_servers`），
  工具清单增 pool 来源字段（`builtin|skill|mcp`）供分组渲染；index.html 运行时 serve，刷新即生效。

### 验证契约（P/Q/I）

- P：本地最小 MCP server fixture（python 实现 stdio JSON-RPC：一个回显工具 + 一个故意崩溃的 server）。
- Q：list 含 `mcp__` 前缀工具且 schema 透传；call 回显成功；崩溃 server 得字段级错误且不影响其他工具。
- I：未配置 MCP_SERVERS 时 mcp 池为空、内置 9 件与技能工具行为零变化；与内置/技能工具重名拒绝；
  server 死后调用得错误而非挂死；tools_plugin 退出无孤儿进程。

### 风险记录

- 子进程管理面（僵尸 / 崩溃循环）→ 逆序回收 + 重启退避；stdio 死锁（大 payload）→ 行协议
  分帧 + deadline 复用。
- 安全面：MCP server = 任意代码执行，等同 bash 增强面 → 启用人工闸为硬性前置，不许静默启用。
- inputSchema 与 ToolSpec parameters 差异 → 透传不深校验，消费在模型侧；失败即字段级错误可观测。

### 已评估不做

- agent-loop 直连 MCP（绕过 tools 插件）：破坏统一清单组装与三层作用域，契约改动不可接受。
- MCP resources / prompts 语义：只取 tools 语义，其余等真实需求出现再议。

### 实施记录（2026-09-07）

方案落定与偏差记录（相对上文方案稿）：

- **通道定案**：`MCP_SERVERS` env（整体 JSON 序列化自 config.json `mcp_servers`），host
  `config.rs` 持久通道下发（与 TOOLS_ENABLED 同风格）；热改无意义（server 子进程生命周期
  归 init/destroy），改后需重启 host——方案稿「热变更杀旧起新」收窄为「不热更」。
- **客户端**：`plugins/tools/tools/mcp.py`（McpServer）——stdio JSON-RPC 行协议分帧；
  initialize（PROTOCOL_VERSION 2024-11-05）→ notifications/initialized → tools/list →
  tools/call；读线程 + 响应队列 + id 自增锁；崩溃重启带退避（`_last_start` 单调钟节流）；
  destroy 逆序 terminate + wait 回收，无孤儿进程。
- **第三池集成**（tools_plugin.py）：`_register_mcp_tools()` 命名空间
  `mcp__{server}__{tool}` 净化落池，与内置/技能工具撞名跳过（fail-closed 打日志）；
  list（all=true）附 `mcp_server` 字段 + inputSchema 透传为 parameters；call 命中 mcp 池
  → content text 拼接回 `{"ok":true,"result"}`，isError=true → `MCP_TOOL_ERROR` 字段级
  错误；未入池（崩溃/未配置）调用即 `UNKNOWN_TOOL` 不挂起；启用闸复用 `_parse_enabled`
  （装载≠启用语义与内置一致）；新增 `mcp_tools` op 返回 `{servers,tools}` 配置视图
  （server 状态 ready/failed + error + 工具数）。
- **host**：config.rs 透传 `MCP_SERVERS`；api.rs GET /api/config 内置视图过滤 `mcp__`
  前缀，另发 `mcp` 区块（servers + tools）。
- **前端**：工具 tab 增 MCP 区块——server 状态行（名称 + 状态徽章 ready/failed/starting/
  stopped，failed 悬停见 error）+ 外接工具勾选（与内置同一白名单，保存合并提交）。
  方案稿「server 表单」收窄为只读视图 + config.json 手改重启（编辑面留待真实需求）。
  后续演进（来源区分轮）：工具 tab 改为**三池统一分组渲染**——`/api/config` tools 改为
  聚合数组（每项 `{name,enabled,pool:"builtin"|"skill"|"mcp",description,parameters}`，
  附 skill/mcp_server 来源徽章），前端按 pool 分组（组头+计数+说明），条目点击折叠展开
  配置详情（description + 参数摘要），再点收起；mcp 区块只留 servers 状态行。
- **agent-loop 零改动**：tools.list 已含启用 MCP 工具（ToolSpec 反序列化忽略未知字段），
  常驻可见语义自动成立。
- **e2e**：`crates/host/tests/e2e_mcp_tools.rs` + fixture `tests/fixtures/mcp_echo_server.py`
  （echo 回显 / boom isError / RA_MCP_CRASH 握手后崩溃）——单测覆盖 Q1-Q6 + I1/I2：
  清单注入、schema 透传、调用回显、工具级失败、崩溃 fail-closed、mcp_tools 视图、
  configure 收窄与未知名拒绝。
- **边界说明**：PROTOCOL_VERSION 固定 2024-11-05 协商标；allowed_tools 白名单字段暂未
  消费（净名后全部入池，启停走统一 TOOLS_ENABLED 闸）。

### 演进：工具 tab 折叠分组（§八后端零改动，2026-09-07）

多 MCP server 接入后工具清单臃肿（高德一家 12 行，N 个 server 线性膨胀）。仅改前端
渲染层，三池契约与保存链路零改动：

- **折叠分组**：池级（内置/技能/MCP 三池头可点击收起整池，默认展开）→ 来源级（技能池按
  `skill`、MCP 池按 `mcp_server` 分组——组头 = 总开关 checkbox +
  来源名 + 工具计数 +（MCP）状态灯，默认收起）；内置 9 件少而稳定平铺。
  层级间 `stopPropagation` 隔离（组头点击不得冒泡触发池折叠）。
- **server 级总开关**：组头 checkbox 三态（全勾 / 全不 / partial `indeterminate`），组内
  勾选变化实时回写；点击 = 整组勾/清，保存时仍展开为明细白名单（原语义不变）。
- **排序**：MCP 组 ready 在前、failed 垫底（故障一眼可见）；技能组按名稳定排。
- **选择器隔离**：白名单收集限定 `#tools-list .toolitem input:checked`——组头总开关
  checkbox 不混入（曾经裸 `input:checked` 会把组头收进白名单）。
- 独立函数 `appendToolRow`（内置/折叠组共用行渲染 + 详情折叠）。
- 验证：浏览器 mock 数据实测——分组/排序/展开收起/全启/三态回写/选择器隔离全过；
  真实数据下 amap（key 空 → failed 垫底）渲染符合预期。
- **二次演进（池级折叠 + key 配置内嵌）**：三池组头本身可点击收起整池（层级间
  `stopPropagation` 隔离）；原 tab 顶部独立「MCP 服务 API Key」区块撤销，key 输入
  内嵌到「MCP 外接」各服务折叠组内（`.tgcfg`：env 输入 + 组内独立「保存 Key」按钮，
  仅落盘重启生效）——信息架构对齐「选中某个 MCP → 填 key → 保存」直觉；
  tools-save 收集逻辑同步瘦身（#mcp-key-list 死代码清除）。PUT 落盘链路浏览器实测通过。
- **三次演进（失败 server 可见性）**：装载失败/0 工具的声明 server 也要渲染组壳
  （failed 状态灯 + 失败原因 + key 配置区照常内嵌）——否则「看不到 MCP 外接组 →
  无从发现该填 key」死锁；池级短路放行条件同步放宽（`items.length === 0 &&
  cfgMcp.servers` 非空不 continue）。真实数据实测：amap key 空 → 组可见、
  failed 灯、占位文案含「填好 Key 保存后重启 host 重试」。

## 九、媒体生成（生图/生视频）：媒体模型接入方案 ✅（2026-09-09 立项并实施）

> 用户需求：支持生图、生视频的模型。本节维护总纲与状态，实施记录见文末；
> host/前端切片的迭代记录分别落各自模块 PLAN。

### 审计结论（挂接点，全部已对码）

| 层 | 现状 | 结论 |
|----|------|------|
| 工具层 | 技能工具 = SKILL.md + tools.json（ToolSpec `exec.cmd` 子进程，stdin `{"args":{...}}` / stdout JSON，`tools_plugin.py` L530-572）；`exec.timeout_secs` 契约已有（§七 T0）；内置 9 件冻结 | 生图/生视频走**技能包 + 技能工具**，零内核/内置改动，符合「只加文件」不变式 |
| 配置层 | config.json `llm` 段 → `PUT /api/config` 落盘（`web/api/config.rs` L242）+ env 热应用；guest 侧 `passthrough_env` 白名单（`config.rs` L115-129，`BOCHA_API_KEY` 等 key 透传先例） | 新增 `media` 配置段 + `MEDIA_*` env 键，沿用同两条既有管道 |
| 产物层 | `PRODUCT_EXTS`（`agent-loop/src/tools_exec.rs` L349）含图片 png/jpg/jpeg/gif/webp/svg，**无视频**；前端产物卡点击分流（`app-files.js` L160-165）图片/ html/svg/pdf → 新标签原生渲染 | 图片零改动即通；视频加扩展名 + 前端 `<video>` 内联增强 |
| 对话层 | `llm.chat` 流式管线按 text/reasoning 帧设计 | 模型直出多模态（如 gemini-image）不在本轮（见已评估不做） |
| 超时约束 | bash 超时上限 60s（`docs/05` L22-32）；技能工具 `timeout_secs` 可声明但长任务不可靠 | 视频生成（1-10 分钟级）采用**两段式工具**（submit/poll），不改超时契约 |

### 方案：技能包 `media-gen` + 三切片

**M1 生图切片**——`skills/media-gen/`（SKILL.md + tools.json）+ 技能工具 `image_gen.py`：
- 调 OpenAI 兼容 `/v1/images/generations`（`b64_json` 或 url 下载二选一），成品落
  产物目录（outputs 规约实施时对码），返回 `{ok, path, bytes, note}`
- 参数：`prompt`（必填）/ `size` / `n` / `model`（可覆盖默认）
- 产物卡经既有 PRODUCT_EXTS 图片分支自然呈现，前端零改动

**M2 生视频切片**——两段式贴合 ReAct 循环与超时契约：
- `video_submit.py`：提交异步任务 → `{ok, task_id, note}`（立即返回，不阻塞）
- `video_poll.py`：`task_id` → 查状态；`succeeded` 即下载视频落盘返回 path；
  `running/failed` 返回状态与原因，agent 决定继续轮询或报错
- provider 形态参数化：提交/查询/下载三 URL 模板 + 轮询间隔/上限，兼容硅基流动/
  可灵/即梦类「提交→轮询→取片」API 形状；站点差异以 media 配置适配，不做代码级 provider

**M3 配置与前端切片**：
- config.json 新 `media` 段：`{image:{base_url,model,key}, video:{base_url,submit_url,query_url,model,key,poll_interval,poll_timeout}}`
  → `PUT /api/config` 增 media 分支校验落盘 + `apply_config_file_to_env` 映射
  `MEDIA_IMAGE_*` / `MEDIA_VIDEO_*` + `passthrough_env` 增键（tools guest → 技能工具
  子进程按既有 env 继承链取得）
- 设置面板 LLM tab 增「媒体模型」区（生图/生视频各一组 base_url/model/key 字段，
  复用既有保存链）；key 掩码回显同 llm.key 规则
- `PRODUCT_EXTS` 两处同步加 `mp4/webm/mov`（agent-loop 探测 + host 侧产物过滤，
  实施时 grep 全仓对齐）；产物卡视频内联 `<video controls preload=metadata>` 预览
  （新标签原生播放为兜底）
- SYSTEM.md 补一段媒体工具使用纪律（何时选生图/生视频、产物引用规约）

### 已评估不做

- **对话模型直出多模态**（chat 返回图片帧）：流式协议需新事件类型 + 各 provider 差异大，
  等真实需求立项
- **llm-adapter 内做 media op**：媒体模型与对话模型的配置/生命周期耦合，且 llm-adapter
  的流式旁路管道对同步/轮询型 API 无收益；工具层直连更简单
- **host 内嵌独立生图 UI**（绕过 agent）：与 agent 平台定位不符，远期可选

### 验证契约（P/Q/I）

- **P**：media 段 PUT 落盘 + env 映射 + 透传（PUT 后 tools guest 内 env 可见）；
  技能三作用域闸门（装载→启用→load_skill 后工具可见）既有机制零改动通过
- **Q**：mock 生图端点（本地 http server）出图 → outputs 落盘 → 产物卡可见可预览；
  mock 视频端点 submit→poll（running 若干次）→succeeded→下载全链路；failed/超时
  路径报错可读；无 media 配置时工具返回「未配置」引导文案而非异常
- **I**：既有测试全绿（改动面 = passthrough_env 增键 / PRODUCT_EXTS 增量 / put_config
  media 分支，均为增量不改既有语义）；`cargo test --workspace` + 技能工具协议单测
- **状态**：✅ M1/M3/M2 已实施（2026-09-08，同日立项当日落地）。

### 实施记录（2026-09-08）

- 交付：`plugins/assets/skills/media-gen/`（SKILL.md + tools.json + tools/{_common,image_gen,video_submit,video_poll}.py，出厂第 5 技能）；config.rs media→8 个 MEDIA_* env 映射 + passthrough；put_config media 分支（宽松校验 + 逐字段 merge，null/空串不覆盖）+ get_config media 回显（key 掩码）；PRODUCT_EXTS 全仓唯一副本加 mp4/webm/mov；前端设置面板「媒体模型」区 + 产物卡 `<video>` 内联预览；SYSTEM.md 媒体纪律节；.gitignore 出厂技能白名单补 media-gen（**漏白名单则技能不入库，克隆即失效——实施时曾漏，实测发现补上**）。
- 偏离（合理）：工具输出走技能工具 Wire 契约 `{"ok":true,"result":{...}}`（非方案文字的裸 path）；media 段为持久通道（重启生效，与 MCP_SERVERS 同语义，guest spawn 时 env 固化）非热通道；未配置返回 MEDIA_NOT_CONFIGURED 引导而非异常。
- 验证：agent-loop 49 测 + host --lib 21 测（含 2 新 media 测试）全绿；py_compile 4 脚本过；mock 冒烟 4 项（b64 生图落盘 / submit 宽容解析 / poll RUNNING→SUCCEEDED 下载 / 未配置引导）全过；实测 PUT→落盘→回显掩码→技能装载（skills_count 5→6）全链路通过。**实施中抓到并修复一处缺陷：media_config_view 已定义但未挂入 GET /api/config 响应（回显丢失），已修复并回归。**
- 时限联动（同日另一改动）：LLM_DEADLINE 120→600s / PROVIDER_DEADLINE 110→590s / CHAT_BUDGET_SECS 缺省 300→900s（深度思考模型长推理常态，三值须满足 预算 ≥ 总闸 ≥ provider 闸 的配对关系）。

### 补记（2026-09-09，实测驱动的三处修正）

- **image_gen 适配 ModelScope 生图 API 全面异步化**（实测发现：同步调用 400「does not support
  synchronous calls」，要求 `X-ModelScope-Async-Mode: true` 头；响应 200 = `{task_id,task_status}`
  而非 OpenAI `data`；轮询 `GET /v1/tasks/{id}` 须带 `X-ModelScope-Task-Type: image_generation`
  头——缺头报「task not found」，任务按类型分命名空间；SUCCEED 后取 `output_images`，Turbo 模型
  ~15s 出图）。响应形状三分支兼容：`data`（OpenAI 同步）/ `images`（旧 ModelScope 同步）/
  `task_id`（异步轮询）；提交头无条件携带（其他 OpenAI 兼容端点忽略未知头，零影响）。
- **修不传 `n` 的 KeyError**：`items[: body["n"]]` → `body.get("n", 1)`（存量缺陷，此前在更早
  环节就报错未暴露）。
- **validate_media 接线修复**：校验函数存在且有测试但 put_config 生产路径从未调用（dead_code
  警告暴露），非法 URL 会静默落盘——media merge 前补校验、K400 拒绝。
- 排障方法论教训：PowerShell 5.1 向 curl.exe 传 JSON 会被吞引号（bogus 模型与正确模型同报
  invalid prompt 即此症状），必须文件传 body 或直接 python+httpx 走真实代码路径测。

## 十、密钥脱敏闸（W18 安全边界）✅（2026-09-09 实施）

### 威胁模型

用户报告：agent 对话能把全部 key 明文显示出来。泄露路径不止一条——工作区=代码根时
`read_file config.json` 直读；任意工作区下 bash 可任意路径读文件、`printenv` 看 env
（密钥都在环境变量里）；读到后 LLM 即可原样复述。设置面板掩码回显（尾 4 位）只防前端展示，
防不了工具链路。

### 方案：唯一咽喉脱敏，不逐工具堵

- **闸位**：agent-loop `act_exec` 返回值（会话历史 + 事件 + trace 的共同源头）+ `act_begin`
  的 tool_call args（前端内联 diff 数据源）——所有工具（read_file/bash/grep/printenv/子代理）
  汇入同两个咽喉，一处闸全覆盖。LLM 从未见过明文就吐不出明文，最终答案自然无明文。
- **密钥清单**（`secrets.rs` OnceLock 惰性收集）：CONFIG_FILE 的 config.json 通用规则——任意
  层级键名含 key/token/secret/password（忽略大小写）的字符串值（覆盖 llm.api_key、media.*.key、
  mcp_servers[].key 及未来新增段）；env 兜底 `OPENAI_API_KEY`/`ANTHROPIC_API_KEY`/
  `MEDIA_IMAGE_KEY`/`MEDIA_VIDEO_KEY`；<6 字符不收集（防误杀普通词），去重。
- **替换占位符**：`«KEY_MASKED»`（肉眼可辨、不像任何合法值）。
- **纵深防御**：SYSTEM.md 新增「密钥纪律」节（不在回答复述、引导走设置面板、调试配置用
  get_config 脱敏回显、不读 config.json 原文）；GET/PUT /api/config 本就掩码（既有）。

### 诚实边界

- 历史 trace（`.stream/traces/*.jsonl`）不追溯清洗——修复前已落盘的明文仍在；已暴露的 key 建议轮换。
- bash 间接写/读不做路径级阻断（沙箱管写不管读）；值脱敏在输出侧兜底，与路径闸正交。

### 验证

- secrets 单测 4 项（嵌套替换/无密钥 no-op/收集去重与短值过滤/敏感键名发现）+ agent-loop
  既有 37 集成全绿；`act_exec`/`act_begin` 咽喉接线由集成路径覆盖。
