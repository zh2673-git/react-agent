"""OpenAI 兼容站点预设清单（纯数据，非 provider pack——无 PROVIDER 导出，registry 自动跳过）。

「站点」= OpenAI 兼容协议 + 特定 base_url 的托管服务（ModelScope / 硅基流动 / OpenRouter 等）。
协议完全相同，差异只有 base_url + api_key + 模型列表，故不做每站点 provider pack（全是样板），
以数据驱动切换：前端「站点」下拉一键选择 → base_url / key 联动 → 保存走既有
PUT /api/config → configure op 热应用（Phase 4-1 通道，零重启零契约改动）。

清单为纯数据，按需增改本文件即可；`custom` 条目保留空 base_url 供手填任意站点。
"""

PRESETS: list[dict] = [
    {"id": "modelscope", "name": "ModelScope 魔搭", "base_url": "https://api-inference.modelscope.cn/v1", "key_url": "https://modelscope.cn/my/myaccesstoken"},
    {"id": "siliconflow", "name": "硅基流动 SiliconFlow", "base_url": "https://api.siliconflow.cn/v1", "key_url": "https://cloud.siliconflow.cn/account/ak"},
    {"id": "openrouter", "name": "OpenRouter", "base_url": "https://openrouter.ai/api/v1", "key_url": "https://openrouter.ai/keys"},
    {"id": "deepseek", "name": "DeepSeek 官方", "base_url": "https://api.deepseek.com/v1", "key_url": "https://platform.deepseek.com/api_keys"},
    {"id": "moonshot", "name": "月之暗面 Kimi", "base_url": "https://api.moonshot.cn/v1", "key_url": "https://platform.moonshot.cn/console/api-keys"},
    {"id": "dashscope", "name": "阿里百炼（兼容模式）", "base_url": "https://dashscope.aliyuncs.com/compatible-mode/v1", "key_url": "https://bailian.console.aliyun.com/?apiKey=1"},
    {"id": "zhipu", "name": "智谱 GLM", "base_url": "https://open.bigmodel.cn/api/paas/v4", "key_url": "https://open.bigmodel.cn/usercenter/apikeys"},
    {"id": "custom", "name": "自定义站点", "base_url": "", "key_url": ""},
]


def list_presets(payload: dict) -> dict:
    """presets.list op：返回站点清单（前端站点下拉数据源）。"""
    return {"ok": True, "presets": PRESETS}
