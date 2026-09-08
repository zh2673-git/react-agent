//! `/files` 工作区文件服务（W10 自 frontend.rs 拆分）：按 mime 服务工作区内文件，
//! 与文件工具同一越界纪律（realpath ⊆ WORKSPACE_ROOT）。
use super::{bad_request, json_resp, parse_query, percent_decode};
use crate::config;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// 工具源码目录（内置 9 件 + 动态装载池）：PLUGINS_DIR/tools，缺省 <workspace>/plugins/tools。
pub(super) fn tools_dir() -> std::path::PathBuf {
    std::env::var_os("PLUGINS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config::workspace_dir().join("plugins"))
        .join("tools")
}

/// GET /api/tree?limit=：工作区文件清单（W14 文件树数据源）。递归遍历 WORKSPACE_ROOT
/// （跳过噪音目录 .git/node_modules/target/__pycache__/.venv/venv/dist/build/.idea/.stream
/// 与点目录），返回扁平相对路径数组（正斜杠 + size），目录/文件排序（目录在前）。
/// 上限保护：max_depth=6、max_entries（默认 3000，≤20000），超限标 truncated。
/// 只读清单不涉及越界写；路径口径与 /files 一致（同一 WORKSPACE_ROOT 基准）。
pub(super) async fn list_workspace_tree(stream: &mut TcpStream, query: &str) -> anyhow::Result<()> {
    const MAX_DEPTH: usize = 6;
    const NOISE: &[&str] = &[
        ".git", "node_modules", "target", "__pycache__", ".venv", "venv", "dist", "build",
        ".idea", ".stream", ".trae", ".vscode",
    ];
    let ws_base = std::env::var_os("WORKSPACE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(config::workspace_dir);
    let ws = std::fs::canonicalize(&ws_base).unwrap_or_else(|_| ws_base);
    let limit = parse_query(query)
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(1, 20_000))
        .unwrap_or(3_000);

    // 深度优先收集（迭代栈实现，避免递归）；剪枝噪音/点目录；symlink 不跟随（walkdir 语义）
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut truncated = false;
    let mut stack: Vec<(std::path::PathBuf, String, usize)> = vec![(ws.clone(), String::new(), 0)];
    while let Some((dir, rel_prefix, depth)) = stack.pop() {
        let mut entries: Vec<(std::path::PathBuf, bool, u64)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || NOISE.contains(&name.as_str()) {
                continue;
            }
            let is_dir = ft.is_dir();
            let size = if is_dir { 0 } else { e.metadata().map(|m| m.len()).unwrap_or(0) };
            entries.push((e.path(), is_dir, size));
        }
        // 目录在前，各自按名排序
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.file_name().cmp(&b.0.file_name())));
        for (path, is_dir, size) in entries {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            let rel = if rel_prefix.is_empty() { name } else { format!("{rel_prefix}/{name}") };
            if is_dir {
                if depth + 1 < MAX_DEPTH {
                    stack.push((path.clone(), rel, depth + 1));
                }
            } else {
                if files.len() >= limit {
                    truncated = true;
                    break;
                }
                files.push((rel, size));
            }
        }
        if truncated {
            break;
        }
    }
    // 栈遍历顺序非全局有序 → 最终按路径排序稳定输出（目录前缀天然聚簇）
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let payload = json!({
        "ok": true,
        "root": ws.to_string_lossy(),
        "count": files.len(),
        "truncated": truncated,
        "files": files.iter().map(|(p, s)| json!({"path": p, "size": s})).collect::<Vec<_>>(),
    });
    json_resp(stream, 200, payload).await
}

/// 变更快照目录（R15 回滚撤销 + 双侧 diff）：MEMORY_DATA_DIR/undo，缺省
/// <workspace>/plugins/memory/data/undo——与 files.py `_undo_dir` 同一口径。
pub(super) fn undo_dir() -> std::path::PathBuf {
    let base = std::env::var_os("MEMORY_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config::workspace_dir().join("plugins").join("memory").join("data"));
    base.join("undo")
}

/// GET /api/fc-snapshot?id=&side=before|after：取一次文件变更的前/后快照原始字节
/// （前端 diff 视图数据源）。id 形如 `{13位毫秒}-{8位hex}`（files.py 生成），严格校验
/// 防穿越；快照目录在工作区外，此端点是前端唯一取用通道。
pub(super) async fn serve_undo_snapshot(stream: &mut TcpStream, query: &str) -> anyhow::Result<()> {
    let q = parse_query(query);
    let id = q.get("id").map(String::as_str).unwrap_or("");
    let side = q.get("side").map(String::as_str).unwrap_or("");
    let valid_id = match id.split_once('-') {
        Some((ts, hex)) => {
            ts.len() == 13
                && ts.bytes().all(|b| b.is_ascii_digit())
                && hex.len() == 8
                && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }
        None => false,
    };
    if !valid_id || !matches!(side, "before" | "after") {
        return json_resp(stream, 400, bad_request("非法 id 或 side", Some("id"))).await;
    }
    let path = undo_dir().join(format!("{id}.{side}"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => {
            return json_resp(
                stream,
                404,
                json!({"ok": false, "error": {"code": "K404", "message": "快照不存在（超限或无快照的变更无 undo 数据）"}}),
            )
            .await
        }
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        bytes.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// 回滚前收集待撤销变更：读会话 trace（R2 物理截断**之前**），从第 `upto_user_index`
/// 条 user 事件（0 基）起至文件尾的区间内收集带 undo 引用的 file_change 事件。
/// session 严格校验（文件名拼接防穿越）；无 trace 文件（纯 memory 会话）→ 空清单。
pub(super) fn collect_undo_items(session: &str, upto_user_index: u64) -> Vec<Value> {
    if session.is_empty() || session.contains(['/', '\\']) || session.contains("..") {
        return Vec::new();
    }
    let Some(traces) = undo_dir().parent().map(|p| p.join("traces")) else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(traces.join(format!("{session}.jsonl"))) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let (mut user_idx, mut in_region) = (0u64, false);
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if !in_region && v.get("type").and_then(Value::as_str) == Some("user") {
            if user_idx == upto_user_index {
                in_region = true;
            }
            user_idx += 1;
        }
        if in_region
            && v.get("type").and_then(Value::as_str) == Some("file_change")
            && v.get("undo").and_then(|u| u.get("id")).and_then(Value::as_str).is_some()
        {
            items.push(json!({"path": v.get("path"), "op": v.get("op"), "undo": v.get("undo")}));
        }
    }
    items
}

/// 执行文件撤销（回滚截断后调用）：倒序逐条恢复。安全纪律：
/// - 冲突检测：当前文件字节必须与该次变更的 after 快照完全一致才撤销——agent 写完后
///   人又改过 → 跳过并报告，绝不硬覆盖用户改动；
/// - created（新建文件）→ 删除；deleted（W16 bash 删除）→ 文件须仍不存在（被重建则
///   跳过），还原 before 快照字节；覆盖/编辑 → 还原 before 快照原始字节；
/// - 路径走 /files 同款 realpath 工作区边界校验；成功后删除快照对。
/// 返回 (已撤销路径, 跳过明细)。
pub(super) fn undo_file_changes(items: &[Value]) -> (Vec<String>, Vec<Value>) {
    let ws_base = std::env::var_os("WORKSPACE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(config::workspace_dir);
    let ws = std::fs::canonicalize(&ws_base).unwrap_or_else(|_| ws_base);
    let ws_s = os_normcase(&ws);
    let (mut undone, mut skipped) = (Vec::new(), Vec::new());
    for it in items.iter().rev() {
        let Some(path) = it.get("path").and_then(Value::as_str) else { continue };
        let Some(id) = it.get("undo").and_then(|u| u.get("id")).and_then(Value::as_str) else { continue };
        let meta = it.get("undo").cloned().unwrap_or(json!({}));
        let created = meta.get("created").and_then(Value::as_bool).unwrap_or(false);
        let deleted = meta.get("deleted").and_then(Value::as_bool).unwrap_or(false);
        let rel = path.replace('\\', "/");
        if rel.is_empty() || rel.split('/').any(|seg| seg == "..") {
            skipped.push(json!({"path": path, "reason": "非法路径"}));
            continue;
        }
        let real = std::fs::canonicalize(ws.join(&rel)).ok();
        if let Some(r) = &real {
            let rs = os_normcase(r);
            if !(rs == ws_s
                || rs.starts_with(&format!("{ws_s}{}", std::path::MAIN_SEPARATOR_STR))
                || rs.starts_with(&format!("{ws_s}\\")))
            {
                skipped.push(json!({"path": path, "reason": "路径越界"}));
                continue;
            }
        }
        let (after_p, before_p) = (undo_dir().join(format!("{id}.after")), undo_dir().join(format!("{id}.before")));
        let restored = if deleted {
            // W16 删除恢复：文件当前必须不存在（被重建 → 跳过，不覆盖重建内容）
            if real.is_some() {
                skipped.push(json!({"path": path, "reason": "文件在删除后又被重建，跳过恢复"}));
                continue;
            }
            match std::fs::read(&before_p) {
                Ok(b) => std::fs::write(ws.join(&rel), b).is_ok(),
                Err(_) => false,
            }
        } else {
            let Some(r) = real else {
                skipped.push(json!({"path": path, "reason": "文件不存在"}));
                continue;
            };
            // 冲突检测：当前文件必须与该次变更写入的 after 快照逐字节一致才撤销
            match (std::fs::read(&r), std::fs::read(&after_p)) {
                (Ok(cur), Ok(after)) if cur == after => {}
                (Ok(_), Ok(_)) => {
                    skipped.push(json!({"path": path, "reason": "变更后文件又被修改，跳过撤销"}));
                    continue;
                }
                _ => {
                    skipped.push(json!({"path": path, "reason": "快照缺失或不可读"}));
                    continue;
                }
            }
            if created {
                std::fs::remove_file(&r).is_ok()
            } else {
                match std::fs::read(&before_p) {
                    Ok(b) => std::fs::write(&r, b).is_ok(),
                    Err(_) => false,
                }
            }
        };
        if !restored {
            skipped.push(json!({"path": path, "reason": "撤销写入失败"}));
            continue;
        }
        let _ = std::fs::remove_file(&after_p);
        let _ = std::fs::remove_file(&before_p);
        undone.push(path.to_string());
    }
    (undone, skipped)
}

/// GET /files/{path}[?download=1]：按 mime 服务工作区内文件。前端文件卡片（artifact
/// trace 事件渲染）经此取内容/下载——浏览器禁 file:// 链接，host 代为 serve。
/// 与文件工具同一越界纪律：realpath ⊆ WORKSPACE_ROOT；目录/越界/缺失/超限均明确拒绝。
pub(super) async fn serve_workspace_file(stream: &mut TcpStream, raw: &str, query: &str) -> anyhow::Result<()> {
    const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
    // 根口径与文件工具一致：WORKSPACE_ROOT env 优先（越界拦截同一基准），缺省编译期工作区
    let ws_base = std::env::var_os("WORKSPACE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(config::workspace_dir);
    let ws = std::fs::canonicalize(&ws_base).unwrap_or_else(|_| ws_base);
    // 路径归一正斜杠（历史 artifact 可能含反斜杠/绝对前缀），穿越校验统一按 '/'
    let rel = percent_decode(raw.trim_end_matches('/')).replace('\\', "/");
    if rel.is_empty() || rel.split('/').any(|seg| seg == "..") {
        return json_resp(stream, 400, bad_request("非法路径", Some("path"))).await;
    }
    let full = ws.join(&rel);
    let real = match std::fs::canonicalize(&full) {
        Ok(r) => r,
        Err(_) => {
            // 兜底：历史产物路径可能带工作区绝对前缀（含空格路径被空白截断/模型复述
            // 绝对路径）——含「工作区目录名/」时截掉前缀重试一次。最终仍走 realpath 越界校验。
            let root_name = ws.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let retry = (!root_name.is_empty())
                .then(|| rel.split_once(&format!("{root_name}/")).map(|(_, rest)| rest))
                .flatten()
                .map(|rest| std::fs::canonicalize(ws.join(rest)));
            match retry {
                Some(Ok(r)) => r,
                _ => {
                    return json_resp(
                        stream,
                        404,
                        json!({"ok": false, "error": {"code": "K404", "message": format!("文件不存在: {rel}")}}),
                    )
                    .await
                }
            }
        }
    };
    let (a, b) = (os_normcase(&real), os_normcase(&ws));
    if !(a == b || a.starts_with(&format!("{b}{}", std::path::MAIN_SEPARATOR_STR))
        || a.starts_with(&format!("{b}\\")))
    {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": "路径越界：不在工作区内"}}),
        )
        .await;
    }
    if !real.is_file() {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": "该路径不是文件（目录不支持预览）"}}),
        )
        .await;
    }
    let meta = std::fs::metadata(&real)?;
    if meta.len() > MAX_FILE_BYTES {
        return json_resp(
            stream,
            400,
            json!({"ok": false, "error": {"code": "K400", "message": format!("文件超过 {}MB 上限，不支持经 /files 传输", MAX_FILE_BYTES / 1024 / 1024)}}),
        )
        .await;
    }
    let bytes = tokio::fs::read(&real).await?;
    let mime = mime_for(&real);
    let download = parse_query(query).contains_key("download");
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let disp = if download {
        format!("attachment; filename*=UTF-8''{}", percent_encode(&name))
    } else {
        "inline".to_string()
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {mime}\r\ncontent-disposition: {disp}; filename=\"{}\"\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        name.replace(['\\', '"', '\r', '\n'], "_"),
        bytes.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

fn os_normcase(p: &std::path::Path) -> String {
    #[cfg(target_os = "windows")]
    {
        p.to_string_lossy().to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        p.to_string_lossy().into_owned()
    }
}

/// 按扩展名推 Content-Type（MVP 清单：产物卡片用到的 + 常见文本/图片）。
fn mime_for(p: &std::path::Path) -> &'static str {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" | "log" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// 最小 percent-encode（RFC 5987 filename*）：非 unreserved 字节转 %XX。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
