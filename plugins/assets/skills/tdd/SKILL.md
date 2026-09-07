---
name: tdd
description: Test-driven verification discipline - the Q executor of the P/Q/I acceptance loop. Run the workspace test suite with the run_tests companion tool after every substantive edit; baseline first, small steps, red means not done. Also guards invariants (no regression, init/destroy pairing, no unbounded loops). Triggers: 改完验证/补测试/跑测试. Orchestrated by project-dev (验证阶段).
tools: tools.json
origin: preset
---

# TDD（改完必须验证）

本技能把「改完必须跑测试」固化为配套工具 `run_tests`：固定探测 cargo test / pytest / npm test，
不接受任意命令。装载后（skill_install + 设置面板启用）即可在会话内调用。
定位：方法论 **P/Q/I 验证环的执行器**（project-dev 验证阶段路由到这里）。

## 1. P/Q/I 视角（保留 / 回退判定）

- **P 前置**：先测基线——动手前跑一次 `run_tests` 确认基线绿；基线红先记录失败清单
  （显式区分既有失败，防止归因误判）。基线红且属环境问题 → **阻断**，修好前置再动工。
- **Q 后置**：每完成一个逻辑完整的修改跑一次（小步，不攒批）；功能不符合预期 → **回退**修复。
- **I 不变量**：既有测试不回归（接口兼容被破坏 = 规则被破坏，**回退**）；附加时空闭环三问——
  新代码 init/destroy（或构造/析构）成对吗？新增循环/后台任务有退出路径吗？有跨层直访吗？
- **决策**：P❌ 阻断；Q❌ 回退；I❌ 回退；三者全过才允许宣布完成 / 保留改动。

## 2. 工作流

1. **先测基线**（= 验 P）：基线本来就红时先记录失败清单，避免把既有失败误判为自己引入。
2. **小步修改**（= 验 Q）：每完成一个逻辑完整的修改就跑一次 `run_tests`，不攒批。
3. **红了先修**：读输出尾部的失败明细，修复后重跑；禁止在测试红的状态下宣布完成。
4. **新行为配新测试**：给修复或新功能补最小测试用例（跟随所在仓库的测试组织），再跑一遍确认。

## 3. 工具约束

- `run_tests` 只运行固定探测到的测试命令（`profile` 可指定 cargo/pytest/npm，`extra` 只能附加
  测试框架参数），**不能执行任意命令**——需要编译检查、单文件脚本等场景用 `bash` 工具。
- 长测试套件超时上限 900s（工具声明 timeout_secs）；输出只回尾部，超长输出自行用 `grep`
  在日志文件里定位。

## 4. 汇报约定

最终回答注明验证结论：跑了什么命令、P/Q/I 各自状态（基线既有失败显式区分）、是否全绿；
未全绿不得使用「完成」表述。
