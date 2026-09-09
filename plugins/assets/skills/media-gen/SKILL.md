---
name: media-gen
description: Media generation discipline - generate images (image_gen) and videos (video_submit + video_poll two-step) via OpenAI-compatible / media endpoints configured in the settings panel (config.json media section). Use when the user asks to create/draw/generate an image or video. Returns artifact paths that MUST be referenced in the final answer. Triggers: 生图/画图/生成图片/画一张/生视频/生成视频/做视频/文生图/文生视频. Orchestrated by project-dev when the task involves media production.
tools: tools.json
origin: preset
---

# 媒体生成（生图 / 生视频）

本技能提供三件配套工具：`image_gen`（生图）、`video_submit` + `video_poll`（生视频两段式）。
模型端点与密钥来自设置面板「媒体模型」区（config.json `media` 段）——**未配置时工具会返回
引导文案，如实转告用户去设置面板配置，不要反复重试**。

工具可见性须知：本技能为出厂技能（origin=preset），**工具随装载自动启用**——调
`load_skill`（读本正文时自动装配套工具）或 `skill_install` 任一即可，下一轮
`image_gen` 等即出现在你的工具清单。若清单中没有它们：
1. 先检查会话是否已执行 `load_skill`（未装载则先装）；
2. 已装载仍不可见 → 如实告知用户「工具未启用/未装载，请重启 host 或检查设置面板技能 tab」，
   **不要误判为「媒体端点未配置」**（端点未配置的报错只会发生在工具真正执行时）。

## 1. 何时使用

- 用户要求「生成/画一张图」「生成视频」「文生图/文生视频」等明确媒体产出时使用；
- 用户只是描述想法未明确要图/视频时，先确认再调用，不要擅自生成；
- 需要示意配图辅助交付（文档插图等）也可用 `image_gen`。

## 2. 工作流

### 生图（一步）

1. 调 `image_gen`：`prompt` 必填（写具体：主体/风格/构图/光线），可选 `size` / `n`(1-4) / `model`；
2. 返回 `{"ok":true,"result":{"path","bytes","note"}}`，`path` 是产物目录下的相对路径。

### 生视频（两段式，生成需 1-10 分钟）

1. 调 `video_submit`：`prompt` 必填（主体/动作/镜头运动写具体），立即返回 `task_id`；
2. 调 `video_poll` 传 `task_id`：`succeeded` → 返回 `path`；`running` → **等 20-60 秒再查**
   （在回答中告知用户需要等待，勿连续高频轮询）；`failed` → 转述原因，可修正参数后重新提交；
3. 超时上限内仍 running → 如实告知用户任务仍在生成，给出 task_id 与稍后查询方式。

## 3. 产物引用规约（硬性）

- **最终回答必须以产物相对路径引用生成的媒体文件**（如 `outputs/20260908-153001-sunset-city.png`）——
  产物卡据此呈现，用户可点击预览（图片新标签打开、视频内联播放）；
- 不要复述 base64 或文件内容；不要把产物写进仓库根或技能目录；
- 一次生成多张（n>1）时逐一列出 `paths` 数组中的全部路径。

## 4. 约束

- 三件工具不接受任意 HTTP 端点——端点由 media 配置指定；站点差异（提交/查询 URL 形状）
  以配置适配，不在参数里传 URL；
- `image_gen` 超时 300s、`video_poll` 超时 300s（下载含在内）；视频生成中的等待靠「间隔轮询」
  而非单次长阻塞；
- 未配置（`MEDIA_NOT_CONFIGURED`）→ 停止调用，引导用户配置；配置错误（HTTP 4xx/密钥无效）
  → 把可读错误转述给用户，不要盲改参数重试超过一次。
