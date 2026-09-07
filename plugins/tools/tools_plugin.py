"""tools guest 插件（Python，Process 域 gRPC）——瘦分派层 + scope 装配。

线契约（03 §2.3）：
  {"op":"list"} → {"ok":true,"tools":[{"name","description","parameters"}]}
  {"op":"list","all":true} → 额外含未启用工具，各项附 "enabled": bool（配置中心视图，08）
  {"op":"call","name":str,"args":object} → {"ok":true,"result":any} | {"ok":false,"error":{"code","message","field"?}}

扩展 op（08，Phase 4）：
  {"op":"configure","enabled":[str,...]}       → 运行时整体替换白名单（未知名 → 字段级 400）
  {"op":"reload"}                              → 动态装载 tools/ 目录下的工具模块（L2 自扩展）。
                                                 **装载 ≠ 启用**：新工具进可用池但不进白名单，
                                                 需显式 configure 才出现在 list（写与启用两步分离）。
                                                 单模块 import 失败 → 跳过并回 skipped（fail-closed，
                                                 该模块旧工具保留，内置工具永不覆盖）。

技能工具（R9，语言无关命令通道）：
  {"op":"install","path":str,"skill"?} → {"ok":true,"skill","loaded","skipped","pending"}
                                                 定点装载技能包内 tools.json（数组，每项
                                                 ToolSpec + exec.cmd）；装载进技能工具池，
                                                 不可调用、不进 list、不启用（三层作用域之①）。
                                                 校验 fail-closed：路径 ⊆ WORKSPACE_ROOT 且 ⊆
                                                 skills root；单项失败跳过不阻断。
  {"op":"skill_tools","skills":[str]?,"all"?} → {"ok":true,"tools":[ToolSpec + skill + enabled?]}
                                                 会话清单装配视图（agent-loop）与配置视图（host）；
                                                 缺省只出**已启用**工具（会话清单），
                                                 all=true 附未启用项（含 enabled 标记）。
                                                 不在 list 契约内——技能工具对内置工具 tab 不可见。

命令执行协议（语言无关）：技能工具被 call 时起**子进程**执行 exec.cmd——
  stdin 收 {"args":{...}}；stdout 回 {"ok":true,"result":...} | {"ok":false,"error":{...}}
  （与 Wire 契约同形，任何语言 JSON 序列化即可实现工具）；cwd 缺省技能目录；
  超时：exec.timeout_secs（可选正数）按工具覆盖全局 SKILL_TOOL_TIMEOUT_SECS（缺省 60s），
  超时杀进程；执行体与插件进程隔离。

MCP 工具（§八，stdio 传输第三池）：MCP_SERVERS env JSON {"name":{"command":[...],"cwd"?,"env"?}}
  → 启动时逐台连接（fail-closed per server：握手/枚举失败跳过不阻断）。命名空间
  `mcp__{server}__{tool}`（非法字符 sanitize，重名/超限跳过）；inputSchema 即 parameters 透传。
  {"op":"mcp_tools"} → {"ok":true,"servers":[{name,status,tools,error?}],"tools":[{name,description,server,enabled}]}
                                                 配置视图（host 工具 tab MCP 区块数据源）。
  装载 ≠ 启用：MCP 工具进池不进白名单，经 configure/设置面板显式启用后常驻全会话可见
  （不绑技能，无 load_skill 前置）——list 对外契约含**已启用** MCP 工具，清单注入
  agent-loop 零改动。调用 = JSON-RPC tools/call，content[] 文本拼接回 {"ok":true,"result"}；
  isError=true / 远端错误 / 超时 / 崩溃 → 字段级错误（崩溃按 5s 退避窗口重启）。
  超时复用 SKILL_TOOL_TIMEOUT_SECS。server 随插件 destroy 逆序回收。

工具实现位于 tools/ 包（files / bash / web / grep / symbols / mcp，按运行时关注点分文件）；
本文件只做：init 装配 scope → list 过滤 → call 校验分发 → ToolError 转字段级错误。

scope：TOOLS_ENABLED=read_file,bash（白名单；未列出的工具 Schema 与实现双不可见）。
保留名 load_skill/skill_install 不在本注册表（归 agent-loop 路由，见 03 §3 / R9）。
"""

import importlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

from agent_kernel.guest import serve

from tools import ToolError
from tools import bash as bash_mod
from tools import files as files_mod
from tools import grep as grep_mod
from tools import mcp as mcp_mod
from tools import web as web_mod

# v4 第 9 件（symbols_search）：依赖 tree-sitter-language-pack（pin <1.0）。依赖缺失时
# 跳过装载（fail-closed）——插件与其余 8 件正常，仅 symbols_search 不注册。
try:
    from tools import symbols as symbols_mod
except ImportError as _exc:  # pragma: no cover - 缺依赖环境
    symbols_mod = None
    print(f"[tools] symbols_search 未装载（缺依赖）: {_exc}", file=sys.stderr)

_ALL: dict = {**files_mod.TOOLS, **bash_mod.TOOLS, **web_mod.TOOLS, **grep_mod.TOOLS}
if symbols_mod is not None:
    _ALL = {**_ALL, **symbols_mod.TOOLS}
ALL_TOOLS: dict = _ALL
_ENABLED: set[str] = set()
# L2 动态装载：按模块分表保留（某模块 reload 失败时其旧工具原样保留——fail-closed）
_EXTRA_BY_MODULE: dict[str, dict] = {}

# §八 MCP 第三池：name(全限定 mcp__server__tool) → {name,description,parameters,server,remote}。
# 装载 ≠ 启用：进池不进白名单；启用后常驻全会话可见（不绑技能）。失败 server 记 _MCP_FAILED。
_MCP_TOOLS: dict = {}
_MCP_SERVERS: dict = {}
_MCP_FAILED: dict = {}

# R9 技能工具池（命令通道）：name → {"name","description","parameters","exec","skill","dir"}
# 装载 ≠ 启用 ≠ 可见（三层作用域）：进池不可调用、不进 list；启用走 configure/config.json；
# 会话可见性由 agent-loop 按 load_skill 组装（skill_tools op）。
_SKILL_TOOLS: dict = {}
# 延迟启用（R9）：config.json tools.enabled 持久化的技能工具名先于装载到达（重启场景），
# 内置启动校验不再对非内置名 SystemExit，改为挂起——install 同名工具时自动转入启用集。
_DEFERRED_ENABLED: set[str] = set()

# reload 时跳过的内置模块名（重入 sys.modules 清理无意义，且防止测试/误删内置实现；
# mcp 是传输模块无 TOOLS dict，列入防 reload 误扫）
_BUILTIN_MODULES = frozenset({"files", "bash", "web", "grep", "symbols", "mcp"})
# 动态工具条目必须暴露的键（与内置 ToolSpec 三元组同规范：dict 键即 name，不要求冗余键）
_REQUIRED_KEYS = ("description", "parameters", "run")

# 技能工具名约束：字母开头，字母数字/_/-，≤64 字符（与技能名约束同风格）
_SKILL_TOOL_NAME_RE = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,63}")


def _parse_enabled() -> set[str]:
    raw = (os.environ.get("TOOLS_ENABLED") or "").strip()
    if not raw:
        return set(ALL_TOOLS)  # 缺省白名单 = 内置全集；MCP/技能工具不自动启用（人工闸）
    names = {n.strip() for n in raw.split(",") if n.strip()}
    known = set(ALL_TOOLS) | set(_MCP_TOOLS)
    unknown = names - known
    if unknown:
        # R9：非内置名不再启动失败——技能工具经 install 装载后才存在，持久化启用授权
        # （config.json tools.enabled）先于装载到达 → 延迟到 install 同名工具时生效。
        _DEFERRED_ENABLED.update(unknown)
        print(f"[tools] TOOLS_ENABLED 含未装载工具（延迟启用，待 install）: {sorted(unknown)}", file=sys.stderr)
    return names & known


def _pool() -> dict:
    """可用池 = 内置 + 动态装载（动态未启用不出 list，见「装载 ≠ 启用」）。
    技能工具不在其中（独立池，list 契约不变）。"""
    return {**ALL_TOOLS, **_extra_pool()}


def _extra_pool() -> dict:
    merged: dict = {}
    for mod_tools in _EXTRA_BY_MODULE.values():
        merged.update(mod_tools)
    return merged


def _within(child: Path, parent: Path) -> bool:
    """realpath 前缀包含（Windows 大小写不敏感）。子 == 父不算 within（必须更深一层）。"""
    c = os.path.normcase(str(child))
    p = os.path.normcase(str(parent))
    return c != p and c.startswith(p.rstrip("\\/") + os.sep)


def _skills_root() -> Path:
    """skills 根目录（与 assets guest 同源：SKILLS_DIR env 或缺省 plugins/assets/skills）。"""
    env = os.environ.get("SKILLS_DIR")
    if env:
        return Path(env).resolve()
    return Path(__file__).resolve().parent.parent / "assets" / "skills"


def _skill_tool_timeout() -> float:
    try:
        v = float(os.environ.get("SKILL_TOOL_TIMEOUT_SECS", "60"))
    except ValueError:
        return 60.0
    return v if v > 0 else 60.0


# ── §八：MCP 第三池装配（stdio 传输，协议见 tools/mcp.py） ──────────────────────

def _mcp_sanitize(s: str) -> str:
    """命名空间片段净化：非法字符 → '_'（全限定名再过 _SKILL_TOOL_NAME_RE 终检）。"""
    return re.sub(r"[^A-Za-z0-9_-]", "_", s)


def _register_mcp_tools(server_name: str, items: list) -> None:
    """server 枚举结果 → 全限定名入池。非法/重名/超限跳过（单工具失败不阻断）。"""
    sname = _mcp_sanitize(server_name)
    for t in items:
        remote = t.get("name") if isinstance(t, dict) else None
        if not isinstance(remote, str) or not remote.strip():
            print(f"[tools] MCP server '{server_name}' 含无名工具条目，跳过", file=sys.stderr)
            continue
        fq = f"mcp__{sname}__{_mcp_sanitize(remote.strip())}"
        if not _SKILL_TOOL_NAME_RE.fullmatch(fq):
            print(f"[tools] MCP 工具全限定名超限/非法，跳过: {fq}", file=sys.stderr)
            continue
        if fq in _MCP_TOOLS or fq in ALL_TOOLS:
            print(f"[tools] MCP 工具名冲突，跳过: {fq}", file=sys.stderr)
            continue
        params = t.get("inputSchema") if isinstance(t, dict) else None
        desc = t.get("description") if isinstance(t, dict) else None
        _MCP_TOOLS[fq] = {
            "name": fq,
            "description": str(desc) if desc else f"MCP tool '{remote}' (server {server_name})",
            "parameters": params if isinstance(params, dict) else {"type": "object", "properties": {}},
            "server": server_name,
            "remote": remote.strip(),
        }


def _init_mcp() -> None:
    """MCP_SERVERS env → 逐台连接 + 枚举工具入池。fail-closed per server：任一失败跳过，
    不影响其余 server 与内置池（与 reload 纪律一致）。"""
    raw = (os.environ.get("MCP_SERVERS") or "").strip()
    if not raw:
        return
    try:
        data = json.loads(raw)
    except ValueError as exc:
        print(f"[tools] MCP_SERVERS 非法 JSON，MCP 不装载: {exc}", file=sys.stderr)
        return
    if not isinstance(data, dict) or not data:
        print("[tools] MCP_SERVERS 须为非空 JSON 对象，MCP 不装载", file=sys.stderr)
        return
    for raw_name, spec in data.items():
        name = str(raw_name)
        if not isinstance(spec, dict) or not isinstance(spec.get("command"), list) or not spec["command"]:
            print(f"[tools] MCP server '{name}' 声明非法（需非空 command 数组），跳过", file=sys.stderr)
            continue
        server = mcp_mod.McpServer(name, spec)
        try:
            server.start()
            items = server.list_tools(timeout=30.0)
        except Exception as exc:  # noqa: BLE001 - 单 server 失败跳过（fail-closed）
            server.stop()
            _MCP_FAILED[name] = f"{type(exc).__name__}: {exc}"
            print(f"[tools] MCP server '{name}' 连接失败，跳过: {exc}", file=sys.stderr)
            continue
        _MCP_SERVERS[name] = server
        _register_mcp_tools(name, items)
        print(f"[tools] MCP server '{name}' 就绪（{sum(1 for e in _MCP_TOOLS.values() if e['server'] == name)} 件工具）", file=sys.stderr)


def _err(message: str, code: str = "TOOL_ERROR", field: str | None = None) -> dict:
    error: dict = {"code": code, "message": message}
    if field:
        error["field"] = field
    return {"ok": False, "error": error}


def _reload_modules() -> tuple[list[str], list[str], list[dict]]:
    """扫描 tools/ 包目录逐模块动态装载。返回 (loaded_modules, added_tools, skipped)。"""
    pkg_dir = Path(__import__("tools").__file__).resolve().parent
    loaded: list[str] = []
    added: list[str] = []
    skipped: list[dict] = []
    before = set(_pool())
    for py in sorted(pkg_dir.glob("*.py")):
        mod_name = py.stem
        if mod_name == "__init__" or mod_name in _BUILTIN_MODULES:
            continue
        try:
            sys.modules.pop(f"tools.{mod_name}", None)
            mod = importlib.import_module(f"tools.{mod_name}")
            spec = getattr(mod, "TOOLS", None)
            if not isinstance(spec, dict) or not spec:
                raise TypeError("模块未暴露非空 TOOLS dict")
            clean: dict = {}
            for name, t in spec.items():
                if not isinstance(t, dict) or any(k not in t for k in _REQUIRED_KEYS) or not callable(t["run"]):
                    raise TypeError(f"工具 '{name}' 形状不合规（需 name/description/parameters/run）")
                if name in ALL_TOOLS:
                    raise TypeError(f"工具名 '{name}' 与内置冲突（内置不可覆盖）")
                clean[name] = t
            _EXTRA_BY_MODULE[mod_name] = clean
            loaded.append(mod_name)
        except Exception as exc:  # noqa: BLE001 - 单模块失败跳过，旧表保留（fail-closed）
            skipped.append({"module": mod_name, "error": f"{type(exc).__name__}: {exc}"})
    added = sorted(set(_pool()) - before)
    return loaded, added, skipped


class ToolsPlugin:
    def manifest(self) -> dict:
        return {"id": "tools", "version": "0.5.0", "api_version": "0.1"}

    def init(self, config) -> None:
        global _ENABLED
        _init_mcp()  # §八：MCP 第三池先于启用闸解析（TOOLS_ENABLED 内的 mcp__ 名即可识别）
        _ENABLED = _parse_enabled()
        os.environ.setdefault("WORKSPACE_ROOT", os.getcwd())

    def on_event(self, envelope: dict) -> dict:
        payload = envelope.get("payload") or {}
        op = payload.get("op")
        if op == "list":
            show_all = bool(payload.get("all"))
            pool = _pool()
            tools = [
                {
                    "name": name,
                    "description": spec["description"],
                    "parameters": spec["parameters"],
                    **({"enabled": name in _ENABLED} if show_all else {}),
                }
                for name, spec in pool.items()
                if show_all or name in _ENABLED
            ]
            # §八 MCP 第三池：已启用工具常驻 list（全会话可见，agent-loop 清单注入零改动）；
            # all=true 视图附 enabled 与 mcp_server（host 配置视图按 mcp_tools op 取，勿混内置 tab）
            for name, e in _MCP_TOOLS.items():
                if show_all or name in _ENABLED:
                    tools.append(
                        {
                            "name": name,
                            "description": e["description"],
                            "parameters": e["parameters"],
                            "mcp_server": e["server"],
                            **({"enabled": name in _ENABLED} if show_all else {}),
                        }
                    )
            return {"ok": True, "tools": tools}
        if op == "call":
            return self._call(payload)
        if op == "configure":
            return self._configure(payload)
        if op == "mcp_tools":
            return self._mcp_tools_view(payload)
        if op == "reload":
            loaded, added, skipped = _reload_modules()
            return {"ok": True, "loaded": loaded, "added": added, "skipped": skipped,
                    "enabled_hint": "added 工具未启用（装载≠启用），需 configure 加入 enabled"}
        if op == "install":
            return self._install(payload)
        if op == "skill_tools":
            return self._skill_tools(payload)
        return _err(f"unknown op: {op}", code="K400")

    def _configure(self, payload: dict) -> dict:
        global _ENABLED
        names = payload.get("enabled")
        if not isinstance(names, list) or not names or not all(isinstance(n, str) and n.strip() for n in names):
            return _err("enabled 必须是非空字符串数组", code="K400", field="enabled")
        uniq = {n.strip() for n in names}
        # R9：合法值 = 内置/动态池 ∪ 技能工具池 ∪ MCP 池（启用闸对三通道一致）
        unknown = uniq - set(_pool()) - set(_SKILL_TOOLS) - set(_MCP_TOOLS)
        if unknown:
            return _err(
                f"未知工具: {sorted(unknown)}（合法值: {sorted(_pool()) + sorted(_SKILL_TOOLS) + sorted(_MCP_TOOLS)}）",
                code="K400",
                field="enabled",
            )
        _ENABLED = uniq
        return {"ok": True, "enabled": sorted(_ENABLED)}

    # ── R9：技能工具（命令通道，语言无关） ──────────────────────────────────

    def _install(self, payload: dict) -> dict:
        """{"op":"install","path":str,"skill"?} → 定点装载技能包 tools.json（fail-closed）。"""
        path_raw = payload.get("path")
        if not isinstance(path_raw, str) or not path_raw.strip():
            return _err("install 需 path（tools.json 绝对路径）", code="K400", field="path")
        try:
            rp = Path(path_raw).resolve(strict=True)
        except OSError as exc:
            return _err(f"tools.json 不可达: {exc}", code="K400", field="path")
        # 越界校验：realpath ⊆ WORKSPACE_ROOT 且 ⊆ skills root（技能目录内，防挪用）
        ws = Path(os.environ.get("WORKSPACE_ROOT") or os.getcwd()).resolve()
        sk = _skills_root()
        if not _within(rp, ws) or not _within(rp, sk):
            return _err(
                f"tools.json 路径越界（须位于 skills 根目录内）: {rp}（skills root: {sk}）",
                code="K400",
                field="path",
            )
        try:
            data = json.loads(rp.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            return _err(f"tools.json 解析失败: {exc}", code="K400", field="path")
        if not isinstance(data, list) or not data:
            return _err("tools.json 必须是非空 JSON 数组", code="K400", field="path")
        # 技能归属：agent-loop 显式传注册名；缺省回退目录名（自扩展约定两者一致）
        skill = payload.get("skill")
        skill_name = skill.strip() if isinstance(skill, str) and skill.strip() else rp.parent.name
        loaded: list[str] = []
        skipped: list[dict] = []
        for i, item in enumerate(data):
            name, error = self._install_one(item, skill_name, rp.parent)
            if error is None:
                loaded.append(name)
            else:
                skipped.append({"index": i, "tool": name, "error": error})
        # pending = 该技能已装载未启用的工具（一键启用的目标清单，含历史装载）
        pending = sorted(n for n, e in _SKILL_TOOLS.items() if e["skill"] == skill_name and n not in _ENABLED)
        return {
            "ok": True,
            "skill": skill_name,
            "loaded": loaded,
            "skipped": skipped,
            "pending": pending,
            "enabled_hint": "loaded 工具未启用（装载≠启用），需 configure 或设置面板启用后生效",
        }

    def _install_one(self, item, skill_name: str, skill_dir: Path) -> tuple[str | None, str | None]:
        """校验并装载单条 ToolSpec + exec。失败返回 (name, error)，不影响其余（fail-closed）。"""
        name = None
        try:
            if not isinstance(item, dict):
                raise ToolError("条目必须是对象")
            for key in ("name", "description", "parameters", "exec"):
                if key not in item:
                    raise ToolError(f"缺少必需键 '{key}'")
            name = item["name"]
            if not isinstance(name, str) or not _SKILL_TOOL_NAME_RE.fullmatch(name):
                raise ToolError(f"工具名非法: {name!r}（字母开头，字母数字/_/-，≤64 字符）")
            if name in ALL_TOOLS:
                raise ToolError(f"工具名 '{name}' 与内置工具冲突（内置不可覆盖）")
            if name in _pool():
                raise ToolError(f"工具名 '{name}' 与已装载的动态工具冲突")
            existing = _SKILL_TOOLS.get(name)
            if existing is not None and existing["skill"] != skill_name:
                raise ToolError(f"工具名 '{name}' 已被技能 '{existing['skill']}' 的工具占用")
            exec_ = item["exec"]
            if not isinstance(exec_, dict):
                raise ToolError("exec 必须是对象")
            cmd = exec_.get("cmd")
            if not isinstance(cmd, list) or not cmd or not all(isinstance(c, str) and c.strip() for c in cmd):
                raise ToolError("exec.cmd 必须是非空字符串数组")
            cwd_raw = exec_.get("cwd")
            if cwd_raw is not None and (not isinstance(cwd_raw, str) or not cwd_raw.strip()):
                raise ToolError("exec.cwd 必须是非空字符串（相对技能目录）")
            # T0：可选 timeout_secs（正数）——按工具覆盖全局 SKILL_TOOL_TIMEOUT_SECS
            timeout_secs = exec_.get("timeout_secs")
            if timeout_secs is not None and (
                isinstance(timeout_secs, bool) or not isinstance(timeout_secs, (int, float)) or timeout_secs <= 0
            ):
                raise ToolError("exec.timeout_secs 必须是正数（秒）")
            if not isinstance(item["parameters"], dict):
                raise ToolError("parameters 必须是 JSON Schema 对象")
            if not isinstance(item["description"], str) or not item["description"].strip():
                raise ToolError("description 必须是非空字符串")
            _SKILL_TOOLS[name] = {
                "name": name,
                "description": item["description"],
                "parameters": item["parameters"],
                "exec": {"cmd": [str(c) for c in cmd], "cwd": cwd_raw, "timeout_secs": timeout_secs},
                "skill": skill_name,
                "dir": str(skill_dir),
            }
            # 延迟启用还原：持久化授权（config.json）先于装载到达 → install 即生效
            if name in _DEFERRED_ENABLED:
                _DEFERRED_ENABLED.discard(name)
                _ENABLED.add(name)
            return name, None
        except ToolError as exc:
            return name, str(exc)
        except Exception as exc:  # noqa: BLE001 - 单项失败跳过，不阻断其余
            return name, f"{type(exc).__name__}: {exc}"

    def _skill_tools(self, payload: dict) -> dict:
        """{"op":"skill_tools","skills"?,"all"?} → 会话清单装配视图 / 配置视图。
        不在 list 契约内——技能工具不出现在内置工具 tab（归属技能界面，R9）。"""
        skills = payload.get("skills")
        show_all = bool(payload.get("all"))
        tools = []
        for name, e in _SKILL_TOOLS.items():
            if isinstance(skills, list) and e["skill"] not in skills:
                continue
            if not show_all and name not in _ENABLED:
                continue
            tools.append(
                {
                    "name": name,
                    "description": e["description"],
                    "parameters": e["parameters"],
                    "skill": e["skill"],
                    **({"enabled": name in _ENABLED} if show_all else {}),
                }
            )
        return {"ok": True, "tools": tools}

    def _call_skill_tool(self, name: str, payload: dict) -> dict:
        """技能工具执行：启用闸 → 受控子进程（语言无关命令协议）。"""
        if name not in _ENABLED:
            return _err(
                f"tool '{name}' 未启用（技能工具装载后需经设置面板/configure 启用；装载≠启用）",
                code="TOOL_DISABLED",
            )
        args = payload.get("args") or {}
        if not isinstance(args, dict):
            return _err(f"args 必须是对象，收到: {type(args).__name__}", code="BAD_ARGS", field="args")
        return self._exec_command(_SKILL_TOOLS[name], args)

    # ── §八：MCP 工具（stdio 传输第三池） ──────────────────────────────────

    def _mcp_tools_view(self, payload: dict) -> dict:
        """{"op":"mcp_tools"} → 配置视图（host 工具 tab MCP 区块数据源）：
        servers 含状态（ready/starting/crashed/stopped/failed），tools 附 enabled。"""
        servers = [
            {
                "name": n,
                "status": s.status,
                "tools": sum(1 for e in _MCP_TOOLS.values() if e["server"] == n),
            }
            for n, s in _MCP_SERVERS.items()
        ]
        servers.extend(
            {"name": n, "status": "failed", "tools": 0, "error": msg} for n, msg in _MCP_FAILED.items()
        )
        tools = [
            {
                "name": name,
                "description": e["description"],
                "parameters": e["parameters"],
                "server": e["server"],
                "enabled": name in _ENABLED,
            }
            for name, e in _MCP_TOOLS.items()
        ]
        return {"ok": True, "servers": servers, "tools": tools}

    def _call_mcp_tool(self, name: str, payload: dict) -> dict:
        """MCP 工具执行：启用闸 → 保活 → JSON-RPC tools/call → content 文本拼接回契约形。"""
        if name not in _ENABLED:
            return _err(
                f"tool '{name}' 未启用（MCP 工具需经设置面板/configure 启用；装载≠启用）",
                code="TOOL_DISABLED",
            )
        args = payload.get("args") or {}
        if not isinstance(args, dict):
            return _err(f"args 必须是对象，收到: {type(args).__name__}", code="BAD_ARGS", field="args")
        entry = _MCP_TOOLS[name]
        server = _MCP_SERVERS.get(entry["server"])
        if server is None:
            return _err(f"MCP server '{entry['server']}' 不可用（启动失败或已跳过）", code="MCP_UNAVAILABLE")
        try:
            server._ensure_running()  # 崩溃退避重启（窗口内 MCP_UNAVAILABLE）
            result = server.call_tool(entry["remote"], args, _skill_tool_timeout())
            text = mcp_mod.content_to_text(result)
        except mcp_mod.McpError as exc:
            return _err(str(exc), code=exc.code)
        except Exception as exc:  # noqa: BLE001 - 失败回喂 LLM，不中断循环
            return _err(f"MCP 调用失败: {type(exc).__name__}: {exc}", code="MCP_ERROR")
        return {"ok": True, "result": text}

    def _exec_command(self, entry: dict, args: dict) -> dict:
        """子进程执行 exec.cmd：stdin={"args":{...}}，stdout=Wire 同形 JSON。
        cwd 缺省技能目录（相对资源可达）；超时杀进程；输出不合契约 → 字段级错误。"""
        cmd = entry["exec"]["cmd"]
        cwd = entry["dir"]
        cwd_raw = entry["exec"].get("cwd")
        if cwd_raw:
            cand = (Path(entry["dir"]) / cwd_raw).resolve()
            if not _within(cand, Path(entry["dir"]).resolve()):
                return _err(f"exec.cwd 越界（不得超出技能目录）: {cwd_raw}", code="K400", field="exec.cwd")
            cwd = str(cand)
        # T0：声明 timeout_secs 优先，缺省回落全局 SKILL_TOOL_TIMEOUT_SECS（install 已保证正数）
        declared = entry["exec"].get("timeout_secs")
        timeout = float(declared) if declared is not None else _skill_tool_timeout()
        try:
            proc = subprocess.run(
                cmd,
                input=json.dumps({"args": args}, ensure_ascii=False),
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                cwd=cwd,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            return _err(
                f"技能工具执行超时（>{timeout:g}s），已终止进程；请减小任务规模或检查执行体",
                code="TOOL_TIMEOUT",
            )
        except OSError as exc:
            return _err(f"执行体不可达（exec.cmd={cmd}）: {exc}", code="TOOL_EXEC_ERROR")
        out = (proc.stdout or "").strip()
        if out:
            try:
                parsed = json.loads(out)
            except ValueError:
                parsed = None
            # 契约形状（{"ok":...}）优先：工具级错误/结果原样回传（执行体自控语义）
            if isinstance(parsed, dict) and "ok" in parsed:
                return parsed
        tail = lambda s: (s or "").strip()[-500:]  # noqa: E731
        return _err(
            f"技能工具输出不符合契约（exit={proc.returncode}）；stderr: {tail(proc.stderr) or '（空）'}；stdout: {tail(out) or '（空）'}",
            code="TOOL_EXEC_ERROR",
        )

    def _call(self, payload: dict) -> dict:
        name = payload.get("name")
        if name in _SKILL_TOOLS:
            return self._call_skill_tool(name, payload)
        if name in _MCP_TOOLS:
            return self._call_mcp_tool(name, payload)
        if name not in _pool():
            avail = sorted(_ENABLED)
            return _err(f"unknown tool: {name}（可用: {', '.join(avail)}）", code="UNKNOWN_TOOL")
        if name not in _ENABLED:
            return _err(
                f"tool '{name}' 未授权（TOOLS_ENABLED={','.join(sorted(_ENABLED))}）。"
                "如需启用请经 configure op 或宿主 TOOLS_ENABLED 调整（装载≠启用）。",
                code="TOOL_DISABLED",
            )
        args = payload.get("args") or {}
        try:
            if not isinstance(args, dict):
                return _err(f"args 必须是对象，收到: {type(args).__name__}", code="BAD_ARGS", field="args")
            return {"ok": True, "result": _pool()[name]["run"](args)}
        except ToolError as exc:
            return _err(str(exc), code=exc.code, field=exc.field)
        except Exception as exc:  # noqa: BLE001 - 工具失败回喂 LLM，不中断循环
            return _err(f"{type(exc).__name__}: {exc}")

    def destroy(self) -> None:
        # §八：MCP server 随插件退出**逆序**回收
        for server in reversed(list(_MCP_SERVERS.values())):
            server.stop()


if __name__ == "__main__":
    serve(ToolsPlugin())
