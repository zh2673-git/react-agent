# 06 assets 四层设计（skills + prompts）

> 锚定父本质：「在内核插件契约的时空约束下，本模块的本质是**按需加载的知识资产注册表**——渐进式披露让领域知识以最小 token 成本进入上下文。」

## 1. 四层结构

| 层 | 设计 |
|---|---|
| 数据规范 | skill = Agent Skills 开放标准最小集：目录含 `SKILL.md`（YAML frontmatter 仅必需 `name`/`description` 两字段，手写解析不引 PyYAML）；prompt = markdown 文件（首行 `# name` + 描述段） |
| 数据存储 | 文件系统（`SKILLS_DIR` / `PROMPTS_DIR`）；frontmatter 解析结果进程内缓存（init 建立一次） |
| 数据流转 | 扫描目录 → 解析缓存 → `list` 只出 name+description（Discovery，~100 token/个）→ `load` 出全文（Activation，<5000 token）→ **Execution 复用基础工具**（模型用 read_file/bash 读 references/、跑 scripts/，零新增机制） |
| 数据接口 | `assets.registry`：`skills.list` / `skills.load` / `prompts.list` / `prompts.get`（见 03 §2.4） |

## 2. 渐进式披露三级映射（开放标准 × 本项目）

| 标准阶段 | 落点 | Token 成本 | 失败行为 |
|---|---|---|---|
| Discovery | agent-loop chat 开始调 `skills.list`，注入系统提示词「可用技能」附录 | ~100/个 | assets 不可用 → 附录省略，行为与现状一致（软依赖降级，I） |
| Activation | 模型调保留工具 `load_skill(name)` → agent-loop 路由 `skills.load` | 按需 | 未知 name → 字段级错误（列出可用 name），不中断循环 |
| Execution | SKILL.md 正文指引模型用基础工具读 `references/`、跑 `scripts/` | 按需 | 复用 05 的越界拦截与超时规则 |

**Skill 语言无关性**：assets 只读 SKILL.md（注册表），从不执行 skill 代码；`scripts/` 由模型经 bash 工具执行，宿主机有什么运行时就能用什么语言（Rust/Go/Node 均可，编译型脚本、预编译二进制亦可，但需在 description 注明 OS/arch）。**前置声明规则**：skill 所需运行时必须写入 description（Discovery 阶段即进入模型上下文，模型可先 bash 探测再执行）；Phase 2+ 可选进阶——frontmatter 增加可选 `requires:` 字段，由 `skills.list` 一并返回。

## 3. prompts 覆盖链（提示词的可定制空间）

优先级从高到低（高者存在即短路）：

| 级别 | 来源 | 说明 |
|---|---|---|
| 1 | `AGENT_SYSTEM_PROMPT`（env） | 调试/临时覆盖 |
| 2 | `SYSTEM.md`（工作区根） | pi 同名机制对位 |
| 3 | `prompts/` 具名模板（`PROMPT=name`） | 可复用模板库，`prompts.list` 可枚举 |
| 4 | agent-loop 内置缺省 | 永远兜底，<1000 token（pi 预算） |

## 4. 生命周期钩子

| 钩子 | 行为 |
|---|---|
| `init` | mkdir 缺省目录；扫描 skills/prompts 建 name→路径缓存；frontmatter 不合规目录跳过并在 list 结果外记 warn |
| `start`/`stop` | 无 |
| `destroy` | 清缓存 |

## 5. 与内核约束的关键化解

Process 域 guest 无法互调 → skill 激活不能做成 tools 普通工具（tools 调不到 assets）。化解：`load_skill` 为 agent-loop **保留名**，由唯一 InProcess 编排者路由——路由规则一条分支，无越界（见 03 §3）。

## 6. 验证点（Q）

- 放入一个示例 skill（含 references/、scripts/）→ catalog 出现在系统提示词 → `load_skill` 返回全文 → 模型用 read_file 读 references 走通全链路；
- 空 skills 目录：行为与现状一致（I）；
- frontmatter 缺 description 的目录被跳过且不影响其他 skill。
