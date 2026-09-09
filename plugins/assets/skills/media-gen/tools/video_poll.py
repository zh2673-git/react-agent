"""video_poll —— 生视频查询（两段式第二段，tools PLAN §九 M2）。

按 task_id 查询任务状态：succeeded → 下载视频落盘返回 path；running/failed →
返回状态与原因，由 agent 决定继续轮询或报错。查询响应字段做宽容解析（站点差异以
media 配置适配，不做代码级 provider）。

协议：stdin {"args":{task_id}} → stdout {"ok":true,"result":{"status","path"?,"error"?,"note"}}。

配置（env，host 从 config.json media.video 段映射）：
- MEDIA_VIDEO_QUERY_URL（必需）：查询端点，支持 {task_id}/{id} 占位替换；
  无占位时以 query 参数 task_id 传递。相对路径以 MEDIA_VIDEO_BASE_URL 为基拼接
- MEDIA_VIDEO_KEY（可选）
"""

import json
import re
import sys
import os

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import (  # noqa: E402
    MediaToolError, emit, http_download, http_json, read_args, resolve_url,
    save_media, wire_err, wire_ok,
)

# 状态词兼容：大小写/同义词归一为 succeeded / failed / running
_DONE_WORDS = {"succeeded", "success", "succeed", "complete", "completed", "done", "finished", "finish", "ok"}
_FAIL_WORDS = {"failed", "fail", "error", "cancelled", "canceled", "rejected", "timeout"}
# 视频地址键名优先级（宽容解析：先精确键名，后按扩展名兜底，避免抓到封面图等无关 url）
_VIDEO_KEYS = ("video_url", "result_url", "output_url", "download_url", "file_url", "url")
_VIDEO_SUFFIX = re.compile(r"\.(mp4|webm|mov)(\?|$)", re.IGNORECASE)


def main() -> int:
    args = read_args()
    query_url = os.environ.get("MEDIA_VIDEO_QUERY_URL", "").strip()
    base_url = os.environ.get("MEDIA_VIDEO_BASE_URL", "").strip()
    if not query_url:
        emit(wire_err(
            "未配置 MEDIA_VIDEO_QUERY_URL（查询端点）；请在设置面板「媒体模型 · 生视频」补全后重启 host。",
            code="MEDIA_NOT_CONFIGURED",
        ))
        return 0
    task_id = str(args.get("task_id") or "").strip()
    if not task_id:
        emit(wire_err("缺少必填参数 task_id（video_submit 返回的任务 id）", code="MISSING_ARG"))
        return 0

    url = resolve_url(query_url, base_url)
    if "{task_id}" in url:
        url = url.replace("{task_id}", task_id)
    elif "{id}" in url:
        url = url.replace("{id}", task_id)
    else:
        url += ("&" if "?" in url else "?") + "task_id=" + task_id

    # W19 站点默认参数：extra 标量键拼查询参数（如 Agnes 的 model_name——keyframe/
    # reference 模式必须带；text 模式推荐）。url 已含该键（占位/用户手填）时不覆盖。
    raw = os.environ.get("MEDIA_VIDEO_EXTRA", "").strip()
    if raw:
        try:
            obj = json.loads(raw)
        except ValueError:
            obj = None
        if isinstance(obj, dict):
            from urllib.parse import quote
            for k, v in obj.items():
                if k.startswith("_") or k in url or not isinstance(v, (str, int, float, bool)):
                    continue
                url += ("&" if "?" in url else "?") + quote(str(k)) + "=" + quote(str(v))

    try:
        data = http_json("GET", url, key=os.environ.get("MEDIA_VIDEO_KEY", "").strip())
        status = _find_status(data)
        if status in _DONE_WORDS:
            raw_url = _find_video_url(data)
            if not raw_url:
                raise MediaToolError(
                    f"任务已完成但响应中未找到视频地址: {str(data)[:300]}",
                    code="MEDIA_BAD_RESPONSE",
                )
            blob = http_download(raw_url, key=os.environ.get("MEDIA_VIDEO_KEY", "").strip())
            path, nbytes = save_media(blob, "mp4", f"video-{task_id}")
            emit(wire_ok({
                "status": "succeeded", "path": path, "bytes": nbytes,
                "note": "视频已落盘产物目录，在最终回答中以相对路径引用给用户（产物卡可内联播放）。",
            }))
        elif status in _FAIL_WORDS:
            emit(wire_ok({
                "status": "failed",
                "error": _find_fail_reason(data) or "端点未给出失败原因",
                "note": "生视频任务失败：把原因转述给用户；如可修正参数（prompt/时长/分辨率）可重新提交。",
            }))
        else:
            emit(wire_ok({
                "status": "running" if status else "unknown",
                "task_id": task_id,
                "note": f"任务未完成（端点状态: {status or '不识别'}）。等 20-60s 后再次调用 video_poll 查询，勿高频轮询。",
            }))
    except MediaToolError as exc:
        emit(wire_err(str(exc), code=exc.code))
    except Exception as exc:  # noqa: BLE001 - 失败回喂 LLM，不中断循环
        emit(wire_err(f"视频任务查询失败: {type(exc).__name__}: {exc}"))
    return 0


def _find_status(data) -> str:
    """宽容找状态词：顶层 → data/result/video 一层嵌套下的 status/state。"""
    if not isinstance(data, dict):
        return ""
    keys = ("status", "state", "task_status")
    for scope in (data, data.get("data"), data.get("result"), data.get("video")):
        if isinstance(scope, dict):
            for k in keys:
                v = scope.get(k)
                if isinstance(v, str) and v.strip():
                    return v.strip().lower()
    return ""


def _find_video_url(data, depth: int = 0):
    """宽容找视频地址：优先 _VIDEO_KEYS 精确键名（值为 http(s) 串），再按视频扩展名兜底。"""
    if depth > 3 or not isinstance(data, dict):
        return ""
    for k in _VIDEO_KEYS:
        for scope in (data, *data.values()):
            if isinstance(scope, dict):
                v = scope.get(k)
                if isinstance(v, str) and v.startswith(("http://", "https://")):
                    return v
    # 兜底：任意层字符串值以视频扩展名结尾
    for v in data.values():
        if isinstance(v, str) and _VIDEO_SUFFIX.search(v):
            return v
        if isinstance(v, dict):
            hit = _find_video_url(v, depth + 1)
            if hit:
                return hit
        if isinstance(v, list):
            for it in v:
                hit = _find_video_url(it if isinstance(it, dict) else {"url": it} if isinstance(it, str) else {}, depth + 1)
                if hit:
                    return hit
    return ""


def _find_fail_reason(data) -> str:
    """宽容找失败原因：error/fail_message/message 字段（顶层或一层嵌套）。"""
    for scope in (data, data.get("data"), data.get("result")) if isinstance(data, dict) else ():
        if isinstance(scope, dict):
            for k in ("fail_message", "fail_reason", "error", "message", "reason"):
                v = scope.get(k)
                if isinstance(v, str) and v.strip():
                    return v.strip()
                if isinstance(v, dict) and isinstance(v.get("message"), str):
                    return v["message"].strip()
    return ""


if __name__ == "__main__":
    sys.exit(main())
