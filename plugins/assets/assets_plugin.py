"""assets guest 插件（Python，Process 域 gRPC）——skills + prompts 注册表。

本质：按需加载的知识资产注册表（06）。渐进式披露：
  Discovery: skills.list 只出 name+description（~100 token/个，随系统提示词注入）
  Activation: skills.load 出全文（模型经 agent-loop 保留名 load_skill 调用）
  Execution: 复用基础工具（assets 从不执行 skill 代码，语言无关）

线契约（03 §2.4）：
  {"op":"skills.list"}             → {"ok":true,"skills":[{"name","description"}],"root":str}
                                     每次调用重扫目录（08：Web 技能 CRUD / L1 自扩展实时可见）；
                                     root=skills 根目录绝对路径（agent-loop 自扩展可达性探测用）
  {"op":"skills.load","name":str}  → {"ok":true,"content":str} | {"ok":false,"error":{...}}（读取前重扫）
  {"op":"prompts.list"}            → {"ok":true,"prompts":[{"name","description"}]}
  {"op":"prompts.get","name":str}  → {"ok":true,"content":str}

skill = 目录含 SKILL.md，frontmatter 仅必需 name/description（手写解析，不引 PyYAML）；
prompt = markdown 文件，首行 `# name`，次个非空行为描述。
frontmatter 不合规的目录跳过（warn），不影响其他 skill。
"""

import os
import sys
from pathlib import Path

from agent_kernel.guest import serve

_MAX_LOAD_BYTES = 256 * 1024


def _err(message: str, code: str = "ASSET_ERROR") -> dict:
    return {"ok": False, "error": {"code": code, "message": message}}


def _parse_frontmatter(text: str) -> dict | None:
    """最小 frontmatter：`---` 围栏内的 `key: value` 行。缺 name/description → None。"""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return None
    meta: dict = {}
    for line in lines[1:]:
        if line.strip() == "---":
            break
        if ":" in line and not line.startswith((" ", "\t")):
            key, _, value = line.partition(":")
            meta[key.strip()] = value.strip()
    if not meta.get("name") or not meta.get("description"):
        return None
    return meta


def _scan_skills(root: Path) -> dict:
    catalog: dict = {}
    if not root.is_dir():
        return catalog
    for d in sorted(root.iterdir()):
        skill_md = d / "SKILL.md"
        if not d.is_dir() or not skill_md.is_file():
            continue
        try:
            meta = _parse_frontmatter(skill_md.read_text(encoding="utf-8")[:65536])
        except OSError as exc:
            print(f"[assets] 读取失败，跳过 {d.name}: {exc}", file=sys.stderr)
            continue
        if meta is None:
            print(f"[assets] frontmatter 缺 name/description，跳过 {d.name}", file=sys.stderr)
            continue
        catalog[meta["name"]] = {"description": meta["description"], "path": skill_md}
    return catalog


def _scan_prompts(root: Path) -> dict:
    catalog: dict = {}
    if not root.is_dir():
        return catalog
    for f in sorted(root.glob("*.md")):
        try:
            text = f.read_text(encoding="utf-8")[:65536]
        except OSError as exc:
            print(f"[assets] 读取失败，跳过 {f.name}: {exc}", file=sys.stderr)
            continue
        lines = [ln.strip() for ln in text.splitlines()]
        if not lines or not lines[0].startswith("# "):
            print(f"[assets] 首行非 '# name'，跳过 {f.name}", file=sys.stderr)
            continue
        name = lines[0][2:].strip()
        description = next((ln for ln in lines[1:] if ln), "")
        if name:
            catalog[name] = {"description": description, "path": f}
    return catalog


class AssetsPlugin:
    def manifest(self) -> dict:
        return {"id": "assets", "version": "0.1.0", "api_version": "0.1"}

    def init(self, config) -> None:
        here = Path(__file__).resolve().parent
        self._skills_root = Path(os.environ.get("SKILLS_DIR") or here / "skills")
        self._prompts_root = Path(os.environ.get("PROMPTS_DIR") or here / "prompts")
        self._skills_root.mkdir(parents=True, exist_ok=True)
        self._prompts_root.mkdir(parents=True, exist_ok=True)
        self._skills = _scan_skills(self._skills_root)
        self._prompts = _scan_prompts(self._prompts_root)

    def on_event(self, envelope: dict) -> dict:
        payload = envelope.get("payload") or {}
        op = payload.get("op")
        if op == "skills.list":
            self._skills = _scan_skills(self._skills_root)  # 每次重扫：Web CRUD / 自扩展写盘即时可见
            return {
                "ok": True,
                "skills": [{"name": n, "description": s["description"]} for n, s in self._skills.items()],
                "root": str(self._skills_root),
            }
        if op == "skills.load":
            name = payload.get("name")
            self._skills = _scan_skills(self._skills_root)  # 读取前重扫：刚写入的技能立即可激活
            entry = self._skills.get(name)
            if entry is None:
                avail = ", ".join(sorted(self._skills)) or "（无）"
                return _err(f"unknown skill: {name}（可用: {avail}）", code="UNKNOWN_SKILL")
            try:
                content = entry["path"].read_text(encoding="utf-8")[:_MAX_LOAD_BYTES]
            except OSError as exc:
                return _err(f"skill 读取失败: {exc}")
            return {"ok": True, "content": content}
        if op == "prompts.list":
            self._prompts = _scan_prompts(self._prompts_root)
            return {
                "ok": True,
                "prompts": [{"name": n, "description": s["description"]} for n, s in self._prompts.items()],
            }
        if op == "prompts.get":
            name = payload.get("name")
            self._prompts = _scan_prompts(self._prompts_root)
            entry = self._prompts.get(name)
            if entry is None:
                avail = ", ".join(sorted(self._prompts)) or "（无）"
                return _err(f"unknown prompt: {name}（可用: {avail}）", code="UNKNOWN_PROMPT")
            try:
                content = entry["path"].read_text(encoding="utf-8")[:_MAX_LOAD_BYTES]
            except OSError as exc:
                return _err(f"prompt 读取失败: {exc}")
            return {"ok": True, "content": content}
        return _err(f"unknown op: {op}", code="K400")

    def destroy(self) -> None:
        self._skills = {}
        self._prompts = {}


if __name__ == "__main__":
    serve(AssetsPlugin())
