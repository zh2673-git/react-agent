---
name: web-research
description: Guide for researching topics on the web with web_search/web_read - multi-source cross-checking, CN/global engine selection, and citation discipline. No extra runtimes required.
---

# Web Research

按以下流程用工具做网络调研：

## 1. 搜索策略
- 用 `web_search` 搜索，中文查询默认走国内引擎链（Bing→搜狗→百度，零 key 直连）；
- 首轮用宽泛查询确定术语，后续用精确术语 + 限定词收敛；
- 同一主题至少换 2 种查询词表述；单引擎连续失败时不要重试同一查询，检查错误信息中的 hint。

## 2. 读取与交叉验证
- 对有价值的结果用 `web_read` 读取正文（结果必带来源 URL）；
- 关键结论至少 2 个独立来源交叉验证；冲突时优先权威域名（官方文档 > 知名社区 > 转载）；
- 搜索结果摘要与正文不符时以正文为准。

## 3. 引用纪律
- 最终回答中每个关键事实标注来源 URL；
- 无法验证的信息明确标注「未验证」；
- 引擎失败信息中的 `tried` 列表可帮助判断网络环境问题。

## References
- `references/engines.md` — 各引擎特性与适用场景（用 read_file 读取）
