"""image_gen —— 生图技能工具（OpenAI 兼容 /v1/images/generations，tools PLAN §九 M1）。

协议：stdin 收 {"args":{...}}；stdout 回 {"ok":true,"result":{"path","bytes","note"}}
或 {"ok":false,"error":{"code","message"}}（技能工具子进程契约，ok 字段优先透传）。

配置（env，host 从 config.json media.image 段映射 + passthrough 透传）：
- MEDIA_IMAGE_BASE_URL（必需）：OpenAI 兼容端点，如 https://api.siliconflow.cn/v1
- MEDIA_IMAGE_MODEL（可选）：缺省模型名
- MEDIA_IMAGE_KEY（可选）：Bearer key

参数：prompt（必填）/ size / n / model（可覆盖缺省）。
产物：落盘产物目录（AGENT_OUTPUT_DIR 或 <工作区>/outputs），时间戳+主题 slug 命名。
"""

import sys
import os
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import (  # noqa: E402
    MediaToolError, api_base, b64_decode_loose, emit, http_download, http_json,
    image_ext, read_args, save_media, wire_err, wire_ok,
)


def poll_task_images(task_id: str, base_url: str, key: str, budget_s: float = 90.0) -> list:
    """异步任务轮询（ModelScope 现行协议）：GET {base}/v1/tasks/{id} 需带
    X-ModelScope-Task-Type: image_generation 头（缺头会「task not found」——任务按类型分命名空间）。
    SUCCEED → output_images（url 列表）；FAILED → 报 errors.message；超预算 → 超时。"""
    url = api_base(base_url) + "/tasks/" + task_id
    extra = {"X-ModelScope-Task-Type": "image_generation"}
    deadline = time.monotonic() + budget_s
    while time.monotonic() < deadline:
        time.sleep(3.0)
        v = http_json("GET", url, key=key, headers=extra, timeout=30.0)
        st = str(v.get("task_status") or "").upper()
        if st == "SUCCEED":
            urls = v.get("output_images")
            if isinstance(urls, list) and urls:
                return urls
            raise MediaToolError(f"任务 SUCCEED 但缺 output_images: {str(v)[:200]}", code="MEDIA_BAD_RESPONSE")
        if st == "FAILED":
            msg = (v.get("errors") or {}).get("message") or "未知原因"
            raise MediaToolError(f"生图任务失败: {msg}", code="MEDIA_TASK_FAILED")
    raise MediaToolError(f"生图任务超时（>{budget_s:g}s 未完成）", code="MEDIA_TIMEOUT")


def main() -> int:
    args = read_args()
    base_url = os.environ.get("MEDIA_IMAGE_BASE_URL", "").strip()
    if not base_url:
        emit(wire_err(
            "生图未配置：请在设置面板「媒体模型 · 生图」填 base_url/model/key（或 config.json media.image 段），"
            "保存后重启 host 生效。",
            code="MEDIA_NOT_CONFIGURED",
        ))
        return 0
    prompt = str(args.get("prompt") or "").strip()
    if not prompt:
        emit(wire_err("缺少必填参数 prompt（画面描述）", code="MISSING_ARG", ))
        return 0
    model = str(args.get("model") or os.environ.get("MEDIA_IMAGE_MODEL", "") or "").strip()
    if not model:
        emit(wire_err("缺少模型名：请在配置中设 media.image.model，或调用时传 model 参数", code="MISSING_ARG"))
        return 0

    body = {"model": model, "prompt": prompt}
    if args.get("size"):
        body["size"] = str(args["size"]).strip()
    if args.get("n"):
        try:
            body["n"] = max(1, min(4, int(args["n"])))
        except (TypeError, ValueError):
            emit(wire_err("参数 n 需为 1-4 的整数", code="BAD_ARGS"))
            return 0

    try:
        # 提交即带头 X-ModelScope-Async-Mode: true（ModelScope 生图已全面异步，同步调用 400；
        # 其他 OpenAI 兼容端点忽略未知头，零影响）——响应形状三分支适配：
        # ① data 数组（OpenAI 同步，siliconflow 等）② images 数组（旧 ModelScope 同步）
        # ③ task_id（异步任务式：轮询 /tasks/{id} 取 output_images）
        data = http_json("POST", api_base(base_url) + "/images/generations",
                         key=os.environ.get("MEDIA_IMAGE_KEY", "").strip(), body=body,
                         headers={"X-ModelScope-Async-Mode": "true"})
        items = None
        if isinstance(data, dict):
            cand = data.get("data") or data.get("images")
            if isinstance(cand, list) and cand:
                items = cand
            elif data.get("task_id"):
                urls = poll_task_images(str(data["task_id"]), base_url,
                                        os.environ.get("MEDIA_IMAGE_KEY", "").strip())
                items = [{"url": u} for u in urls]
        if not isinstance(items, list) or not items:
            raise MediaToolError(
                "响应缺少 data/images 数组且无 task_id（检查端点是否为生图接口）: " + str(data)[:200],
                code="MEDIA_BAD_RESPONSE")
        saved = []
        for item in items[: body.get("n", 1)]:
            if not isinstance(item, dict):
                continue
            if item.get("b64_json"):
                raw = b64_decode_loose(str(item["b64_json"]))
            elif item.get("url"):
                raw = http_download(str(item["url"]), key=os.environ.get("MEDIA_IMAGE_KEY", "").strip())
            else:
                raise MediaToolError(f"响应项既无 b64_json 也无 url: {str(item)[:200]}",
                                     code="MEDIA_BAD_RESPONSE")
            path, nbytes = save_media(raw, image_ext(raw), prompt)
            saved.append({"path": path, "bytes": nbytes})
        if not saved:
            raise MediaToolError("响应 data 中没有可用的结果项", code="MEDIA_BAD_RESPONSE")
        emit(wire_ok({
            "paths": [s["path"] for s in saved],
            "path": saved[0]["path"],  # 单数别名：单张场景直接引用
            "bytes": sum(s["bytes"] for s in saved),
            "note": "图片已落盘产物目录，在最终回答中以相对路径引用给用户（产物卡可预览）。",
        }))
    except MediaToolError as exc:
        emit(wire_err(str(exc), code=exc.code))
    except Exception as exc:  # noqa: BLE001 - 失败回喂 LLM，不中断循环
        emit(wire_err(f"生图失败: {type(exc).__name__}: {exc}"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
