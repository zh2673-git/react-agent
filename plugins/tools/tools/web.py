"""web 工具：搜索后端链（区域选路 + 反爬纪律）+ web_read（Jina 区域镜像 + 直抓兜底）。

可达性事实（05 §3）：DDG/Brave/Google 大陆不可直连；Bing/搜狗/百度直连。
零 key 链 = HTML 抓取公开结果页 + 多引擎故障转移；有 key 自动提升（博查/千帆/Tavily）。
反爬纪律：不绕验证码/PoW（引擎挑战即降级下一引擎）；每引擎冷却限速；
全部失败时报错附「已试引擎列表 + hint」。
"""

import html as html_mod
import json
import re
import time
import urllib.parse
import urllib.request

from . import ToolError, optional_int, require

_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"
_TIMEOUT = 15
_COOLDOWN_S = 2.0
_last_call: dict[str, float] = {}

_TAG_RE = re.compile(r"<[^>]+>")
_WS_RE = re.compile(r"\s+")


def _region() -> str:
    r = (__import__("os").environ.get("SEARCH_REGION") or "cn").strip().lower()
    return r if r in ("cn", "global") else "cn"


def _strip_tags(s: str) -> str:
    return _WS_RE.sub(" ", html_mod.unescape(_TAG_RE.sub("", s))).strip()


def _throttle(engine: str) -> None:
    now = time.monotonic()
    wait = _COOLDOWN_S - (now - _last_call.get(engine, 0.0))
    if wait > 0:
        time.sleep(wait)
    _last_call[engine] = time.monotonic()


def _http_get(url: str, timeout: int = _TIMEOUT, max_bytes: int = 512 * 1024) -> tuple[int, bytes]:
    req = urllib.request.Request(url, headers={"User-Agent": _UA, "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310 - scheme 已在上游白名单
        return resp.status, resp.read(max_bytes)


# ---- 零 key 引擎（HTML 抓取，每个返回 [{title,url,snippet}]，失败抛异常） --------

_BING_RE = re.compile(
    r'<li class="b_algo".*?<h2[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>.*?<p[^>]*>(.*?)</p>',
    re.S,
)


def _eng_bing(query: str, max_results: int) -> list[dict]:
    _, body = _http_get(f"https://cn.bing.com/search?q={urllib.parse.quote(query)}&count={max_results}")
    page = body.decode("utf-8", errors="replace")
    out = []
    for m in _BING_RE.finditer(page):
        url, title, snippet = m.group(1), _strip_tags(m.group(2)), _strip_tags(m.group(3))
        if url.startswith("http"):
            out.append({"title": title, "url": url, "snippet": snippet})
        if len(out) >= max_results:
            break
    return out


_SOGOU_A_RE = re.compile(r'<h3[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>', re.S)
_SOGOU_SNIPPET_RE = re.compile(r'class="(?:str-text-info|fz-mid space-txt|text-layout)"[^>]*>(.*?)</(?:p|div|span)>', re.S)


def _eng_sogou(query: str, max_results: int) -> list[dict]:
    _, body = _http_get(f"https://www.sogou.com/web?query={urllib.parse.quote(query)}")
    page = body.decode("utf-8", errors="replace")
    if "验证码" in page or "antispider" in page.lower():
        raise RuntimeError("sogou 返回验证码页（不绕行，降级下一引擎）")
    anchors = _SOGOU_A_RE.findall(page)[:max_results]
    snippets = [_strip_tags(s) for s in _SOGOU_SNIPPET_RE.findall(page)]
    out = []
    for i, (href, title) in enumerate(anchors):
        url = urllib.parse.urljoin("https://www.sogou.com", href)
        out.append({"title": _strip_tags(title), "url": url, "snippet": snippets[i] if i < len(snippets) else ""})
    return out


_BAIDU_A_RE = re.compile(r'<h3[^>]*>\s*<a[^>]*href="(http[^"]+)"[^>]*>(.*?)</a>', re.S)
_BAIDU_SNIPPET_RE = re.compile(r'class="content-right_[^"]*"[^>]*>(.*?)</span>', re.S)


def _eng_baidu(query: str, max_results: int) -> list[dict]:
    _, body = _http_get(f"https://www.baidu.com/s?wd={urllib.parse.quote(query)}&rn={max_results}")
    page = body.decode("utf-8", errors="replace")
    if "百度安全验证" in page or "wappass" in page.lower():
        raise RuntimeError("baidu 要求安全验证（不绕行，降级下一引擎）")
    anchors = _BAIDU_A_RE.findall(page)[:max_results]
    snippets = [_strip_tags(s) for s in _BAIDU_SNIPPET_RE.findall(page)]
    out = []
    for i, (href, title) in enumerate(anchors):
        out.append({"title": _strip_tags(title), "url": href, "snippet": snippets[i] if i < len(snippets) else ""})
    return out


def _eng_duckduckgo(query: str, max_results: int) -> list[dict]:
    req = urllib.request.Request(
        "https://html.duckduckgo.com/html/",
        data=urllib.parse.urlencode({"q": query}).encode(),
        headers={"User-Agent": _UA},
    )
    with urllib.request.urlopen(req, timeout=_TIMEOUT) as resp:  # noqa: S310 - 固定域名
        page = resp.read(512 * 1024).decode("utf-8", errors="replace")
    a_re = re.compile(r'<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>', re.S)
    sn_re = re.compile(r'class="result__snippet"[^>]*>(.*?)</a>', re.S)
    snippets = [_strip_tags(s) for s in sn_re.findall(page)]
    out = []
    for i, (href, title) in enumerate(a_re.findall(page)[:max_results]):
        if "uddg=" in href:  # DDG 跳转链 → 解出真实 URL
            q = urllib.parse.urlsplit(href).query
            href = urllib.parse.unquote(dict(p.split("=", 1) for p in q.split("&") if "=" in p).get("uddg", href))
        out.append({"title": _strip_tags(title), "url": href, "snippet": snippets[i] if i < len(snippets) else ""})
    return out


def _eng_ddgs(query: str, max_results: int) -> list[dict]:
    try:
        from ddgs import DDGS  # 新包名
    except ImportError:
        from duckduckgo_search import DDGS  # 旧包名
    out = []
    for r in DDGS().text(query, max_results=max_results):
        out.append({"title": r.get("title", ""), "url": r.get("href", ""), "snippet": r.get("body", "")})
    return out


# ---- 有 key 引擎（结果质量更稳，自动提升） --------------------------------------

def _eng_bocha(query: str, max_results: int) -> list[dict]:
    import os

    key = os.environ.get("BOCHA_API_KEY", "")
    body = json.dumps({"query": query, "count": max_results, "summary": False}).encode()
    req = urllib.request.Request(
        "https://api.bochaai.com/v1/web-search",
        data=body,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=_TIMEOUT) as resp:  # noqa: S310 - 固定域名
        data = json.loads(resp.read())
    pages = (data.get("data") or {}).get("webPages") or {}
    return [{"title": v.get("name", ""), "url": v.get("url", ""), "snippet": v.get("snippet", "")} for v in pages.get("value", [])[:max_results]]


def _eng_baidu_ai(query: str, max_results: int) -> list[dict]:
    import os

    key = os.environ.get("BAIDU_API_KEY", "")
    body = json.dumps({"messages": [{"content": query, "role": "user"}], "search_source": "baidu_search"}).encode()
    req = urllib.request.Request(
        "https://qianfan.baidubce.com/v2/ai_search",
        data=body,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=_TIMEOUT) as resp:  # noqa: S310 - 固定域名
        data = json.loads(resp.read())
    return [{"title": r.get("title", ""), "url": r.get("url", ""), "snippet": _strip_tags(r.get("content", ""))} for r in data.get("references", [])[:max_results]]


def _eng_tavily(query: str, max_results: int) -> list[dict]:
    import os

    key = os.environ.get("TAVILY_API_KEY", "")
    body = json.dumps({"api_key": key, "query": query, "max_results": max_results}).encode()
    req = urllib.request.Request("https://api.tavily.com/search", data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=_TIMEOUT) as resp:  # noqa: S310 - 固定域名
        data = json.loads(resp.read())
    return [{"title": r.get("title", ""), "url": r.get("url", ""), "snippet": r.get("content", "")[:200]} for r in data.get("results", [])[:max_results]]


_ENGINES = {
    "bing": _eng_bing,
    "sogou": _eng_sogou,
    "baidu": _eng_baidu,
    "duckduckgo": _eng_duckduckgo,
    "ddgs": _eng_ddgs,
    "bocha": _eng_bocha,
    "baidu_ai": _eng_baidu_ai,
    "tavily": _eng_tavily,
}

_ZERO_KEY_CHAINS = {
    "cn": ["bing", "sogou", "baidu"],
    "global": ["ddgs", "duckduckgo", "bing"],
}
_KEYED_PRIORITY = [("bocha", "BOCHA_API_KEY"), ("baidu_ai", "BAIDU_API_KEY"), ("tavily", "TAVILY_API_KEY")]


def _backend_chain() -> list[str]:
    import os

    forced = (os.environ.get("SEARCH_BACKEND") or "").strip().lower()
    if forced:
        return [forced]
    chain = [name for name, env in _KEYED_PRIORITY if os.environ.get(env)]
    for name in _ZERO_KEY_CHAINS[_region()]:
        if name not in chain:
            chain.append(name)
    return chain


def _web_search(args: dict) -> dict:
    query = require(args, "query")
    max_results = optional_int(args, "max_results", 5, 1, 10)
    chain = _backend_chain()
    tried = []
    for engine in chain:
        fn = _ENGINES.get(engine)
        if fn is None:
            tried.append(f"{engine}: 未知引擎（合法值: {', '.join(_ENGINES)}）")
            continue
        try:
            _throttle(engine)
            results = fn(query, max_results)
            if results:
                return {"query": query, "engine": engine, "region": _region(), "results": results}
            tried.append(f"{engine}: 0 条结果")
        except Exception as exc:  # noqa: BLE001 - 单引擎失败降级下一引擎
            tried.append(f"{engine}: {type(exc).__name__}: {exc}")
    hint = (
        "所有搜索引擎都失败了。可选：1) 检查网络连通性；2) 设置 SEARCH_BACKEND 指定单一引擎调试；"
        "3) 配置免费额度后端（BAIDU_API_KEY 每日 100 次 / TAVILY_API_KEY 每月 1000 次）或付费 BOCHA_API_KEY。"
    )
    raise ToolError("搜索引擎链全部失败。已试: " + " | ".join(tried) + "。" + hint, code="SEARCH_ALL_ENGINES_FAILED")


# ---- web_read：Jina 区域镜像优先，失败回退本地直抓（含 URL 缓存 + 字符分页，v3） ----

_READ_TTL_S = 600
_READ_CACHE: dict[str, tuple[float, str]] = {}
_READ_CACHE_MAX = 32
_CACHEABLE_CHARS = 256 * 1024


def _cache_get(url: str) -> str | None:
    hit = _READ_CACHE.get(url)
    if hit is None:
        return None
    ts, text = hit
    if time.monotonic() - ts > _READ_TTL_S:
        _READ_CACHE.pop(url, None)
        return None
    return text


def _cache_put(url: str, text: str) -> None:
    _READ_CACHE.pop(url, None)
    if len(text) <= _CACHEABLE_CHARS:
        _READ_CACHE[url] = (time.monotonic(), text)
    while len(_READ_CACHE) > _READ_CACHE_MAX:
        _READ_CACHE.pop(next(iter(_READ_CACHE)))


def _jina_read(url: str) -> str:
    host = "https://r.jinaai.cn/" if _region() == "cn" else "https://r.jina.ai/"
    status, body = _http_get(host + url, timeout=_TIMEOUT)
    if status != 200:
        raise RuntimeError(f"jina HTTP {status}")
    text = body.decode("utf-8", errors="replace")
    if len(text.strip()) < 32:
        raise RuntimeError("jina 返回空内容")
    return text


def _direct_read(url: str) -> str:
    status, body = _http_get(url, timeout=_TIMEOUT)
    page = body.decode("utf-8", errors="replace")
    page = re.sub(r"(?is)<(script|style|noscript)[^>]*>.*?</\1>", " ", page)
    text = _strip_tags(page)
    if len(text.strip()) < 32:
        raise RuntimeError("正文过短（可能是动态渲染页或非 HTML）")
    return text


def _web_read(args: dict) -> dict:
    url = require(args, "url")
    scheme = urllib.parse.urlsplit(url).scheme.lower()
    if scheme not in ("http", "https"):
        raise ToolError(f"scheme '{scheme}' 不允许（仅 http/https）", code="BAD_SCHEME", field="url")
    cached = _cache_get(url)
    if cached is not None:  # 10 分钟内同 URL 直接命中缓存，不再出网
        text, via = cached, "cache"
    else:
        text, via, errors = None, None, []
        for name, fn in (("jina", _jina_read), ("direct", _direct_read)):
            try:
                text = fn(url)
                via = name
                break
            except Exception as exc:  # noqa: BLE001 - 降级下一读取器
                errors.append(f"{name}: {type(exc).__name__}: {exc}")
        if text is None:
            raise ToolError(
                f"网页读取失败（两种方式均失败: {' | '.join(errors)}）。"
                "可先用 web_search 换来源，或确认 URL 可达。",
                code="READ_FAILED",
                field="url",
            )
        _cache_put(url, text)
    total = len(text)
    offset = optional_int(args, "offset", 0, 0, 10**7)
    limit = optional_int(args, "limit", 32_000, 1, 64_000)
    result = {
        "url": url,
        "via": via,
        "total_chars": total,
        "offset": offset,
        "content": text[offset : offset + limit],
    }
    if offset + limit < total:
        result["next_offset"] = offset + limit  # 还有后续页，用 offset=next_offset 续读
    return result


TOOLS = {
    "web_search": {
        "description": (
            "Web search. Returns [{title,url,snippet}] with source URLs. "
            "Args: query (required), max_results (1-10, default 5). "
            "Region auto-routed via SEARCH_REGION (cn: bing/sogou/baidu direct, no key needed)."
        ),
        "parameters": {
            "type": "object",
            "properties": {"query": {"type": "string"}, "max_results": {"type": "integer"}},
            "required": ["query"],
        },
        "run": _web_search,
    },
    "web_read": {
        "description": (
            "Read a web page as markdown-ish text (Jina Reader with CN mirror, fallback to direct "
            "fetch; per-URL 10-min in-process cache). Char-paged: result has total_chars and "
            "next_offset when more remains. Args: url (required, http/https), offset (chars, default 0), "
            "limit (chars, default 32000, max 64000)."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "offset": {"type": "integer", "description": "char offset for paging"},
                "limit": {"type": "integer", "description": "chars per page (1000-64000)"},
            },
            "required": ["url"],
        },
        "run": _web_read,
    },
}
