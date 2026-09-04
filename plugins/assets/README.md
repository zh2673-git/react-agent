# assets（Python guest，软依赖）

按需加载的知识资产注册表：**skills**（技能）与 **prompts**（提示词模板）。
assets **从不执行** skill 代码——执行靠 tools 插件的基础工具，因此与语言无关。

渐进式披露（06）：

- **Discovery**：`skills.list` 只出 name + description（约 100 token/个，随系统提示词注入）
- **Activation**：`skills.load` 出全文（模型经 `agent-loop` 的保留名 `load_skill` 调用）
- **Execution**：复用 tools 插件的基础工具

## 文件结构

| 路径 | 职责 |
|---|---|
| `assets_plugin.py` | 全部实现（扫描 + 4 个 op） |
| `skills/<name>/SKILL.md` | 技能：目录内含 `SKILL.md`，frontmatter 需 `name` + `description` |
| `prompts/<name>.md` | 提示词模板：首行 `# name`，次个非空行为描述 |

## op 契约（线契约 03 §2.4）

```jsonc
{"op":"skills.list"}        → {"ok":true,"skills":[{name,description}],"root":str}
{"op":"skills.load","name"} → {"ok":true,"content":str}
{"op":"prompts.list"}       → {"ok":true,"prompts":[{name,description}]}
{"op":"prompts.get","name"} → {"ok":true,"content":str}
```

错误码：`ASSET_ERROR`（读取失败）、`UNKNOWN_SKILL` / `UNKNOWN_PROMPT`（未知名，错误信息里附可用列表）、
`K400`（未知 op）。

## 扫描策略

- **每次调用都重扫目录**：Web 技能 CRUD 写盘后、模型自扩展创建新技能后，
  下一次 `list` / `load` 立即可见（文件即注册表，没有缓存要失效）。
- 不合规资产**跳过并 warn 到 stderr，不影响其他资产**：
  - skill：目录里没有 `SKILL.md`，或 frontmatter 缺 `name` / `description`。
  - prompt：首行不是 `# name`。
- frontmatter 是手写最小解析（`---` 围栏内的 `key: value`），**不依赖 PyYAML**。
- 单次读取上限 256 KB（`_MAX_LOAD_BYTES`）；扫描阶段只读前 64 KB 解析元信息。

## 环境变量

| 变量 | 作用 |
|---|---|
| `SKILLS_DIR` | skills 根目录（缺省 `plugins/assets/skills`） |
| `PROMPTS_DIR` | prompts 目录（缺省 `plugins/assets/prompts`） |
| `WORKSPACE_ROOT` | 宿主下发，用于 `root ⊆ WORKSPACE_ROOT` 的可达性探测 |

`skills.list` 返回 `root`（绝对路径）：`agent-loop` 据此判断模型能否用 `write_file` 自创技能
——只有 root 在工作区范围内时，才在系统提示词里追加"技能自扩展"授权段。

## 改动时的联动点

- 宿主 Web 技能 CRUD 的写入目录必须与 `SKILLS_DIR` 同源：`crates/host/src/config.rs::skills_dir()`。
- 前端技能编辑走 `/api/skills/*`（`crates/host/src/frontend.rs`），保存后靠本插件"每次重扫"生效，**无需重启**。
- `load_skill` 是 `agent-loop` 的保留名，不进 tools 分发
  （`crates/agent-loop/src/lib.rs::RESERVED_LOAD_SKILL`）。
- 本插件是**软依赖**：spawn 或探测失败只 warn，不阻断启动；此时无技能附录、无具名提示词模板。
