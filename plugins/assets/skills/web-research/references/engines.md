# 搜索引擎链速查

## 零 key 直连（国内 SEARCH_REGION=cn 默认链）

| 引擎 | 强项 | 注意 |
|---|---|---|
| bing | 中英皆可，反爬最温和 | 首选 |
| sogou | 中文内容（微信生态收录好） | 结果链接是跳转链 |
| baidu | 中文存量最大 | 反爬较强，低频调用可用 |

## global 链（海外）

| 引擎 | 说明 |
|---|---|
| ddgs | 需要 `pip install ddgs`（可选依赖） |
| duckduckgo | HTML 抓取，大陆不可直连 |
| bing | 兜底 |

## 有 key 提升（质量更稳）

| 引擎 | env | 额度 |
|---|---|---|
| bocha | BOCHA_API_KEY | 付费（DeepSeek 官方搜索引擎） |
| baidu_ai | BAIDU_API_KEY | 每日 100 次免费 |
| tavily | TAVILY_API_KEY | 每月 1000 次免费 |

## 故障排查
- 全链失败：看错误里的 tried 列表；国内环境勿设 SEARCH_BACKEND=duckduckgo；
- 需要强制单引擎：SEARCH_BACKEND=bing；
- web_read 失败：国内站点直抓天然可达，jina 失败不影响 direct 兜底。
