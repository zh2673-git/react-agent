"""媒体生成技能工具公共层（media-gen 技能内自包含，仅标准库 + httpx）。

职责：
- Wire 契约输出：成功 {"ok":true,"result":{...}} / 失败 {"ok":false,"error":{"code","message"}}——
  与技能工具子进程协议（tools_plugin._exec_command）同形，ok 字段优先透传。
- 产物落盘：统一写入产物目录（AGENT_OUTPUT_DIR 或 <工作区>/outputs，与 bash 工具同口径），
  目录解析到工作区外时回落 <工作区>/outputs（产物卡 /files 通道以 WORKSPACE_ROOT 为根）。
- URL 拼接宽容：站点差异以 media 配置适配，不做代码级 provider（tools PLAN §九）。
"""

import base64
import json
import os
import re
import sys
import time

# HTTP 客户端：仓库既有依赖（插件侧 web_read 同款），不引入新依赖
import httpx


def wire_ok(result: dict) -> dict:
    """成功输出：result 为工具结果对象（path/bytes/task_id 等字段）。"""
    return {"ok": True, "result": result}


def wire_err(message: str, code: str = "MEDIA_ERROR") -> dict:
    """失败输出：可读中文信息回喂 LLM，绝不裸异常/裸退出码。"""
    return {"ok": False, "error": {"code": code, "message": message}}


def emit(payload: dict) -> None:
    sys.stdout.write(json.dumps(payload, ensure_ascii=False))
    sys.stdout.flush()


def workspace_root() -> str:
    return os.environ.get("WORKSPACE_ROOT") or os.getcwd()


def out_dir() -> str:
    """产物目录：AGENT_OUTPUT_DIR（相对则拼工作区）或 <工作区>/outputs；出界回落缺省。"""
    ws = os.path.abspath(workspace_root())
    raw = os.environ.get("AGENT_OUTPUT_DIR") or "outputs"
    cand = raw if os.path.isabs(raw) else os.path.join(ws, raw)
    cand = os.path.abspath(cand)
    if cand != ws and not cand.startswith(ws + os.sep):
        cand = os.path.join(ws, "outputs")
    return cand


def slugify(text: str, limit: int = 40) -> str:
    """prompt → 文件名 slug：仅保留 ASCII 字母数字与连字符；全非 ASCII 时回退 'media'。"""
    s = re.sub(r"[^A-Za-z0-9]+", "-", str(text)).strip("-").lower()
    return s[:limit].strip("-") or "media"


def save_media(data: bytes, ext: str, prompt: str) -> tuple[str, int]:
    """按「时间戳+主题 slug」命名落盘，返回 (工作区相对路径正斜杠形式, 字节数)。"""
    d = out_dir()
    os.makedirs(d, exist_ok=True)
    name = f"{time.strftime('%Y%m%d-%H%M%S')}-{slugify(prompt)}.{ext}"
    full = os.path.join(d, name)
    # 时间戳秒级可能同秒重名（同回合同主题多次生成）：追加序号
    i = 1
    while os.path.exists(full):
        i += 1
        full = os.path.join(d, f"{time.strftime('%Y%m%d-%H%M%S')}-{i}-{slugify(prompt)}.{ext}")
    with open(full, "wb") as f:
        f.write(data)
    rel = os.path.relpath(full, os.path.abspath(workspace_root())).replace("\\", "/")
    return rel, len(data)


def auth_headers(key: str) -> dict:
    """Bearer 认证头；key 未配置时省略（部分本地端点免鉴权）。"""
    return {"Authorization": f"Bearer {key}", "Content-Type": "application/json"} if key else {"Content-Type": "application/json"}


def api_base(base_url: str) -> str:
    """去尾斜杠；不以 /v1 结尾则补 /v1（OpenAI 兼容端点两种给法都接）。"""
    b = (base_url or "").strip().rstrip("/")
    return b if b.endswith("/v1") else b + "/v1"


def resolve_url(url: str, base_url: str) -> str:
    """绝对 URL 直用；相对路径（如 /v1/video/submit）以 media.video.base_url 为基拼接。"""
    u = (url or "").strip()
    if u.startswith("http://") or u.startswith("https://"):
        return u
    base = (base_url or "").strip().rstrip("/")
    if not base:
        return u
    return base + ("/" + u.lstrip("/") if not u.startswith("/") else u)


def image_ext(data: bytes, default: str = "png") -> str:
    """按魔数识别图片格式，缺省 png。"""
    if data.startswith(b"\x89PNG"):
        return "png"
    if data.startswith(b"\xff\xd8"):
        return "jpg"
    if data.startswith((b"GIF87a", b"GIF89a")):
        return "gif"
    if data.startswith(b"RIFF") and data[8:12] == b"WEBP":
        return "webp"
    return default


def b64_decode_loose(s: str) -> bytes:
    """宽容 base64 解码：容忍 dataURI 前缀与换行。"""
    s = re.sub(r"^data:[^,]+,", "", s.strip()).replace("\n", "").replace("\r", "")
    return base64.b64decode(s)


def http_json(method: str, url: str, *, key: str = "", body: dict | None = None,
              timeout: float = 120.0, headers: dict | None = None) -> dict:
    """统一 HTTP：超时/网络错误抛 MediaToolError（可读信息），2xx 返回解析后的 JSON。
    headers 为附加头（如 X-ModelScope-* 异步族），与认证头合并。"""
    hh = auth_headers(key)
    if headers:
        hh.update(headers)
    try:
        resp = httpx.request(method, url, json=body, headers=hh, timeout=timeout, follow_redirects=True)
    except httpx.TimeoutException:
        raise MediaToolError(f"请求超时（>{timeout:g}s）: {url}", code="MEDIA_TIMEOUT")
    except httpx.HTTPError as exc:
        raise MediaToolError(f"网络请求失败: {type(exc).__name__}: {exc}", code="MEDIA_NETWORK")
    if resp.status_code // 100 != 2:
        raise MediaToolError(
            f"HTTP {resp.status_code}（端点 {url}）: {(resp.text or '')[:300]}",
            code="MEDIA_HTTP",
        )
    try:
        return resp.json()
    except ValueError:
        raise MediaToolError(f"端点返回非 JSON 响应: {(resp.text or '')[:300]}", code="MEDIA_BAD_RESPONSE")


def http_download(url: str, *, key: str = "", timeout: float = 120.0) -> bytes:
    """下载二进制成品（图片 url / 视频文件 url）。"""
    try:
        resp = httpx.get(url, headers={"Authorization": f"Bearer {key}"} if key else {},
                         timeout=timeout, follow_redirects=True)
    except httpx.TimeoutException:
        raise MediaToolError(f"下载超时（>{timeout:g}s）: {url}", code="MEDIA_TIMEOUT")
    except httpx.HTTPError as exc:
        raise MediaToolError(f"下载失败: {type(exc).__name__}: {exc}", code="MEDIA_NETWORK")
    if resp.status_code // 100 != 2:
        raise MediaToolError(f"下载 HTTP {resp.status_code}: {url}", code="MEDIA_HTTP")
    return resp.content


class MediaToolError(Exception):
    """带错误码的领域异常：main 捕获后转 wire_err，信息始终可读。"""

    def __init__(self, message: str, code: str = "MEDIA_ERROR"):
        super().__init__(message)
        self.code = code


def read_args() -> dict:
    """读子进程协议 stdin：{"args":{...}}；容错空输入与残片。"""
    try:
        raw = sys.stdin.read()
    except Exception:  # noqa: BLE001 - stdin 异常按空参数处理
        raw = ""
    try:
        payload = json.loads(raw) if raw.strip() else {}
    except ValueError:
        payload = {}
    args = payload.get("args")
    return args if isinstance(args, dict) else {}
