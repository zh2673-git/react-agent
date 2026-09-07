---
name: repo-explorer
description: Codebase reverse-reading discipline, two depths - (a) quick map before code changes (structure, entry points, build/test commands via list_dir + manifests; never guess the layout); (b) full six-step reverse audit (essence + spacetime contract, layered audit, execution-flow type, module recursion, dependency audit, test/PQI audit). Triggers: 熟悉仓库/解读分析项目/架构审计. Orchestrated by project-dev (逆向阶段).
origin: preset
---

# Repo Explorer（逆向阅读）

两档深度按任务选用：改代码前的**快速建图**（只读不写）；项目解读/架构审计走**六步深读**（产出报告）。

## 档一：快速建图（任务涉及代码改动时必做）

- 用 `list_dir`（recursive 视仓库规模，建议 max_depth 3）看顶层与关键目录布局；
- 读以下清单文件（存在哪个读哪个，用 read_file）：
  - `README.md` — 项目定位与用法；
  - `Cargo.toml` / `package.json` / `pyproject.toml` — 语言、依赖、构建与测试命令；
  - 目录名本身即结构信号（crates/ src/ tests/ plugins/ docs/）。
- 定位任务相关代码：`grep` 按关键词定位；有 `symbols_search` 时优先用它找函数/类的定义位置；
  从入口（main/lib/index）向下游追调用链，确认要改的文件确实在链路上；
- 改动前必读目标文件原文，确认既有风格（命名、错误处理、模块组织）。
- 探索只读不写。收尾形成三件事：**改哪些文件、按什么顺序、用什么命令验证**；
  回答中引用具体路径，让用户可核对。

## 档二：六步深读（逆向模式）

1. **本质逆向定义 + 时空契约还原**：从代码反推系统本质；回答三条——
   空间归属（核心数据谁持有？有无泄漏风险？）、时间流形态（纯顺序调用栈 / 异步运行时 / 事件循环？）、
   规则拦截点（编译时泛型/借用检查，还是运行时 if/raise？）。
2. **第0级分层审计**：对照 core / infrastructure / domain / application / runtime / interfaces
   六层映射核对实际目录与职责；**runtime 层缺失（无显式生命周期管理）= 最高级债务**。
3. **执行流类型识别**：主循环是顺序管道、事件驱动 Loop（while/select 等外部事件）、
   状态机跳转（match state 驱动），还是混合模式。
4. **模块递归拆解**：核心模块逐层做四层提取（规范/存储/流转/接口），标注生命周期钩子实际落点
   （init 在哪？destroy 由谁触发——显式调用 / GC / Drop？）。
5. **依赖方向审计**：三条铁律逐条核查——infrastructure 反向依赖 domain 实现？domain 直调 runtime
   调度器？跨层直访绕过 core/interfaces 契约？产出违规清单（位置 + 严重度）。
6. **验证契约审计**：提取现有测试，评估 P（前置就绪）/ Q（功能正确）/ I（不变量保持）的覆盖与通过情况。

### 工具映射

`list_dir` 建图 → `grep` / `symbols_search` 找定义与调用链 → `read_file` 精读关键文件 →
`bash` 跑构建/测试观察真实行为（不以文档推断代替运行证据）。

### 产出约定

- 深读产出**解读报告**：概览 / 本质+时空契约还原 / 执行流类型 / 分层审计表 / 依赖违规清单 /
  债务汇总（时空分类）/ 改进建议（短中长期）。完整报告模板见 `project-dev` 技能
  `references/project-development-prompt.md` §3.3。
- 报告落点遵循目标仓库约定；存量项目优化期**不新增 docs/ 编号文档**（构建期快照），
  审计结论沉淀进对应模块 `PLAN.md`（见 project-dev 优化模式）。
