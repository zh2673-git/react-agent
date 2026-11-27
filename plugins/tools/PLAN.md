# tools 插件改造计划（P6 轮）

> 范围：`plugins/tools/tools/*.py`（内置 9 件工具实现）。本轮只动 **bash 的文件追溯遍历**
> 与职责边界**决策记录**；不改工具清单、不动 `_CORE_BLOCKLIST` 语义、不动沙箱路径。

## 背景（为什么改）

### B1 bash 每次调用要遍历两遍全工作区，单次 ~22 秒

`bash.py` 的 W16 文件追溯（`BASH_WRITE_TRACE`，默认开）在每次命令执行**前后各扫一次**
快照区：

- 执行前 `_scan_zone(with_content=True)` —— stat 清单 + 内容 pre-copy；
- 执行后 `_diff_zone()` → 内部 `_scan_zone(with_content=False)` —— 再来一次 stat 清单。

原实现是 `os.walk` 全树 + **逐文件** `_in_snapshot_zone(rel, excluded)`。本机实测（快照区内
23496 个文件）：

| 阶段 | 改造前 | 改造后 | 提速 |
|---|---|---|---|
| `_scan_zone(with_content=True)` | 11016.7 ms | 503.3 ms | **21.9×** |
| `_scan_zone(with_content=False)` | 11264.1 ms | 460.1 ms | **24.5×** |

即每次 bash 调用从 ~22.3s 降到 ~1.0s——**每次工具调用都付这个成本**，是全仓最贵的一次
遍历。三处浪费：

1. **排除目录走了再丢**：`outputs/`、`.stream`、`MEMORY_DATA_DIR`（默认 `plugins/memory/data`，
   实测 21308 个 undo 快照文件）、各噪音目录，原实现全部下钻遍历后逐文件 `continue` 丢弃。
   实测因此白扫 ~21500 个文件（44963 → 剪枝后 23496）。
2. **`workspace_root()` 每文件调一次**：它内部是 `Path(...).resolve()`（有 syscall），
   实测 0.14ms/次 × 23496 ≈ 3.3s/次扫描。
3. **`os.stat()` 每文件一次额外 syscall**：scandir 的 `DirEntry.stat()` 在 Windows 上直接来自
   `FindFirstFile`，不需要再来一次。

### B2 超 undo 上限的文件做的是纯浪费的 pre-copy

`_snapshot_change()` 对 `> _UNDO_MAX_BYTES`（2MB）的文件**直接返回 None**（永不落 undo），
但旧的 `_scan_zone` 仍然把它的全部字节读进内存当 before —— 读了也永远用不上。

## 本次改动（已落地）

### P6-1 `_scan_zone` 改 `os.scandir` 显式栈 + 遍历期剪枝

补丁：`scripts/patches/p6-bash-scan-perf-1.json`（order 176，`applied_marker = "    stack = [ws]"`）。

- **下钻前剪枝**：排除目录（由 `_excluded_roots()` 给出，含噪音目录 / `.stream` / `outputs` /
  `MEMORY_DATA_DIR`）在 `scandir` 阶段就判断、命中即不下钻；点目录与 `_NOISE_DIRS` 同样不下钻。
- **`workspace_root()` 提到扫描级**：每次扫描算一次 `ws`；`_in_snapshot_zone()` 改为接受
  绝对路径（相对路径仍兼容，会自行拼接）——签名从 `(rel, excluded)` 变为
  `(path, excluded)`，语义不变。
- **`DirEntry.stat()`** 取代 `os.stat(join(...))`，省一次 syscall。
- **`rel` 仍用 `os.path.relpath`**（与原实现同函数同结果），清单键形态不变。
- **超 `_UNDO_MAX_BYTES` 的文件不进内容缓存**：清单仍记 `(mtime_ns, size)`（变更仍可见），
  只是没有 before 字节 → 该条变更无 undo 引用（与「超 2MB 本来就无 undo」的既有语义一致）。
- **目录符号链接不下钻**：对齐原 `os.walk` 的 `followlinks=False`，防环。

### 等价性验证（不是「看起来一样」）

- 改造前先冻结基线：`_scan_zone` 的完整 stat 清单（23496 条 `rel -> (mtime,size)`）dump 到
  `.scratch/p6_baseline_before.json`。
- 改造后同样 dump 并逐键比对：`manifest_equal=True`、`extra=0 missing=0`、
  `stat_listing_equal=True`、`content_cache_active` 不变。
- 端到端行为（隔离小工作区，内容缓存处于有效状态）：**create → undo=True**、
  **modify → undo=True**、**delete → undo=True**；`>2MB` 文件 → 在清单中、不在内容缓存中、
  缓存整体仍有效。
- 真实 bash 工具路径下也留痕成功（工具返回 `changes:[{"kind":"create", ...}]`）。
- 顺带确认剪枝语义正确：`.scratch/`（点目录）内的文件按设计**不在**快照区——最初 e2e 用例
  放在 `.scratch/` 下被误判为「未检测到」，换到非点目录后三类变更全部命中。

### P6-2 职责边界：bash 能写文件，代码层不做闸（决策记录）

现状与判断：

- 描述层已大量劝阻（`_description()` 明写「do NOT use it for file operations … use the
  dedicated tools」），但**代码层不阻止** bash 写文件。
- 这是**有意的**：bash 的职责是「跑构建/测试/git/进程端口查询」，而构建与测试本身就会产生
  文件；把 bash 关成只读会让 `cargo test`、`npm install` 这类正常调用失败。
- 真实防线不在「禁止 bash 写」，而在**可追溯 + 可回滚**：bash 的写入以 `changes[]` 返回、
  由 agent-loop 转发成 `op=bash` 的 `file_change` 事件，与 write/edit 同一条 undo/回滚链路。
  核心区（`_CORE_BLOCKLIST`）的间接写同样留痕、可撤销。
- **结论：本轮不加写闸。** 若将来要收紧，正确做法是「对核心区路径的 bash 写做 deny」
  （复用 `_is_core_path` + `allow_core_write`），而不是全面禁止 bash 写——全面禁写会与
  「bash 跑构建写 target/」冲突，且 `runtime/` 整树本身就是核心区。

## 回退决策（保留项）

- 不改 `_excluded_roots()` 的构成（排除哪些目录是语义，不是性能问题）。
- 不改 `_diff_zone` 的变更判定逻辑（`mtime+size` 变化即视为 modify；同 mtime+size 的内容
  替换本就检测不到——诚实的既有边界）。
- 不改 `_CHANGES_MAX=50` / `_SNAP_MAX_BYTES=32MB` / `_SNAP_MAX_FILES=4000` 三个上限。
- 不把 `_scan_zone` 改成「只扫 git 跟踪区」——bash 写的新文件可能不在 git 里，会漏账。

## 验证记录（本轮）

- `cargo test --workspace`（经 `.scratch/run_clean.py`，剥 16 个泄漏变量）：**19 个 suite 全绿**，
  0 FAILED / 0 panicked / 0 `error[`。分布：agent-loop lib 83、conformance_check 1、
  contract_conformance 2、react_mocks 50、host lib 99、main 0、sandbox-run 1、
  e2e_full_react_mock 2、e2e_live_llm 1、e2e_llm_mock 1、e2e_mcp_tools 1、
  e2e_python_tools 4、e2e_react_compaction 1、e2e_subagent 1、e2e_system_prompt 1、
  e2e_ts_memory 10、e2e_web_gateway 2、doc-tests 0×2。
- `python -m pytest tests -q`：**428 passed**（改动前后同值，无回归）。
- `python scripts/apply-tool-patches.py --dry-run`：`FAIL` 0 行；`p6-bash-scan-perf-1`
  二次执行为 `OK 已应用过（applied_marker 命中）`。
