"""video_submit —— 生视频提交（两段式第一段，tools PLAN §九 M2）。

异步提交生视频任务后立即返回 task_id（视频生成 1-10 分钟级，超出技能工具超时契约，
故拆 submit/poll 两段贴合 ReAct 循环）。查询用配套 video_poll 工具。

协议：stdin {"args":{...}} → stdout {"ok":true,"result":{"task_id","note"}} | {"ok":false,...}。

配置（env，host 从 config.json media.video 段映射）：
- MEDIA_VIDEO_SUBMIT_URL（必需）：提交端点；相对路径（如 /v1/video/submit）以
  MEDIA_VIDEO_BASE_URL 为基拼接——站点差异以配置适配，不做代码级 provider
- MEDIA_VIDEO_MODEL / MEDIA_VIDEO_KEY（可选）

参数：prompt（必填）/ model（可覆盖）/ duration、size 等标量参数原样透传给端点。
"""

import sys
import os

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import (  # noqa: E402
    MediaToolError, emit, http_json, read_args, resolve_url, wire_err, wire_ok,
)

# 透传给端点的参数类型（str/int/float/bool）；容器/嵌套类型不透传防误用
_PASS_TYPES = (str, int, float, bool)


def main() -> int:
    args = read_args()
    submit_url = os.environ.get("MEDIA_VIDEO_SUBMIT_URL", "").strip()
    base_url = os.environ.get("MEDIA_VIDEO_BASE_URL", "").strip()
    if not submit_url and not base_url:
        emit(wire_err(
            "生视频未配置：请在设置面板「媒体模型 · 生视频」填 submit_url/query_url/model/key"
            "（或 config.json media.video 段），保存后重启 host 生效。",
            code="MEDIA_NOT_CONFIGURED",
        ))
        return 0
    if not submit_url:
        emit(wire_err("缺少 MEDIA_VIDEO_SUBMIT_URL（提交端点）；请补配置后重启 host", code="MEDIA_NOT_CONFIGURED"))
        return 0
    prompt = str(args.get("prompt") or "").strip()
    if not prompt:
        emit(wire_err("缺少必填参数 prompt（视频内容描述）", code="MISSING_ARG"))
        return 0
    model = str(args.get("model") or os.environ.get("MEDIA_VIDEO_MODEL", "") or "").strip()

    body = {"prompt": prompt}
    if model:
        body["model"] = model
    for k, v in args.items():
        if k not in ("prompt", "model") and isinstance(v, _PASS_TYPES):
            body[k] = v

    try:
        data = http_json("POST", resolve_url(submit_url, base_url),
                         key=os.environ.get("MEDIA_VIDEO_KEY", "").strip(), body=body)
        task_id = _find_task_id(data)
        if not task_id:
            raise MediaToolError(
                f"提交成功但未找到任务 id（响应形态不识别）: {str(data)[:300]}",
                code="MEDIA_BAD_RESPONSE",
            )
        emit(wire_ok({
            "task_id": str(task_id),
            "note": "任务已提交。用 video_poll 传此 task_id 查询进度；生成需 1-10 分钟，"
                    "返回 running 时等 20-60s 再查，勿连续高频轮询。",
        }))
    except MediaToolError as exc:
        emit(wire_err(str(exc), code=exc.code))
    except Exception as exc:  # noqa: BLE001 - 失败回喂 LLM，不中断循环
        emit(wire_err(f"视频任务提交失败: {type(exc).__name__}: {exc}"))
    return 0


def _find_task_id(data) -> str:
    """宽容解析任务 id：顶层/一层嵌套下的 id / task_id / video_id / job_id。"""
    if not isinstance(data, dict):
        return ""
    candidates = ["task_id", "id", "video_id", "job_id"]
    for scope in (data, data.get("data"), data.get("result"), data.get("video")):
        if isinstance(scope, dict):
            for k in candidates:
                v = scope.get(k)
                if v:
                    return str(v)
    return ""


if __name__ == "__main__":
    sys.exit(main())
