"""todo 工具：任务清单外化（2026-09-21 解冻新增，第 10 件内置工具，PLAN §十一 v5）。

动因：多步任务的计划只存于模型上下文，历史压缩（COMPACT_TRIGGER）后丢失 → 重新探索；
todo 把计划写进磁盘（MEMORY_DATA_DIR/todo.json，缺省回退 .stream/），跨压缩跨会话可恢复。

语义（对齐主流 harness 的 todowrite）：**全量替换式写入**——每次提交完整清单（含未变
项），省略 todos 参数 = 只读返回当前清单。状态机：pending → in_progress → completed |
cancelled（不做状态迁移校验，模型自律）。

纪律：纯状态外化，无文件系统面（落盘点固定在 MEMORY_DATA_DIR，不经 _guard）；单 guest
进程内模块级锁保护并发；落盘失败降级为内存态（会话内仍可用）并如实标注。
"""

import json
import os
import threading
from pathlib import Path

from . import ToolError, workspace_root

_STATUSES = ("pending", "in_progress", "completed", "cancelled")
_MAX_TODOS = 50
_LOCK = threading.Lock()
_STATE: list[dict] = []


def _store_path() -> Path:
    base = os.environ.get("MEMORY_DATA_DIR", "").strip()
    d = Path(base) if base else Path(workspace_root()) / ".stream"
    return d / "todo.json"


def _load() -> list[dict]:
    """惰性加载：内存态优先；落盘仅首次回读（之后内存为准）。"""
    if _STATE:
        return _STATE
    try:
        data = json.loads(_store_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    if isinstance(data, list):
        _STATE.extend(
            t for t in data
            if isinstance(t, dict) and all(k in t for k in ("id", "content", "status"))
        )
    return _STATE


def _persist() -> None:
    p = _store_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.parent / (p.name + ".tmp")
    tmp.write_text(json.dumps(_STATE, ensure_ascii=False, indent=1), encoding="utf-8")
    os.replace(tmp, p)  # 原子替换，防半截 JSON


def _todo(args: dict) -> str:
    with _LOCK:
        todos = args.get("todos")
        note = None
        if todos is not None:
            if not isinstance(todos, list):
                raise ToolError(
                    "参数 'todos' 必须是数组（每项 {id, content, status}）；省略该参数则只读返回当前清单",
                    code="BAD_ARG", field="todos",
                )
            if len(todos) > _MAX_TODOS:
                raise ToolError(
                    f"todos 超过上限 {_MAX_TODOS} 条，请合并细项或清理已完成项",
                    code="BAD_ARG", field="todos",
                )
            cleaned: list[dict] = []
            seen: set[str] = set()
            for i, t in enumerate(todos):
                if not isinstance(t, dict):
                    raise ToolError(f"todos[{i}] 必须是对象 {{id, content, status}}", code="BAD_ARG", field="todos")
                tid = t.get("id")
                content = t.get("content")
                status = t.get("status", "pending")
                if not isinstance(tid, str) or not tid.strip():
                    raise ToolError(f"todos[{i}].id 必须是非空字符串（如 '1'）", code="BAD_ARG", field="todos")
                if tid in seen:
                    raise ToolError(f"todos[{i}].id 重复: '{tid}'（id 必须唯一）", code="BAD_ARG", field="todos")
                if not isinstance(content, str) or not content.strip():
                    raise ToolError(f"todos[{i}].content 必须是非空字符串（一句话描述该项任务）", code="BAD_ARG", field="todos")
                if status not in _STATUSES:
                    raise ToolError(
                        f"todos[{i}].status 必须是 {list(_STATUSES)} 之一，收到: {status!r}",
                        code="BAD_ARG", field="todos",
                    )
                seen.add(tid)
                cleaned.append({"id": tid.strip(), "content": content.strip(), "status": status})
            _STATE.clear()
            _STATE.extend(cleaned)
            try:
                _persist()
            except OSError as exc:
                note = f"警告：落盘失败（{exc}），清单仅本会话内存有效"
        current = [dict(t) for t in _load()]
    counts = {}
    for s in _STATUSES:
        n = sum(1 for t in current if t["status"] == s)
        if n:
            counts[s] = n
    out = {"todos": current, "total": len(current), "counts": counts}
    if note:
        out["note"] = note
    return json.dumps(out, ensure_ascii=False)


TOOLS = {
    "todo": {
        "description": (
            "Maintain an external todo list for multi-step tasks. Write the FULL list each call "
            "(unchanged items included, exactly one item 'in_progress' while working; mark items "
            "'completed' immediately after finishing) — the list survives context compaction and "
            "session restarts. Omit 'todos' to read the current list. "
            "Use it whenever a task has 3+ steps or the user provides multiple tasks; do NOT use "
            "it for single trivial actions. "
            "Args: todos (optional array of {id: str, content: str, status: 'pending'|'in_progress'|"
            "'completed'|'cancelled'})."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "full replacement list; omit to read current list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "unique id, e.g. '1'"},
                            "content": {"type": "string", "description": "one-line task description"},
                            "status": {"type": "string", "enum": list(_STATUSES)},
                        },
                        "required": ["id", "content"],
                    },
                },
            },
        },
        "run": _todo,
    },
}
