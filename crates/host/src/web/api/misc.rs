//! 杂项端点：会话归属校验 / presets·models 转发 / 源文件定位 / W17 多实例管理。
//! （W17 迭代七自 api.rs 按域拆出，纯搬家零逻辑改动。）
use super::super::files::tools_dir;
use super::super::{bad_request, dispatch_or_err, json_resp, parse_query};
use super::skills::valid_skill_name;
use crate::config;
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use tokio::net::TcpStream;

/// GET /api/session-exists?session=：会话归属校验（W17 迭代六，前端存量列表迁移用）。
/// 会话列表存浏览器 localStorage（按端口=源隔离），而多实例端口动态分配可被不同工作区
/// 的实例复用——列表会跨工作区串显（用户实测）。前端换工作区命名空间后，一次性把旧列表
/// 逐个经此端点校验：仅收编**当前实例 traces 里真实存在**的会话，其余属于别的工作区实例。
/// traces 文件名 safe 规则与 memory 插件同口径（`[^A-Za-z0-9._-]` → `_`）。
pub(crate) async fn session_exists(stream: &mut TcpStream, query: &str) -> anyhow::Result<()> {
    let sid = parse_query(query).get("session").cloned().unwrap_or_default();
    if sid.is_empty() || sid.len() > 128 {
        return json_resp(stream, 400, bad_request("session 参数非法", Some("session"))).await;
    }
    let safe: String = sid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    let exists = super::super::files::undo_dir()
        .parent()
        .map(|p| p.join("traces").join(format!("{safe}.jsonl")))
        .map(|p| p.is_file())
        .unwrap_or(false);
    json_resp(stream, 200, json!({ "ok": true, "exists": exists })).await
}

/// GET /api/presets：转发 llm-adapter `presets.list`（OpenAI 兼容站点预设清单，
/// 数据源 plugins/llm_adapter/presets.py——切换站点 = configure 热应用，零重启）。
pub(crate) async fn get_presets(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
    match dispatch_or_err(kernel, "llm-adapter", json!({"op": "presets.list"})).await {
        Ok(v) => {
            let presets = v.get("presets").cloned().unwrap_or_else(|| json!([]));
            json_resp(stream, 200, json!({ "ok": true, "presets": presets })).await
        }
        Err(e) => json_resp(stream, 502, e).await,
    }
}

/// GET /api/models：转发 llm-adapter `models.list`，返回该 provider 当前可用模型 id 列表
/// （openai/deepseek 走 `/v1/models`；ollama 走 `/api/tags` + 逐模型 `/api/show` 探测原生窗口；
/// anthropic/mock 走静态清单）。models_meta 为可选扩展（原生窗口 ctx_limit），存在才透传。
pub(crate) async fn get_models(stream: &mut TcpStream, kernel: &Kernel, query: &str) -> anyhow::Result<()> {
    // 前端可经 ?provider= 覆盖（未保存设置时也能拉对应 provider 的模型清单）
    let provider = parse_query(query).get("provider").cloned().unwrap_or_default();
    let mut op = json!({"op": "models.list"});
    if !provider.is_empty() {
        op["provider"] = json!(provider);
    }
    match dispatch_or_err(kernel, "llm-adapter", op).await {
        Ok(v) => {
            let models = v.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut out = json!({ "ok": true, "models": models });
            if let Some(meta) = v.get("models_meta") {
                out["models_meta"] = meta.clone();
            }
            json_resp(stream, 200, out).await
        }
        Err(e) => json_resp(stream, 502, e).await,
    }
}

/// POST /api/reveal?target=config|tools|skills|skill&name=<name>：在系统文件管理器中
/// 打开配置源文件所在位置（浏览器 http 页面无法跳 file:// 链接，由 host 代为打开）。
/// 只允许白名单目标——config.json / plugins/tools / skills 根 / 具名技能目录
/// （名字约束杜绝路径注入）。explorer 退出码无意义，spawn 后不等待。
pub(crate) async fn reveal_target(stream: &mut TcpStream, query: &str) -> anyhow::Result<()> {
    let q = parse_query(query);
    let target = q.get("target").map(String::as_str).unwrap_or("");
    let name = q.get("name").map(String::as_str).unwrap_or("");
    let (path, select) = match target {
        "config" => (config::config_file(), true),
        "tools" => (tools_dir(), false),
        "skills" => (config::skills_dir(), false),
        "skill" => {
            if !valid_skill_name(name) {
                return json_resp(
                    stream,
                    400,
                    bad_request("非法技能名（仅字母数字/_/-，≤64 字符）", Some("name")),
                )
                .await;
            }
            let dir = config::skills_dir().join(name);
            if !dir.is_dir() {
                return json_resp(
                    stream,
                    404,
                    json!({"ok": false, "error": {"code": "K404", "message": format!("技能不存在: {name}")}}),
                )
                .await;
            }
            (dir.join("SKILL.md"), true)
        }
        _ => {
            return json_resp(
                stream,
                400,
                bad_request("target 须为 config | tools | skills | skill（后者附 name）", Some("target")),
            )
            .await
        }
    };
    if !path.exists() {
        return json_resp(
            stream,
            404,
            json!({"ok": false, "error": {"code": "K404", "message": format!("路径不存在: {}", path.display())}}),
        )
        .await;
    }
    let spawned: std::io::Result<()> = if cfg!(target_os = "windows") {
        #[cfg(target_os = "windows")]
        {
            // /select,"<file>" = 打开所在文件夹并选中文件（raw_arg 保引号原样传递，
            // 规避含空格路径被二次引号包裹的 explorer 解析怪癖）；目录直接打开
            use std::os::windows::process::CommandExt;
            let arg = if select {
                format!("/select,\"{}\"", path.display())
            } else {
                format!("\"{}\"", path.display())
            };
            std::process::Command::new("explorer").raw_arg(arg).spawn().map(|_| ())
        }
        #[cfg(not(target_os = "windows"))]
        {
            unreachable!()
        }
    } else {
        let _ = select;
        let prog = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(prog).arg(&path).spawn().map(|_| ())
    };
    match spawned {
        Ok(()) => json_resp(stream, 200, json!({"ok": true, "path": path.display().to_string()})).await,
        Err(e) => json_resp(
            stream,
            500,
            json!({"ok": false, "error": {"code": "K500", "message": format!("打开失败: {e}")}}),
        )
        .await,
    }
}

// ───────────────────────── W17 多窗口多实例 ─────────────────────────

/// POST /api/instances：{"name","workspace"} → 建实例目录 + spawn 自身 → 等就绪 → {port,url}。
/// 同步实现含就绪等待（最多 ~15s），放 spawn_blocking 避免阻塞 tokio worker。
pub(crate) async fn create_instance(stream: &mut TcpStream, body: &[u8]) -> anyhow::Result<()> {
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        return json_resp(stream, 400, bad_request("body 非法 JSON", None)).await;
    };
    // name 可选：缺省自动取工作区文件夹名（同名目录已存在 → 同工作区复用/复活，异工作区加后缀）
    let name = req.get("name").and_then(Value::as_str).map(str::to_string);
    let Some(workspace) = req.get("workspace").and_then(Value::as_str) else {
        return json_resp(stream, 400, bad_request("缺 workspace（需存在的目录绝对路径）", Some("workspace"))).await;
    };
    let workspace = workspace.to_string();
    let result = tokio::task::spawn_blocking(move || crate::instances::create_instance(name.as_deref(), &workspace))
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking 失败: {e}"))?;
    match result {
        Ok(v) => json_resp(stream, 200, v).await,
        Err(e) => json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": e}}),
        )
        .await,
    }
}

/// POST /api/pick-folder：弹原生文件夹选择对话框（host 所在机器），返回所选绝对路径。
/// 取消/无选择 → K400「未选择文件夹」。
pub(crate) async fn pick_folder(stream: &mut TcpStream) -> anyhow::Result<()> {
    let result = tokio::task::spawn_blocking(crate::instances::pick_folder)
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking 失败: {e}"))?;
    match result {
        Ok(path) => json_resp(stream, 200, json!({"ok": true, "path": path})).await,
        Err(e) => json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": e}}),
        )
        .await,
    }
}

/// GET /api/instances：实例清单（扫 .instances/ + TCP 探活标注 online/offline）。
pub(crate) async fn list_instances(stream: &mut TcpStream) -> anyhow::Result<()> {
    let list = tokio::task::spawn_blocking(crate::instances::list_instances)
        .await
        .unwrap_or_default();
    json_resp(stream, 200, json!({"ok": true, "instances": list})).await
}

/// DELETE /api/instances/{name}：杀实例进程（数据目录保留，同名 POST 可复活）。
pub(crate) async fn delete_instance(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
    let name = name.to_string();
    let result = tokio::task::spawn_blocking(move || crate::instances::stop_instance(&name))
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking 失败: {e}"))?;
    match result {
        Ok(v) => json_resp(stream, 200, v).await,
        Err(e) => json_resp(
            stream,
            404,
            json!({"ok": false, "error": {"code": "K404", "message": e}}),
        )
        .await,
    }
}
