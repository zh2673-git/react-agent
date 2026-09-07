---
name: project-dev
description: Meta-methodology orchestrator for project-level work (build-from-scratch / reverse architecture reading / iterative optimization). Decides the working mode, routes phases to companion skills (repo-explorer = reverse reading, tdd = verification), enforces the PLAN.md iteration loop and P/Q/I acceptance. Load FIRST for 做项目/从零开发/设计方案/重构优化/解读分析项目; skip for small one-off edits.
origin: preset
---

# 项目开发方法论（编排层）

本技能是「时空运行时版」项目方法论的**编排层**：判定模式、路由阶段、守住收敛。
执行细节分流：怎么读仓库 → load `repo-explorer`；怎么跑测试 → load `tdd`；
四层模板 / 报告模板 / 完整公理体系 → read_file 本目录 `references/project-development-prompt.md`（权威全文）。

## 1. 模式判定（第0步）

| 用户意图特征 | 模式 |
|---|---|
| 「我想做…/帮我设计…/从零开发…」 | **正向**（从零生成） |
| 「分析这个项目/解读一下」或给出仓库地址、粘贴源码 | **逆向**（从源码还原设计） |
| 「优化/重构这个模块/持续修缺陷」（存量项目） | **优化**（逆向的延续，按方案迭代） |
| 不明确 | 反问用户，确认后再动工 |

## 2. 底层公理：时空 + 规则

一切系统 = **空间**（内存/数据归属）+ **时间**（执行流：顺序/分支/循环）+ **规则**（类型/契约/沙箱）。
推论：**运行时/内核 = 时空管家**——开辟空间（init/注册）、编排时间（主循环/调度）、强制规则（拦截越界）。

## 3. 递归四层模型

任意粒度代码单元（项目/模块/类/函数）都按四层拆解：

| 层 | 问的问题 |
|---|---|
| 数据规范 | 空间里放什么？类型/约束/枚举 |
| 数据存储 | 空间落在哪？堆/文件/DB/缓存 |
| 数据流转 | 数据在时间轴上怎么变？调用链/状态机/事件 |
| 数据接口 | 外部入口是什么？API/CLI/公共方法 |

第0级映射（项目级目录职责）：`core/`（纯规则，不依赖任何人）→ `infrastructure/`（空间落地，
只依赖 core 抽象）→ `domain/`（业务规则，不直依赖 infrastructure 实现）→ `application/`（编排）→
`runtime/`（生命周期 init/start/stop/destroy + 主循环，只管启停不写业务）→ `interfaces/`（入口）。

**依赖铁律**：infrastructure 不反向依赖 domain 实现；domain 不直调 runtime 调度器；跨层只走 core/interfaces 契约。
**递归规则**：第2级起锚定父本质（「在{父本质}约束下，本模块本质是…」），超过 3 层终止。

## 4. 正向模式：先契约后代码

1. **program.md 闸**：无 program.md 时先出草稿（目标 / 语言 / P·Q·I 具体值 / 修改范围 /
   约束 / 研究方向 + 时空契约确认项：核心数据谁独占、主执行流是顺序管道/事件循环/状态机），
   **用户确认前不动工**；首行声明 `探索`（精简方案+代码）或 `生产`（完整文档树）。
2. **五步**：本质定义 → 因果链+选型推导 → 第0级映射分配目录 → 递归四层设计 → 契约验证。
   本质定义必须自答三条时空契约：空间归属（谁独占/共享、何时回收）、时间流形态、规则拦截（编译时/运行时）。
3. 文档模板（01-项目方案固有结构）见 references。

## 5. 逆向模式 → load repo-explorer

六步审计：本质逆向+时空契约还原 → 第0级分层审计（**runtime 层缺失 = 最高级债务**）→
执行流类型识别 → 模块递归拆解（钩子落点）→ 依赖方向审计 → 验证契约审计（现有测试 vs P/Q/I）。
产出解读报告；存量项目优化期**不新增 docs/ 编号文档**，审计结论沉淀进模块 PLAN.md。

## 6. 优化模式：PLAN 纪律

- 方案落在**模块目录内**（如 `crates/{module}/PLAN.md`，随代码走）；`PLAN.md` = 演进轨迹
  （审计结论/路线/迭代记录），`README.md` = 对外契约（仅契约实际变更时同步最终形态，不写过程进度）。
- 迭代循环：Phase 1 更新 PLAN（目标/方案）→ Phase 2 代码 → Phase 3 测试 + 保留/回退决策
  → PLAN 追加迭代记录。评估过不做的方案记 PLAN「已评估不做」，避免重复尝试。
- 跨模块节奏：先修规则层（正确性），再修时间层（效率/健壮性），最后工程面收敛。

## 7. 编码纪律

先输出设计思路（本质/契约/四层）再写代码；新模块显式写 init 与 destroy（或语言等效物：构造/析构、Drop）；
简洁优先——不加未要求的辅助函数/装饰器/抽象层；不引入技术栈决策未列的依赖；
改动 ≤50 行直接进测试，>50 行先给《大范围改动合理性说明》再动手。

## 8. 验证收敛：P/Q/I → load tdd

- **P 前置**：环境/依赖/基线就绪？❌ → 阻断修复前置，不进入验证。
- **Q 后置**：功能正确、指标达标？❌ → 回退（时间流执行错误）。
- **I 不变量**：既有测试不回归、接口兼容、无泄漏/死锁/越界？❌ → 回退（规则被破坏）。
- 全 ✅ → 保留（commit / 交付）；协同 LLM 产品加 `validate_llm()`。
- **时空闭环三问**：所有 init 都有对应 destroy 吗？主循环有不可退出的死循环吗？有跨层直访内存吗？

## 9. 阶段路由速查

| 阶段 | 去处 |
|---|---|
| 读仓库 / 架构审计 | load `repo-explorer`（快速建图档 or 六步深读档） |
| 跑测试 / 改后验证 | load `tdd`（run_tests） |
| 模板与完整公理 | read_file `references/project-development-prompt.md` |
