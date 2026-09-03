"""provider registry：导入期自动发现 providers/ 下的 pack，形状不合规即拒绝（规则前置拦截）。

选路：请求 payload["provider"] > 环境变量 LLM_PROVIDER > mock。
"""

import importlib
import inspect
import os
import pkgutil

from . import base as _base

PROVIDERS: dict[str, dict] = {}


def _validate(name: str, spec) -> None:
    if not isinstance(spec, dict):
        raise TypeError(f"provider '{name}': PROVIDER 必须是 dict")
    for key in ("name", "chat"):
        if key not in spec:
            raise TypeError(f"provider '{name}': PROVIDER 缺少 '{key}'")
    if not callable(spec["chat"]):
        raise TypeError(f"provider '{name}': chat 必须可调用")
    if len(inspect.signature(spec["chat"]).parameters) != 1:
        raise TypeError(f"provider '{name}': chat 签名必须为 chat(payload) -> dict")
    if spec["name"] in PROVIDERS:
        raise TypeError(f"provider 重名: {spec['name']}")


def _load_all() -> None:
    import providers as pkg

    for m in pkgutil.iter_modules(pkg.__path__):
        if m.name in ("base", "registry", "__init__"):
            continue
        mod = importlib.import_module(f"providers.{m.name}")
        spec = getattr(mod, "PROVIDER", None)
        if spec is None:  # 无 PROVIDER 的辅助模块（如共享实现）允许存在
            continue
        _validate(m.name, spec)
        PROVIDERS[spec["name"]] = spec


_load_all()


def resolve(name: str | None):
    """按名取 provider chat 函数；未知名返回 None（分派层报字段级错误）。"""
    key = name or os.environ.get("LLM_PROVIDER", "mock")
    return PROVIDERS.get(key)
