//! 杂项端点：会话归属校验 / presets·models 转发 / 源文件定位 / W17 多实例管理。
//! （W17 迭代七自 api.rs 按域拆出，纯搬家零逻辑改动。）
use super::super::files::tools_dir;
use super::super::{bad_request, dispatch_or_err, json_resp, parse_query};
use super::skills::valid_skill_name;
use crate::config;
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use std::path::PathBuf;
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

/// GET /api/im/sessions：只读 IM 会话元数据（网关写在 <repo>/im-gateway/data/im-sessions.json）。
/// 电脑端据此把 QQ/钉钉会话合并进左侧车道；不写任何状态，文件缺失/损坏一律空列表。
pub(crate) async fn im_sessions(stream: &mut TcpStream) -> anyhow::Result<()> {
    let path = config::config_file()
        .parent()
        .map(|p| p.join("im-gateway").join("data").join("im-sessions.json"));
    let sessions = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    json_resp(stream, 200, json!({ "ok": true, "sessions": sessions })).await
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

// ───────────────────────── 上游本地网关（magpie serve 一键启动） ─────────────────────────

/// 从 config.json `llm.base_url` 解析上游 (host, port)。仅放行回环地址——
/// 「一键启动」只对本地网关有意义，防止把外部站点当本地进程误操作。
fn upstream_hostport() -> Option<(String, u16)> {
    let base = crate::config::load_config().get("llm")?.get("base_url")?.as_str()?.to_string();
    let rest = base.strip_prefix("http://").or_else(|| base.strip_prefix("https://"))?;
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), if base.starts_with("https") { 443 } else { 80 }),
    };
    let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]");
    loopback.then_some((host, port))
}

/// GET /api/upstream/status：转发 llm-adapter `upstream.status`
/// （探活 {base_url}/models + magpie 扩展 /magpie/quotas、/magpie/usage）。
pub(crate) async fn upstream_status(stream: &mut TcpStream, kernel: &Kernel) -> anyhow::Result<()> {
    match dispatch_or_err(kernel, "llm-adapter", json!({"op": "upstream.status"})).await {
        Ok(v) => json_resp(stream, 200, v).await,
        Err(e) => json_resp(stream, 502, e).await,
    }
}

/// 探测 127.0.0.1:port 是否有服务监听（500ms 超时）。
async fn port_up(port: u16) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// 定位 magpie CLI：MAGPIE_EXE env → %USERPROFILE%\.local\bin\magpie.exe → PATH（where/which）。
fn find_magpie_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MAGPIE_EXE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(&profile).join(".local").join("bin").join("magpie.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    let (prog, arg) = if cfg!(target_os = "windows") { ("where.exe", "magpie") } else { ("which", "magpie") };
    std::process::Command::new(prog)
        .arg(arg)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| PathBuf::from(l.trim())))
        .filter(|p| p.is_file())
}

/// POST /api/upstream/start：一键启动本地网关（magpie serve，幂等）。
/// 已监听 → 直接返回；未监听 → 定位 magpie.exe → spawn（stdout/stderr 追加到
/// config 同目录 .magpie-serve.log）→ 轮询 10s 探活。
pub(crate) async fn upstream_start(stream: &mut TcpStream) -> anyhow::Result<()> {
    let Some((_host, port)) = upstream_hostport() else {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": "当前 llm.base_url 不是本机回环地址，无法一键启动本地网关（请先在站点预设选择 magpie 并保存）"}}),
        )
        .await;
    };
    if port_up(port).await {
        return json_resp(stream, 200, json!({"ok": true, "started": false, "up": true, "port": port})).await;
    }
    let Some(exe) = tokio::task::spawn_blocking(find_magpie_exe).await.unwrap_or(None) else {
        return json_resp(
            stream,
            404,
            json!({"ok": false, "error": {"code": "K404", "message": "未找到 magpie CLI（可设 MAGPIE_EXE env，或装到 %USERPROFILE%\\.local\\bin\\magpie.exe）"}}),
        )
        .await;
    };
    let log_path = crate::config::config_file()
        .parent()
        .map(|p| p.join(".magpie-serve.log"));
    let open_log = || {
        log_path
            .as_ref()
            .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok())
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["serve"]).stdin(std::process::Stdio::null());
    match (open_log(), open_log()) {
        (Some(o), Some(e)) => {
            cmd.stdout(std::process::Stdio::from(o));
            cmd.stderr(std::process::Stdio::from(e));
        }
        _ => {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    if let Err(e) = cmd.spawn() {
        return json_resp(
            stream,
            500,
            json!({"ok": false, "error": {"code": "K500", "message": format!("magpie serve 启动失败: {e}")}}),
        )
        .await;
    }
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if port_up(port).await {
            return json_resp(stream, 200, json!({"ok": true, "started": true, "up": true, "port": port})).await;
        }
    }
    json_resp(stream, 200, json!({"ok": true, "started": true, "up": false, "port": port})).await
}


// ───────────────────── 账号签到（magpie accounts login/checkin/list） ─────────────────────

/// 解析查询串里 url 编码的 provider 参数：**只做形状校验**（非空、仅 `[a-z0-9-]{1,64}`）。
/// provider 是否存在、能不能登录，一律由 `magpie plugin list --json` 动态判定（P3 动态化）。
/// 为什么不保留固定白名单：渠道是插件能力，装了什么就有什么；写死在 Host 里必然与用户
/// 机器上的实际插件漂移（曾出现 minimax 登录入口在插件里根本不存在）。
fn checkin_provider(query: &str) -> Option<String> {
    let v = parse_query(query).get("provider").cloned().unwrap_or_default();
    if v.is_empty() || v.len() > 64 {
        return None;
    }
    if !v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return None;
    }
    Some(v)
}

/// Windows 下隐藏 CLI 子进程窗口（与 upstream_start 同款）。
fn checkin_spawn(command: &mut std::process::Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = command;
}

/// 读 `magpie plugin list --json` 的 `providers` 数组——登录渠道与可用 method 的**唯一事实源**。
/// 为什么不写死白名单：渠道是插件能力，用户装了什么就有什么；写死在 Host 里必然与机器实际
/// 插件漂移（曾出现「MiniMax 登录入口」在插件里根本不存在）。解析走 serde_json，不用
/// PowerShell ConvertFrom-Json（中文 labels 会触发 `Invalid object passed in`）。
fn plugin_providers(exe: &std::path::Path) -> Option<Value> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["plugin", "list", "--json"]).stdin(std::process::Stdio::null());
    checkin_spawn(&mut cmd);
    let o = cmd.output().ok()?;
    let v: Value = serde_json::from_slice(&o.stdout).ok()?;
    v.get("providers").cloned()
}

/// 判断一个 method 能否被 Host 子进程自动完成：Host 的 stdin=null，凡是需要用户输入值的
/// method（带 `placeholder` 的 api-key 型，或 label 里写明「输入…」的企业域名型）一打就
/// EOF/挂死，只能留给终端。返回 true = 可自动完成。
fn method_usable(m: &Value) -> bool {
    let label = m.get("label").and_then(Value::as_str).unwrap_or("").trim();
    !label.is_empty() && m.get("placeholder").is_none() && !label.contains("输入")
}

/// 从 provider 的 `methods[]` 里挑一个可自动完成的 method（偏好浏览器 OAuth，其次复用本机
/// CLI/IDE 登录态，最后取第一个可用的）。
fn usable_method(p: &Value) -> Option<String> {
    let ms = p.get("methods")?.as_array()?;
    let cands: Vec<String> = ms
        .iter()
        .filter(|m| method_usable(m))
        .filter_map(|m| m.get("label").and_then(Value::as_str).map(|s| s.trim().to_string()))
        .collect();
    if let Some(l) = cands.iter().find(|l| l.contains("browser")) {
        return Some(l.clone());
    }
    if let Some(l) = cands.iter().find(|l| l.contains("CLI's or")) {
        return Some(l.clone());
    }
    cands.into_iter().next()
}

/// POST /api/upstream/login?provider=workbuddy：弹 `magpie` 登录窗（CLI 子进程跑
/// `magpie plugin login <provider> <method>`，GUI 完成 OTP 后子进程自然退出）。请求会阻塞到
/// 登录窗关闭——前端按钮自行 disable + 等待；宿主每连接独立 spawn，不会卡其它请求。
pub(crate) async fn upstream_login(stream: &mut TcpStream, query: &str) -> anyhow::Result<()> {
    let Some(provider) = checkin_provider(query) else {
        return json_resp(stream, 400, json!({"ok": false, "error": {"code": "K400", "message": "缺少或非法 provider（形如 trae-cn / workbuddy；可用清单见 GET /api/upstream/providers）"}})).await;
    };
    let exe = tokio::task::spawn_blocking(find_magpie_exe).await.unwrap_or(None);
    let Some(exe) = exe else {
        return json_resp(stream, 404, json!({"ok": false, "error": {"code": "K404", "message": "未找到 magpie CLI（可设 MAGPIE_EXE env，或装到 %USERPROFILE%\\.local\\bin\\magpie.exe）"}})).await;
    };
    // 先问 CLI 现有哪些渠道、目标渠道有哪些 method——provider 是否存在只能这样判定：
    // `magpie plugin login nosuchprovider` 可能退出 0，凭退出码判断会放行不存在的渠道。
    let probe_exe = exe.clone();
    let probe_pid = provider.clone();
    let probe = tokio::task::spawn_blocking(move || -> Result<String, (u16, Value)> {
        let Some(ps) = plugin_providers(&probe_exe) else {
            return Err((502, json!({"ok": false, "error": {"code": "K502", "message": "读取 magpie plugin list --json 失败，无法确认登录渠道"}})));
        };
        let arr = ps.as_array().cloned().unwrap_or_default();
        let ids: Vec<String> = arr
            .iter()
            .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let Some(p) = arr.iter().find(|p| p.get("id").and_then(Value::as_str) == Some(probe_pid.as_str())) else {
            return Err((
                400,
                json!({"ok": false, "error": {"code": "K400", "message": format!("未安装的 provider: {probe_pid}（当前可用: {}）", ids.join(" / "))}}),
            ));
        };
        match usable_method(p) {
            Some(m) => Ok(m),
            None => Err((
                400,
                json!({"ok": false, "error": {"code": "K400", "message": format!("{probe_pid} 没有能在面板里自动完成的登录方式（需要交互输入或企业域名），请在终端执行: magpie plugin login {probe_pid}")}}),
            )),
        }
    })
    .await;
    let method = match probe {
        Ok(Ok(m)) => m,
        Ok(Err((status, body))) => return json_resp(stream, status, body).await,
        Err(_) => {
            return json_resp(stream, 500, json!({"ok": false, "error": {"code": "K500", "message": "magpie CLI 执行失败"}})).await
        }
    };
    let pid = provider.clone();
    let r = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["plugin", "login", provider.as_str(), method.as_str()])
            .stdin(std::process::Stdio::null());
        checkin_spawn(&mut cmd);
        cmd.output()
    })
    .await;
    match r {
        Ok(Ok(o)) if o.status.success() => {
            json_resp(stream, 200, json!({"ok": true, "provider": pid})).await
        }
        Ok(Ok(o)) => {
            let msg = String::from_utf8_lossy(&o.stderr);
            let msg = msg.lines().next().unwrap_or("CLI 退出非 0").to_string();
            json_resp(stream, 502, json!({"ok": false, "error": {"code": "K502", "message": format!("{pid} 登录未完成: {msg}")}})).await
        }
        _ => json_resp(stream, 500, json!({"ok": false, "error": {"code": "K500", "message": "magpie CLI 执行失败"}})).await,
    }
}

/// GET /api/upstream/providers：登录渠道清单（动态，来自 `magpie plugin list --json`）。
/// 每个 provider 给出 id / name / usable / methods[{label,usable}]，前端据此渲染登录按钮——
/// 装了什么插件就有什么按钮，MiniMax 这类不存在的渠道不会再出现。
pub(crate) async fn upstream_providers(stream: &mut TcpStream) -> anyhow::Result<()> {
    let exe = tokio::task::spawn_blocking(find_magpie_exe).await.unwrap_or(None);
    let Some(exe) = exe else {
        return json_resp(stream, 404, json!({"ok": false, "error": {"code": "K404", "message": "未找到 magpie CLI"}})).await;
    };
    let r = tokio::task::spawn_blocking(move || plugin_providers(&exe)).await;
    let Some(ps) = r.ok().flatten() else {
        return json_resp(stream, 502, json!({"ok": false, "error": {"code": "K502", "message": "读取 magpie plugin list --json 失败"}})).await;
    };
    let mut list: Vec<Value> = Vec::new();
    for p in ps.as_array().cloned().unwrap_or_default() {
        let id = p.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| id.clone());
        let mut methods: Vec<Value> = Vec::new();
        for m in p.get("methods").and_then(Value::as_array).cloned().unwrap_or_default() {
            let label = m.get("label").and_then(Value::as_str).unwrap_or("").trim().to_string();
            methods.push(json!({"label": label, "usable": method_usable(&m)}));
        }
        let usable = methods.iter().any(|m| m.get("usable").and_then(Value::as_bool).unwrap_or(false));
        list.push(json!({"id": id, "name": name, "usable": usable, "methods": methods}));
    }
    json_resp(stream, 200, json!({"ok": true, "providers": list})).await
}

/// POST /api/upstream/checkin：全量签到（跑 `magpie accounts checkin`，幂等），返回可读结果行。
pub(crate) async fn upstream_checkin(stream: &mut TcpStream) -> anyhow::Result<()> {
    let exe = tokio::task::spawn_blocking(find_magpie_exe).await.unwrap_or(None);
    let Some(exe) = exe else {
        return json_resp(stream, 404, json!({"ok": false, "error": {"code": "K404", "message": "未找到 magpie CLI"}})).await;
    };
    let r = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["accounts", "checkin"]).stdin(std::process::Stdio::null());
        checkin_spawn(&mut cmd);
        cmd.output()
    })
    .await;
    match r {
        Ok(Ok(o)) => {
            let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            let lines: Vec<String> = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            json_resp(stream, 200, json!({"ok": o.status.success(), "lines": lines})).await
        }
        _ => json_resp(stream, 500, json!({"ok": false, "error": {"code": "K500", "message": "magpie CLI 执行失败"}})).await,
    }
}

/// GET /api/upstream/accounts：转发 magpie accounts list --json（账号清单，供前端显示账号数）。
pub(crate) async fn upstream_accounts(stream: &mut TcpStream) -> anyhow::Result<()> {
    let exe = tokio::task::spawn_blocking(find_magpie_exe).await.unwrap_or(None);
    let Some(exe) = exe else {
        return json_resp(stream, 404, json!({"ok": false, "error": {"code": "K404", "message": "未找到 magpie CLI"}})).await;
    };
    let r = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["accounts", "--json"]).stdin(std::process::Stdio::null());
        checkin_spawn(&mut cmd);
        cmd.output()
    })
    .await;
    let Ok(Ok(o)) = r else {
        return json_resp(stream, 500, json!({"ok": false, "error": {"code": "K500", "message": "magpie CLI 执行失败"}})).await;
    };
    let v: Value = serde_json::from_slice(&o.stdout).unwrap_or(json!([]));
    json_resp(stream, 200, json!({"ok": true, "accounts": v})).await
}

// ── 自更新（SELF-UPDATE-3）：交接换血 ────────────────────────────────────────
// Windows 约束：运行中 exe 镜像被锁（不能边跑边替换），无 SO_REUSEPORT（不能共享端口）。
// 因此：构建进隔离目录（产物路径不同于运行中 exe）；交接用「等待-换绑」——新进程
// RSI_HANDOVER=1 反复重试 bind（见 web/mod.rs），旧进程确认其就绪后排水退出。
// fail-safe：任何一步失败 = 旧进程继续服务；日志落 %TEMP%/rsi-selfupdate-<port>.log。

/// 当前运行体构建时间（编译期注入，作为版本指纹与仓库源码时间对比）。
fn self_build_time() -> Option<std::time::SystemTime> {
    let exe = std::env::current_exe().ok()?;
    std::fs::metadata(exe).and_then(|m| m.modified()).ok()
}

/// 运行时 Cargo 工程根——布局自适应：rsi-agent 内嵌在 `<repo>/runtime/`，react-agent
/// 就是 `<repo>`。自更新三处路径（源码 mtime / 隔离构建目录 / cargo cwd）统一经此，
/// 两仓同一份代码。判据用 `Cargo.toml` 而非目录存在：前者说明「这是工程根」。
fn runtime_root(repo: &std::path::Path) -> std::path::PathBuf {
    let embedded = repo.join("runtime");
    if embedded.join("Cargo.toml").is_file() {
        embedded
    } else {
        repo.to_path_buf()
    }
}

/// GET /api/self/update：版本指纹 + 交接状态（供前端/调用方判断是否需要换血）。
pub(crate) async fn self_update_status(stream: &mut TcpStream) -> anyhow::Result<()> {
    let handover = std::env::var("RSI_HANDOVER").map(|v| v == "1").unwrap_or(false);
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let repo = std::path::PathBuf::from(crate::config::workspace_root());
    let src_mtime = std::fs::metadata(runtime_root(&repo).join("crates/host/src"))
        .and_then(|m| m.modified())
        .ok()
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    let exe_mtime = self_build_time()
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    // 源码比运行体新 = 仓库里有尚未生效的变更
    let stale = match (src_mtime, exe_mtime) {
        (Some(s), Some(e)) => s > e,
        _ => false,
    };
    json_resp(
        stream,
        200,
        json!({
            "ok": true,
            "exe": exe,
            "exe_mtime": exe_mtime,
            "src_mtime": src_mtime,
            "stale": stale,
            "handover_waiting": handover,
        }),
    )
    .await
}

/// POST /api/self/update：隔离构建 → 孵化新进程 → 等就绪 → 排水退出。立即回 202。
pub(crate) async fn self_update_start(stream: &mut TcpStream) -> anyhow::Result<()> {
    let addr = std::env::var("WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8710".into());
    let port = addr.rsplit(':').next().unwrap_or("8710").to_string();
    let repo = std::path::PathBuf::from(crate::config::workspace_root());
    let log_path = std::env::temp_dir().join(format!("rsi-selfupdate-{port}.log"));
    let log_display = log_path.display().to_string();
    let stamp = { let lp = log_path.clone(); move |msg: &str| stamp_line(&lp, msg) };
    stamp("POST /api/self/update 收到，开始自更新流程");
    tracing::info!("self-update：构建 + 交接开始（日志 {}）", log_display);

    // 后台执行；本请求立即返回 202（浏览器面板随后会经历一次 SSE 断线自动重连）。
    tokio::spawn(async move {
        // 1. 隔离构建：CARGO_TARGET_DIR 指 target-selfupdate，产物与运行中 exe 不同路径，
        //    避开 Windows 运行中镜像锁；同时防止动到开发中 target 目录的构建锁。
        let rt = runtime_root(&repo);
        let out = rt.join("target-selfupdate");
        stamp("step1 隔离构建开始（cargo build --release，首次可能数分钟）");
        let build = tokio::process::Command::new("cargo")
            .args(["build", "--release", "-p", "react-agent-host"])
            .current_dir(&rt)
            .env("CARGO_TARGET_DIR", &out)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;
        let Ok(b) = build else {
            stamp("ABORT cargo 无法启动（未安装或 PATH 不含 cargo）");
            tracing::error!("self-update：cargo 启动失败，放弃（旧进程继续服务）");
            return;
        };
        if !b.status.success() {
            let tail: String = String::from_utf8_lossy(&b.stderr).lines().rev().take(15).collect::<Vec<_>>().join("\\n");
            stamp(&format!("ABORT 构建失败（保留旧版本继续服务）：{tail}"));
            tracing::error!("self-update：构建失败，放弃（旧进程继续服务）");
            return;
        }
        let new_exe = out.join("release/react-agent-host.exe");
        if !new_exe.is_file() {
            stamp("ABORT 构建产物不存在");
            return;
        }
        stamp("step1 隔离构建完成");

        // 2. 孵化新进程：HANDOVER=1（等待-换绑）+ 就绪 marker（写盘时间戳，供旧进程确认）。
        let marker = std::env::temp_dir().join(format!("rsi-handover-ready-{port}.flag"));
        let _ = std::fs::remove_file(&marker);
        stamp("step2 孵化新进程（RSI_HANDOVER=1，等待-换绑）");
        let child = tokio::process::Command::new(&new_exe)
            .env("RSI_HANDOVER", "1")
            .env("RSI_HANDOVER_MARKER", &marker)
            .env("REACT_FRONTEND", "web")
            .env("WEB_ADDR", &addr)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                stamp(&format!("ABORT 新进程孵化失败：{e}"));
                tracing::error!("self-update：新进程孵化失败，放弃（旧进程继续服务）");
                return;
            }
        };

        // 3. 等就绪：marker 出现 = 新进程完成装配、正在抢端口；60s 超时则放弃并回收新进程。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let ready = loop {
            if marker.is_file() {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            if let Ok(Some(_st)) = child.try_wait() {
                // 新进程提前退出 = 它自己交接着失败了（如 90s 抢不到端口），旧进程继续。
                break false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        };
        if !ready {
            stamp("ABORT 新进程 60s 未就绪或提前退出，回收并放弃（旧进程继续服务）");
            let _ = child.kill().await;
            let _ = std::fs::remove_file(&marker);
            return;
        }
        stamp("step3 新进程就绪（装配完成，正在接管端口）；排水 2s 后退出");
        // 4. 排水：停 2s 让在途 HTTP 应答收尾（会话/事件均有持久层与断线续传，不丢状态），
        //    然后退出——端口随即释放，新进程的下一次 bind 重试即成功（commit point）。
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        stamp("step4 旧进程退出，交出端口");
        // P1b 自更新看门狗（接力进程）：上面这跳 exit(0) 若因外部原因未遂（镜像句柄未
        // 释放 / 收尾挂起 / 崩溃），换血失败且没有任何进程收拾残局。此处孵化一个与宿主
        // 完全解耦的普通线程进程——宿主死活不影响它：marker 被旧进程清掉（正常交接成功）
        // 即自杀；25s 仍在则判定旧进程收尾失败，以 RSI_SELF_UPDATE_RELAUNCH=1 重拉新
        // 二进制（无 HANDOVER，正常装配 + 90s bind 指数退避，由 web::run_inner 兜底）。
        // 全程失败不报警不二次重启：旧进程可能仍在服务（fail-safe 语义不变），仅留日志。
        let handover_marker = std::env::temp_dir().join(format!(
            "rsi-handover-ready-{}.flag",
            addr.rsplit(':').next().unwrap_or("8710")
        ));
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
            loop {
                if !std::path::Path::new(&handover_marker).is_file() {
                    // marker 清理 = 旧进程走完 run_inner 收尾（交接正常完成）
                    std::process::exit(0);
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(_) => std::process::exit(0),
            };
            stamp("watchdog 旧进程 25s 未退场（marker 未清），接力重拉新二进制");
            let child = std::process::Command::new(&exe)
                .env("RSI_SELF_UPDATE_RELAUNCH", "1")
                .env("REACT_FRONTEND", "web")
                .env("WEB_ADDR", &addr)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match child {
                Ok(mut c) => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                    loop {
                        if std::path::Path::new(&handover_marker).is_file() {
                            break; // 重拉进程已装配完成并接管端口，看门狗功成身退
                        }
                        if std::time::Instant::now() >= deadline {
                            stamp("watchdog 重拉进程 120s 未就绪，放弃（需人工核对）");
                            let _ = c.kill();
                            break;
                        }
                        if let Ok(Some(st)) = c.try_wait() {
                            stamp(&format!("watchdog 重拉进程提前退出（{st}），放弃"));
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
                Err(e) => stamp(&format!("watchdog 重拉失败：{e}（旧进程可能仍在服务，请人工核对）")),
            }
            std::process::exit(0);
        });
        std::process::exit(0);
    });

    json_resp(stream, 202, json!({"ok": true, "accepted": true, "log": log_display})).await
}

fn stamp_line(log_path: &std::path::Path, msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "[{}] {msg}", chrono_now());
    }
}

fn chrono_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}
