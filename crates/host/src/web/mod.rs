//! Web 前端（W10 自 frontend.rs 拆分）：dsh 风格 web 网关——极简手写 HTTP
//! （tokio TcpListener，不引 web 框架依赖）。
//!
//! 子模块：
//! - [`gateway`]：SSE 事件流（trace 重放 + 流式旁路 + 子代理旁路）
//! - [`api`]：配置中心 / 技能 CRUD / models / presets / 附件校验
//! - [`files`]：`/files` 工作区文件服务
//!
//! host 是组合根：前端是**入口组件**而非 guest 能力——网关需调 `agent.chat` +
//! `session.trace`，而 guest 不可互调（内核物理约束）；前端切换必伴随重启。
//! 静态单页 `web-dist/`（index.html + style.css + app.js）运行时 serve，改样式刷新即生效。
mod api;
mod files;
mod gateway;

use crate::frontend::Frontend;
use agent_kernel_sdk::{Envelope, PluginId};
use agent_kernel_kernel::Kernel;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Cursor 暖色系「事件流式会话」前端。
///
/// 路由：
///   GET  / /index.html /style.css /app.js → web-dist/ 静态文件（白名单，运行时读取）
///   GET  /api/events?session=&after= → SSE：增量轮询 memory trace.read，逐事件推送
///   POST /api/chat              → {"session_id","message","attachments"?} → agent.chat（阻塞到收敛）
///   POST /api/chat/cancel?session= → agent-loop cancel + llm-adapter abort（取消，立即返回）
///   POST /api/chat/rollback     → {"session_id","upto_user_index"} → memory rollback（R2 回滚）
///   GET  /api/presets           → llm-adapter presets.list（OpenAI 兼容站点预设清单）
pub struct WebFrontend {
    addr: String,
}

impl WebFrontend {
    pub fn from_env() -> Self {
        Self {
            addr: std::env::var("WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8710".into()),
        }
    }

    async fn run_inner(&self, kernel: Arc<Kernel>) -> anyhow::Result<()> {
        // 自更新交接（SELF-UPDATE-1）：RSI_HANDOVER=1 = 新进程等待旧进程退出后再 bind。
        // Windows 无 SO_REUSEPORT、运行中 exe 镜像被锁，等待-换绑（nginx USR2 的变体）是
        // 唯一安全形态；亚秒级空窗由前端 SSE 断线 1s 防抖 + after 游标续传覆盖，浏览器无感。
        if std::env::var("RSI_HANDOVER").map(|v| v == "1").unwrap_or(false) {
            // 就绪 marker：能走到这里说明内核+插件装配已完成，写盘告知旧进程可以开始排水。
            let marker = std::env::var("RSI_HANDOVER_MARKER").ok();
            if let Some(m) = &marker {
                let _ = std::fs::write(m, std::process::id().to_string());
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
            let mut wait = std::time::Duration::from_millis(200);
            let listener = loop {
                match TcpListener::bind(&self.addr).await {
                    Ok(l) => break l,
                    Err(e) if tokio::time::Instant::now() < deadline => {
                        tracing::info!("self-update 等待旧进程释放 {}（{e}），{:?} 后重试", self.addr, wait);
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(std::time::Duration::from_secs(2));
                    }
                    Err(e) => {
                        tracing::error!("self-update 交接失败：90s 内未拿到 {}（{e}）。请手动重启。", self.addr);
                        return Err(e.into());
                    }
                }
            };
            // 接管成功 = commit point。清交接态：本次进程后续可再次触发自更新；marker 文件一并清理。
            std::env::remove_var("RSI_HANDOVER");
            std::env::remove_var("RSI_HANDOVER_MARKER");
            if let Some(m) = marker {
                let _ = std::fs::remove_file(&m);
            }
            tracing::info!("self-update 交接完成：已在 {} 接管（旧进程已退出）", self.addr);
            return Self::serve_listener(listener, kernel).await;
        }
        // P1b 接力启动（自更新看门狗拉起）：完整环境继承自旧进程但无 RSI_HANDOVER 握手
        // ——说明旧进程没有走完交接收尾，由看门狗兜底重拉。显式示警而非静默换血（否则
        // 「这次启动为何没有交接日志」无法追溯）；清理标记后按正常启动继续，行为无差别。
        if std::env::var("RSI_SELF_UPDATE_RELAUNCH").map(|v| v == "1").unwrap_or(false) {
            std::env::remove_var("RSI_SELF_UPDATE_RELAUNCH");
            tracing::warn!(
                "RSI 启动示警：本进程由自更新接力看门狗拉起（上一次交接未正常收尾）。会话与配置经持久层延续；如状态异常请核对新进程日志。"
            );
        }
        let listener = TcpListener::bind(&self.addr).await?;
        Self::serve_listener(listener, kernel).await
    }

    /// 伺服循环（绑定与伺服分离：测试用 port 0 绑定后取 local_addr）。
    pub async fn serve_listener(listener: TcpListener, kernel: Arc<Kernel>) -> anyhow::Result<()> {
        let local = listener.local_addr()?;
        tracing::info!("web 网关就绪: http://{local}（事件流式会话，SSE 实时 + 刷新重放恢复）");
        loop {
            let (stream, _peer) = listener.accept().await?;
            let kernel = kernel.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, kernel).await {
                    tracing::debug!("web 连接处理结束: {e}");
                }
            });
        }
    }
}

#[async_trait::async_trait]
impl Frontend for WebFrontend {
    async fn run(&self, kernel: Arc<Kernel>, _session: String) -> anyhow::Result<()> {
        self.run_inner(kernel).await
    }
}

// ── HTTP 最小实现 ──

/// 定位 web 前端静态文件（白名单 + vendor 前缀规则，杜绝目录穿越），按优先级尝试：
/// 1. 编译期注入的源码目录（`CARGO_MANIFEST_DIR`，`cargo run`/`build` 时有效）；
/// 2. 相对 cwd 的 `crates/host/web-dist/`（在 workspace 根启动）；
/// 3. 相对二进制同级的 `web-dist/`（打包发布）。
///
/// W11 vendor 前缀规则：monaco 为多文件目录树（40+ 文件），逐文件白名单不现实——
/// `vendor/<rel>` 以「路径段校验（无 `..`、无反斜杠、无空段）+ 扩展名白名单」放行，
/// 既覆盖目录树又保证只可能落到 web-dist/vendor/ 下的静态资源。
fn web_dist_file(name: &str) -> Option<PathBuf> {
    const ALLOWED: &[&str] = &["index.html", "style.css", "app.js"];
    const VENDOR_EXTS: &[&str] = &["js", "css", "json", "ttf", "woff", "woff2"];
    let rel: PathBuf = if ALLOWED.contains(&name) {
        PathBuf::from(name)
    } else if let Some(vendor_rel) = name.strip_prefix("vendor/") {
        // 逐段校验：空段 / 点段（. / ..）/ 反斜杠 / NUL 一律拒绝
        let mut segs = PathBuf::new();
        for seg in vendor_rel.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') || seg.contains('\0') {
                return None;
            }
            segs.push(seg);
        }
        let ext_ok = segs
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| VENDOR_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !ext_ok {
            return None;
        }
        PathBuf::from("vendor").join(segs)
    } else {
        return None;
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("web-dist").join(&rel));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("crates").join("host").join("web-dist").join(&rel));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("web-dist").join(&rel));
        }
    }
    candidates.into_iter().find(|p| p.exists())
}

/// 静态资源 content-type：顶层三件走既有映射；vendor 按扩展名（W11）。
fn static_content_type(name: &str) -> &'static str {
    match name {
        "style.css" => "text/css; charset=utf-8",
        "app.js" => "text/javascript; charset=utf-8",
        n if n.starts_with("vendor/") => match n.rsplit('.').next().unwrap_or("") {
            "css" => "text/css; charset=utf-8",
            "js" | "mjs" => "text/javascript; charset=utf-8",
            "json" => "application/json; charset=utf-8",
            "ttf" => "font/ttf",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            _ => "application/octet-stream",
        },
        _ => "text/html; charset=utf-8",
    }
}

/// W17 迭代六：index.html serve 时注入实例工作区标识（`window.RA_WS`）。
/// 前端会话列表 localStorage 以它做命名空间——会话归属**工作区**而非端口（源）：
/// 多实例端口动态分配、可被不同工作区复用，按端口存列表会跨工作区串显（用户实测：
/// test 工作区的会话出现在代码根工作区的界面）；绑工作区后实例复活换端口也不丢会话。
/// 找不到 `<head>` 时原样返回（前端退化为空后缀 key，不影响功能）。
fn inject_ws_anchor(html: &str, ws: &str) -> String {
    let tag = format!("<script>window.RA_WS={}</script>", json!(ws));
    match html.find("<head>") {
        Some(pos) => {
            let mut out = String::with_capacity(html.len() + tag.len());
            out.push_str(&html[..pos + "<head>".len()]);
            out.push_str(&tag);
            out.push_str(&html[pos + "<head>".len()..]);
            out
        }
        None => html.to_string(),
    }
}

/// 实例当前工作区（WORKSPACE_ROOT 优先，与文件树/undo 端点同口径）。
fn instance_workspace() -> String {
    let raw = std::env::var_os("WORKSPACE_ROOT")
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::config::workspace_dir().to_string_lossy().into_owned());
    norm_ws_key(&raw)
}

/// localStorage key 形态稳定化：剥一个尾部斜杠（`D:\ws\` 与 `D:\ws` 必须同 key）。
/// 驱动器根 `D:\` 会剥成 `D:`——形态依然恒定，key 稳定性不受影响。
fn norm_ws_key(ws: &str) -> String {
    ws.strip_suffix(['/', '\\']).unwrap_or(ws).to_string()
}

/// `GET /`、`/style.css`、`/app.js`：从 `web-dist/` 读取并返回；文件缺失给出明确 500（而非 panic）。
/// 强制 `no-store`：前端是运行时 serve 的，任何改动刷新即生效，绝不被浏览器缓存旧 JS。
async fn serve_static(stream: &mut TcpStream, name: &str) -> anyhow::Result<()> {
    let content_type = static_content_type(name);
    match web_dist_file(name) {
        Some(p) => match tokio::fs::read(&p).await {
            Ok(mut bytes) => {
                if name == "index.html" {
                    if let Ok(html) = std::str::from_utf8(&bytes) {
                        bytes = inject_ws_anchor(html, &instance_workspace()).into_bytes();
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(head.as_bytes()).await?;
                stream.write_all(&bytes).await?;
                stream.flush().await?;
                Ok(())
            }
            Err(e) => respond(
                stream,
                500,
                "application/json; charset=utf-8",
                json!({"ok": false, "error": {"code": "K500", "message": format!("读取前端文件失败: {e}")}})
                    .to_string()
                    .as_bytes(),
            )
            .await,
        },
        None => respond(
            stream,
            500,
            "application/json; charset=utf-8",
            json!({"ok": false, "error": {"code": "K500", "message": format!("找不到 web-dist/{name}（应在 crates/host/web-dist/ 下；cargo run 在 workspace 根或 crate 同级即可）")}})
                .to_string()
                .as_bytes(),
        )
        .await,
    }
}

async fn handle_conn(mut stream: TcpStream, kernel: Arc<Kernel>) -> anyhow::Result<()> {
    // 读请求头（至多 64KB）
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    let head_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("客户端关闭");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            anyhow::bail!("请求头过大");
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let path = parts.next().unwrap_or("/").to_string();
    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    // 读请求体
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    let (route, query) = match path.split_once('?') {
        Some((r, q)) => (r, q),
        None => (path.as_str(), ""),
    };

    match (method.as_str(), route) {
        ("GET", "/") | ("GET", "/index.html") => serve_static(&mut stream, "index.html").await,
        ("GET", "/style.css") => serve_static(&mut stream, "style.css").await,
        ("GET", "/app.js") => serve_static(&mut stream, "app.js").await,
        // W11：vendor 静态树（monaco 编辑器等），web_dist_file 内做段级校验 + 扩展名白名单
        ("GET", r) if r.starts_with("/vendor/") => {
            serve_static(&mut stream, r.trim_start_matches('/')).await
        }
        ("GET", "/api/events") => gateway::sse_events(stream, kernel, query).await,
        // 任务看板 / 灵感看板（单会话形态：左侧顶栏是任务与灵感，不是会话列表）。
        // GET 返回整块板 + 汇总；POST 只承接 UI 侧写（灵感增/改、补字段），任务写走 scripts/board.py。
        ("GET", "/api/board") => api::get_board(&mut stream).await,
        ("POST", "/api/board") => api::post_board(&mut stream, &body).await,
        // D 轮：任务/灵感 ↔ 对话轮次反查（读 trace 的 refs 字段，只读、与 board.py link 同口径）。
        ("GET", "/api/board/link") => api::get_board_link(&mut stream, query).await,
        // F2：画像（`.rsi/profile.json`）——GET 全量读，POST 只支持删除。
        // 「能看」解决「我记了什么看不见」，「只能删」守住「结论该由观察产生」。
        ("GET", "/api/profile") => api::get_profile(&mut stream).await,
        ("POST", "/api/profile") => api::post_profile(&mut stream, &body).await,
        // T-13 / I-20261108-11：GitHub 登录态（读 gh 已有凭据，不暴露 token）。
        // 刻意只做「读」：登录动作由用户在终端执行 gh auth login，
        // host 拉那条阻塞式交互命令会挂到超时（历史实测 K502）。
        ("GET", "/api/git/auth") => api::get_git_auth(&mut stream).await,
        // I-20261108-07：内容分发平台配置（凭证单独落 config/publish.json）
        ("GET", "/api/publish/config") => api::get_publish_config(&mut stream).await,
        ("POST", "/api/publish/config") => api::post_publish_config(&mut stream, &body).await,
        ("GET", "/api/config") => api::get_config(&mut stream, &kernel).await,
        ("GET", "/api/models") => api::get_models(&mut stream, &kernel, query).await,
        ("GET", "/api/presets") => api::get_presets(&mut stream, &kernel).await,
        // P2：Magpie 路由组管理（列表 / 选项 / 详情 / 新建 / 改 / 删 / 恢复）。
        // 组状态只存在于 Magpie 自己的 providers.json，机器可读出口**只有 CLI**
        // （`groups --json` / `group show --json` 都被忽略）——所以每个 handler 由 Host
        // 用 `Command::args(...)` 调 CLI，前端只发 HTTP，不碰 shell。
        // 顺序要紧：`/api/routegroups/options` 必须排在泛化的 `/api/routegroups/<id>`
        // **之前**——Rust 的 match 首个命中即生效，反过来 `options` 会被当成组 id 去读。
        ("GET", "/api/routegroups") => api::routegroups_list(&mut stream).await,
        ("GET", "/api/routegroups/options") => api::routegroups_options(&mut stream, &kernel).await,
        ("POST", "/api/routegroups") => api::routegroups_create(&mut stream, &body, &kernel).await,
        // 恢复（把被移除的 Magpie 内置 found 组放回来）：必须排在泛化 POST 之前单独判尾缀。
        ("POST", r) if r.starts_with("/api/routegroups/") && r.ends_with("/restore") => {
            api::routegroups_restore(&mut stream, r.trim_start_matches("/api/routegroups/")).await
        }
        ("PUT", r) if r.starts_with("/api/routegroups/") => {
            api::routegroups_set(&mut stream, r.trim_start_matches("/api/routegroups/"), &body, &kernel).await
        }
        ("DELETE", r) if r.starts_with("/api/routegroups/") => {
            api::routegroups_rm(&mut stream, r.trim_start_matches("/api/routegroups/")).await
        }
        ("GET", r) if r.starts_with("/api/routegroups/") => {
            api::routegroups_show(&mut stream, r.trim_start_matches("/api/routegroups/")).await
        }
        // 上游本地网关（magpie）：状态（探活+积分）/ 一键启动 serve
        ("GET", "/api/upstream/status") => api::upstream_status(&mut stream, &kernel).await,
        ("POST", "/api/upstream/start") => api::upstream_start(&mut stream).await,
        // 登录渠道清单（P3 动态化）：装了什么插件就有什么按钮，前端不再写死渠道表。
        ("GET", "/api/upstream/providers") => api::upstream_providers(&mut stream).await,
        // 账号签到扩展：弹登录窗（CLI 子进程，长阻塞由每连接 tokio::spawn 吸收）/ 全量签到 / 账号清单
        ("POST", "/api/upstream/login") => api::upstream_login(&mut stream, &query).await,
        ("POST", "/api/upstream/checkin") => api::upstream_checkin(&mut stream).await,
        ("GET", "/api/upstream/accounts") => api::upstream_accounts(&mut stream).await,
        // 自更新（SELF-UPDATE-2）：GET 查进度/版本，POST 触发交接换血。交接等待中的新进程
        // 拒绝再次自更新（409）——递归交接只会叠加等待链，没有意义。
        ("GET", "/api/self/update") => api::self_update_status(&mut stream).await,
        ("POST", "/api/self/update") => {
            if std::env::var("RSI_HANDOVER").map(|v| v == "1").unwrap_or(false) {
                json_resp(&mut stream, 409, json!({"ok": false, "error": {"code": "K409", "message": "本进程处于自更新交接等待中，拒绝再次自更新"}})).await
            } else {
                api::self_update_start(&mut stream).await
            }
        }
        ("PUT", "/api/config") => api::put_config(&mut stream, &kernel, &body).await,
        ("GET", "/api/skills") => api::get_skills(&mut stream, &kernel).await,
        ("GET", r) if r.starts_with("/api/skills/") => {
            api::get_skill(&mut stream, r.trim_start_matches("/api/skills/")).await
        }
        ("PUT", r) if r.starts_with("/api/skills/") => {
            api::put_skill(&mut stream, r.trim_start_matches("/api/skills/"), &body).await
        }
        ("DELETE", r) if r.starts_with("/api/skills/") => {
            api::delete_skill(&mut stream, r.trim_start_matches("/api/skills/")).await
        }
        ("POST", "/api/reveal") => api::reveal_target(&mut stream, query).await,
        // W17 多窗口多实例：spawn 自身起新实例 / 清单探活 / 停止（任意窗口可再开窗口）
        ("POST", "/api/instances") => api::create_instance(&mut stream, &body).await,
        ("GET", "/api/instances") => api::list_instances(&mut stream).await,
        ("POST", "/api/pick-folder") => api::pick_folder(&mut stream).await,
        ("DELETE", r) if r.starts_with("/api/instances/") => {
            api::delete_instance(&mut stream, r.trim_start_matches("/api/instances/")).await
        }
        ("GET", "/api/tree") => files::list_workspace_tree(&mut stream, query).await,
        ("GET", "/api/fc-snapshot") => files::serve_undo_snapshot(&mut stream, query).await,
        ("GET", "/api/session-exists") => api::session_exists(&mut stream, query).await,
        // IM 会话元数据（网关写 <repo>/im-gateway/data/im-sessions.json）；只读，缺失即空。
        ("GET", "/api/im/sessions") => api::im_sessions(&mut stream).await,
        ("GET", r) if r.starts_with("/files/") => {
            files::serve_workspace_file(&mut stream, r.trim_start_matches("/files/"), query).await
        }
        // Q 轮 Phase 3：真实运行态 / 显式队列（纯追加端点；旧 /api/chat 的 400/404
        // 与 K409 裸并发裁决逐字不变）。status 由 agent-loop 的 session_chats + 队列
        // 状态派生，多标签页不再靠「自己那次 fetch 没结束」猜。
        ("GET", "/api/chat/status") => {
            let session = parse_query(query).get("session").cloned().unwrap_or_default();
            if session.is_empty() {
                return json_resp(&mut stream, 400, bad_request("缺 session 参数", Some("session"))).await;
            }
            let resp = kernel
                .dispatch(Envelope::new(PluginId::new("agent-loop"), json!({"op": "queue/status", "session_id": session})))
                .await
                .unwrap_or_else(|e| json!({"ok": false, "error": {"code": "K500", "message": e.to_string()}}));
            let st = resp.get("status").cloned().unwrap_or_else(|| json!({"ok": false}));
            json_resp(&mut stream, 200, st).await
        }
        ("POST", "/api/queue") => {
            let req: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return json_resp(&mut stream, 400, json!({"ok": false, "error": {"code": "K400", "message": format!("body 非法 JSON: {e}")}})).await;
                }
            };
            let Some(session) = req.get("session_id").and_then(Value::as_str) else {
                return json_resp(&mut stream, 400, bad_request("缺 session_id", Some("session_id"))).await;
            };
            let Some(text) = req.get("message").and_then(Value::as_str) else {
                return json_resp(&mut stream, 400, bad_request("缺 message", Some("message"))).await;
            };
            let mut payload = json!({"op": "queue/enqueue", "session_id": session, "user_text": text,
                "target": req.get("target").and_then(Value::as_str).unwrap_or("followup")});
            if let Some(att) = req.get("attachments") {
                match api::validate_attachments(att) {
                    Ok(Some(list)) => { payload["attachments"] = list; }
                    Ok(None) => {}
                    Err(e) => return json_resp(&mut stream, 400, e).await,
                }
            }
            let resp = kernel
                .dispatch(Envelope::new(PluginId::new("agent-loop"), payload))
                .await
                .unwrap_or_else(|e| json!({"ok": false, "error": {"code": "K500", "message": e.to_string()}}));
            json_resp(&mut stream, 200, resp).await
        }
        ("POST", "/api/queue/remove") => {
            let req: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return json_resp(&mut stream, 400, json!({"ok": false, "error": {"code": "K400", "message": format!("body 非法 JSON: {e}")}})).await;
                }
            };
            let payload = json!({"op": "queue/remove", "session_id": req.get("session_id").and_then(Value::as_str).unwrap_or(""), "id": req.get("id").and_then(Value::as_str).unwrap_or("")});
            let resp = kernel
                .dispatch(Envelope::new(PluginId::new("agent-loop"), payload))
                .await
                .unwrap_or_else(|e| json!({"ok": false, "error": {"code": "K500", "message": e.to_string()}}));
            json_resp(&mut stream, 200, resp).await
        }
        ("POST", "/api/chat/cancel") => {
            // P2/T1：取消运行中的 chat。转发 agent-loop `cancel` op（置位取消标志，
            // 循环在轮次边界收敛为 K499）。不等待 chat 结束，立即返回。
            // R1 补强：并行向 llm-adapter dispatch `abort`——流式生成逐帧检查命中即关流，
            // 单轮长生成无需等轮次边界（llm-adapter 已为 Concurrent 语义，可即时受理）。
            let session = parse_query(query).get("session").cloned().unwrap_or_default();
            if session.is_empty() {
                return json_resp(&mut stream, 400, bad_request("缺 session 参数", Some("session"))).await;
            }
            match dispatch_or_err(&kernel, "agent-loop", json!({"op": "cancel", "session_id": session})).await {
                Ok(_) => {
                    if let Err(e) =
                        dispatch_or_err(&kernel, "llm-adapter", json!({"op": "abort", "session_id": session})).await
                    {
                        tracing::warn!(session = %session, "llm-adapter abort dispatch failed（轮次边界取消仍有效）: {e}");
                    }
                    json_resp(
                        &mut stream,
                        200,
                        json!({"ok": true, "session_id": session, "note": "取消信号已置位；流式生成中断或轮次边界收敛"}),
                    )
                    .await
                }
                Err(e) => json_resp(&mut stream, 502, e).await,
            }
        }
        ("POST", "/api/chat/rollback") => {
            // R2 回滚：按「第 N 条 user 消息」（0 基）截断——memory 插件原子回滚
            // 会话消息（LLM 上下文）与 trace 事件日志（UI 重放），二者同源对齐。
            let req: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return respond(
                        &mut stream,
                        400,
                        "application/json; charset=utf-8",
                        json!({"ok": false, "error": {"code": "K400", "message": format!("body 非法 JSON: {e}")}})
                            .to_string()
                            .as_bytes(),
                    )
                    .await
                }
            };
            let Some(session) = req.get("session_id").and_then(Value::as_str) else {
                return json_resp(&mut stream, 400, bad_request("缺 session_id", Some("session_id"))).await;
            };
            let Some(idx) = req.get("upto_user_index").and_then(Value::as_u64) else {
                return json_resp(&mut stream, 400, bad_request("缺 upto_user_index", Some("upto_user_index"))).await;
            };
            // R15 文件撤销：memory 物理截断**之前**先读 trace 收集区间内带 undo 引用的
            // 变更事件；截断成功后倒序恢复（冲突检测：文件被改过即跳过，绝不硬覆盖）。
            // body `undo_files:false` 可显式关闭（缺省撤销）。bash 间接改的文件无事件、不追踪。
            let undo_items = if req.get("undo_files").and_then(Value::as_bool) != Some(false) {
                files::collect_undo_items(session, idx)
            } else {
                Vec::new()
            };
            match dispatch_or_err(
                &kernel,
                "memory",
                json!({"op": "rollback", "session_id": session, "upto_user_index": idx}),
            )
            .await
            {
                Ok(mut v) => {
                    if !undo_items.is_empty() {
                        let (undone, skipped) = files::undo_file_changes(&undo_items);
                        v["undone"] = json!(undone);
                        v["skipped"] = json!(skipped);
                    }
                    json_resp(&mut stream, 200, v).await
                }
                Err(e) => json_resp(&mut stream, 502, e).await,
            }
        }
        ("POST", "/api/chat") => {
            let req: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return respond(
                        &mut stream,
                        400,
                        "application/json; charset=utf-8",
                        json!({"ok": false, "error": {"code": "K400", "message": format!("body 非法 JSON: {e}")}})
                            .to_string()
                            .as_bytes(),
                    )
                    .await
                }
            };
            let Some(session) = req.get("session_id").and_then(Value::as_str) else {
                return respond(
                    &mut stream,
                    400,
                    "application/json; charset=utf-8",
                    json!({"ok": false, "error": {"code": "K400", "message": "缺 session_id"}})
                        .to_string()
                        .as_bytes(),
                )
                .await;
            };
            let Some(message) = req.get("message").and_then(Value::as_str) else {
                return respond(
                    &mut stream,
                    400,
                    "application/json; charset=utf-8",
                    json!({"ok": false, "error": {"code": "K400", "message": "缺 message"}})
                        .to_string()
                        .as_bytes(),
                )
                .await;
            };
            // R3 附件（可选）：[{name, mime, data_b64}]——校验形状与体量上限后透传
            // agent-loop（构造多模态 user 消息）。非法/超限 = 确定性 K400，不静默丢弃。
            let mut chat_payload = json!({"op": "chat", "session_id": session, "user_text": message});
            // Q6：续跑（被插队打断后自动回头）不重复落库/重放 user 消息——被打断那轮
            // 已在 chat_run 开头 append 过 user 消息并发出 trace user 事件，重发会造成
            // memory 多一条 user、前端多一个气泡。resume=true 让 chat_run 跳过这两步，
            // 直接用已有工作集继续（见 chat.rs resume 分支）。
            if req.get("resume").and_then(Value::as_bool) == Some(true) {
                chat_payload["resume"] = json!(true);
            }
            if let Some(att) = req.get("attachments") {
                match api::validate_attachments(att) {
                    Ok(Some(list)) => {
                        chat_payload["attachments"] = list;
                    }
                    Ok(None) => {}
                    Err(e) => return json_resp(&mut stream, 400, e).await,
                }
            }
            let resp = kernel
                .dispatch(Envelope::new(PluginId::new("agent-loop"), chat_payload))
                .await
                .unwrap_or_else(|e| json!({"ok": false, "error": {"code": "K500", "message": e.to_string()}}));
            respond(
                &mut stream,
                200,
                "application/json; charset=utf-8",
                resp.to_string().as_bytes(),
            )
            .await
        }
        _ => {
            respond(
                &mut stream,
                404,
                "application/json; charset=utf-8",
                json!({"ok": false, "error": {"code": "K404", "message": format!("no route: {method} {route}")}})
                    .to_string()
                    .as_bytes(),
            )
            .await
        }
    }
}

/// guest 调用便捷封装：ok → Ok(v)；业务失败/传输失败 → Err（错误 payload）。
async fn dispatch_or_err(kernel: &Kernel, target: &str, payload: Value) -> Result<Value, Value> {
    match kernel.dispatch(Envelope::new(PluginId::new(target), payload)).await {
        Ok(v) if v.get("ok") == Some(&json!(true)) => Ok(v),
        Ok(v) => Err(v),
        Err(e) => Err(json!({"ok": false, "error": {"code": "K500", "message": e.to_string()}})),
    }
}

async fn json_resp(stream: &mut TcpStream, status: u16, v: Value) -> anyhow::Result<()> {
    respond(stream, status, "application/json; charset=utf-8", v.to_string().as_bytes()).await
}

fn bad_request(msg: impl Into<String>, field: Option<&str>) -> Value {
    let mut error = json!({"code": "K400", "message": msg.into()});
    if let Some(f) = field {
        error["field"] = json!(f);
    }
    json!({"ok": false, "error": error})
}

async fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

/// 极简 query 解析（含 %XX 与 '+' 解码）。
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 1 && i + 2 < bytes.len() + 1 => {
                let hex = bytes.get(i + 1..i + 3).and_then(|h| {
                    std::str::from_utf8(h).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
                });
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parses_and_decodes() {
        let q = parse_query("session=a%20b&after=7&x=");
        assert_eq!(q.get("session").unwrap(), "a b");
        assert_eq!(q.get("after").unwrap(), "7");
        assert_eq!(q.get("x").unwrap(), "");
        assert!(q.get("missing").is_none());
    }

    // ── W11：vendor 前缀规则（段级校验 + 扩展名白名单）──

    #[test]
    fn vendor_paths_resolved_and_rejected() {
        // 合法路径 → web-dist/vendor/ 下相对路径
        let hit = web_dist_file("vendor/monaco/vs/loader.js").expect("loader.js 应命中");
        assert!(hit.ends_with("vendor/monaco/vs/loader.js"), "got {hit:?}");
        assert!(hit.exists(), "vendor 文件应真实存在");

        // 目录穿越 / 反斜杠 / 点段 / 空段 / 无扩展名一律拒绝
        for bad in [
            "vendor/../Cargo.toml",
            "vendor/monaco/../../lib.rs",
            "vendor/monaco/vs\\base\\worker/workerMain.js",
            "vendor/./x.js",
            "vendor//x.js",
            "vendor/x",
            "vendor/monaco/vs/loader.rs",
        ] {
            assert!(web_dist_file(bad).is_none(), "{bad} 应被拒绝");
        }
        // 非 vendor 且不在顶层白名单 → 拒绝
        assert!(web_dist_file("app2.js").is_none());
    }

    #[test]
    fn vendor_content_types_by_ext() {
        assert_eq!(static_content_type("vendor/a/b.js"), "text/javascript; charset=utf-8");
        assert_eq!(static_content_type("vendor/a.css"), "text/css; charset=utf-8");
        assert_eq!(static_content_type("vendor/x/codicon.ttf"), "font/ttf");
        assert_eq!(static_content_type("vendor/x.woff2"), "font/woff2");
        assert_eq!(static_content_type("vendor/x.bin"), "application/octet-stream");
        assert_eq!(static_content_type("style.css"), "text/css; charset=utf-8");
        assert_eq!(static_content_type("index.html"), "text/html; charset=utf-8");
    }

    #[test]
    fn ws_anchor_injected_right_after_head() {
        let html = "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>";
        let out = inject_ws_anchor(html, r"D:\ws a\b");
        // json! 序列化负责引号与反斜杠转义，产出合法 JS 字面量
        assert!(out.contains(r#"<head><script>window.RA_WS="D:\\ws a\\b"</script>"#), "{out}");
        assert!(out.ends_with("</body></html>"), "其余内容原样保留");
    }

    #[test]
    fn ws_anchor_without_head_is_noop() {
        let html = "<html><body>hi</body></html>";
        assert_eq!(inject_ws_anchor(html, "D:\\ws"), html);
    }

    #[test]
    fn norm_ws_key_strips_single_trailing_slash() {
        assert_eq!(super::norm_ws_key(r"D:\ws\"), r"D:\ws");
        assert_eq!(super::norm_ws_key("D:/ws/"), "D:/ws");
        assert_eq!(super::norm_ws_key(r"D:\"), "D:");
        assert_eq!(super::norm_ws_key("/ws//"), "/ws/");
        assert_eq!(super::norm_ws_key("D:\\ws"), "D:\\ws");
    }
}
