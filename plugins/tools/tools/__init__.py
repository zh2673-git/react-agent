"""tools 内部包：按运行时关注点分文件（05 §6）。

每个模块暴露 `TOOLS: dict[name, spec]`；spec = {"description", "parameters", "run"}。
`ToolError` 是唯一的受控错误通道：携带 code / field，由 tools_plugin 捕获转为字段级错误回喂。
"""

import os
from pathlib import Path


class ToolError(Exception):
    """字段级工具错误：message 必须含「下一步怎么改」。"""

    def __init__(self, message: str, code: str = "TOOL_ERROR", field: str | None = None):
        super().__init__(message)
        self.code = code
        self.field = field


def workspace_root() -> Path:
    return Path(os.environ.get("WORKSPACE_ROOT") or os.getcwd()).resolve()


def require(args: dict, key: str) -> str:
    v = args.get(key)
    if not isinstance(v, str) or not v.strip():
        raise ToolError(f"缺少必填参数 '{key}'（string，且不能为空）", code="MISSING_ARG", field=key)
    return v


def optional_int(args: dict, key: str, default: int, lo: int, hi: int) -> int:
    v = args.get(key, default)
    try:
        v = int(v)
    except (TypeError, ValueError):
        raise ToolError(f"参数 '{key}' 必须是整数，收到: {v!r}", code="BAD_ARG", field=key) from None
    if not lo <= v <= hi:
        raise ToolError(f"参数 '{key}' 超出合法范围 [{lo}, {hi}]，收到: {v}", code="BAD_ARG", field=key)
    return v
