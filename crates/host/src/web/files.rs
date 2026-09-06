//! `/files` 工作区文件服务（W10 自 frontend.rs 拆分）：按 mime 服务工作区内文件，
//! 与文件工具同一越界纪律（realpath ⊆ WORKSPACE_ROOT）。
use super::{bad_request, json_resp, parse_query, percent_decode};
use crate::config;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// 工具源码目录（内置 8 件 + 动态装载池）：PLUGINS_DIR/tools，缺省 <workspace>/plugins/tools。
pub(super) fn tools_dir() -> std::path::PathBuf {
    std::env::var_os("PLUGINS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config::workspace_dir().join("plugins"))
        .join("tools")
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
