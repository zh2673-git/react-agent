# -*- coding: utf-8 -*-
"""run_tests —— tdd 技能配套工具：固定探测并运行工作区测试套件。

协议（tools 插件命令通道）：stdin 收 {"args":{...}}，stdout 回
{"ok":true,"result":...} | {"ok":false,"error":{"code","message"}}。

安全约束：
- 只运行固定探测出的测试命令（cargo test / python -m pytest / npm test），
  不接受任意命令；extra 仅作为测试框架的附加 argv 拼接（list 形式不经 shell，无注入面）；
- 测试固定在 WORKSPACE_ROOT 运行（未设置时报错拒绝，不回落任意目录）。

判定：工具执行成功即 ok:true；测试是否通过看 result.passed（exit_code == 0）。
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

TAIL = 8000  # stdout/stderr 尾部保留字符数（报错信息多在尾部）


def _resp(payload: dict) -> None:
    sys.stdout.write(json.dumps(payload, ensure_ascii=False))
    sys.stdout.flush()


def _tail(s: str) -> str:
    return s[-TAIL:] if len(s) > TAIL else s


def _detect(ws: Path, profile: str | None) -> tuple[list[str], str] | None:
    """按 cargo > pytest > npm 探测；profile 指定时只看对应清单。返回 (cmd, profile)。"""
    want = [profile] if profile else ["cargo", "pytest", "npm"]
    for p in want:
        if p == "cargo" and (ws / "Cargo.toml").is_file():
            return ["cargo", "test"], p
        if p == "pytest" and (
            (ws / "pytest.ini").is_file()
            or (ws / "pyproject.toml").is_file()
            or (ws / "setup.py").is_file()
            or (ws / "setup.cfg").is_file()
            or (ws / "tests").is_dir()
        ):
            return [sys.executable, "-m", "pytest"], p
        if p == "npm" and (ws / "package.json").is_file():
            pkg = (ws / "package.json").read_text(encoding="utf-8", errors="replace")
            if '"test"' in pkg:
                return ["npm.cmd" if os.name == "nt" else "npm", "test"], p
    return None


def main() -> int:
    try:
        req = json.loads(sys.stdin.read() or "{}")
        args = req.get("args") or {}
        if not isinstance(args, dict):
            return _resp({"ok": False, "error": {"code": "BAD_ARGS", "message": "args 必须是对象"}})

        ws_raw = os.environ.get("WORKSPACE_ROOT", "").strip()
        if not ws_raw:
            return _resp({"ok": False, "error": {"code": "NO_WORKSPACE", "message": "WORKSPACE_ROOT 未设置，拒绝运行"}})
        ws = Path(ws_raw).resolve()

        profile = args.get("profile")
        if profile is not None and profile not in ("cargo", "pytest", "npm"):
            return _resp({"ok": False, "error": {"code": "BAD_PROFILE", "message": f"profile 只支持 cargo/pytest/npm，收到: {profile!r}"}})

        extra = args.get("extra") or []
        if not isinstance(extra, list) or not all(isinstance(x, str) for x in extra):
            return _resp({"ok": False, "error": {"code": "BAD_EXTRA", "message": "extra 必须是字符串数组"}})
        if len(extra) > 16 or any(len(x) > 200 for x in extra):
            return _resp({"ok": False, "error": {"code": "BAD_EXTRA", "message": "extra 最多 16 项且每项 ≤200 字符"}})

        found = _detect(ws, profile)
        if found is None:
            return _resp({
                "ok": False,
                "error": {
                    "code": "NO_PROFILE",
                    "message": "未探测到测试命令（需 Cargo.toml / pytest 清单或 tests 目录 / package.json 带 test 脚本）",
                },
            })
        cmd, prof = found
        cmd = cmd + [x for x in extra if x.strip()]

        t0 = time.perf_counter()
        proc = subprocess.run(
            cmd, cwd=str(ws), capture_output=True, text=True, encoding="utf-8", errors="replace",
        )
        dt = time.perf_counter() - t0
        return _resp({
            "ok": True,
            "result": {
                "profile": prof,
                "cmd": cmd,
                "exit_code": proc.returncode,
                "passed": proc.returncode == 0,
                "duration_secs": round(dt, 1),
                "stdout_tail": _tail(proc.stdout or ""),
                "stderr_tail": _tail(proc.stderr or ""),
            },
        })
    except Exception as exc:  # noqa: BLE001 - 任何异常都按协议回错误而非崩溃
        return _resp({"ok": False, "error": {"code": "TOOL_ERROR", "message": f"{type(exc).__name__}: {exc}"}})


if __name__ == "__main__":
    sys.exit(main())
