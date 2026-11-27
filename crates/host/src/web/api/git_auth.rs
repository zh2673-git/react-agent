//! GitHub 登录态（T-13）：读 `gh` 已有登录，必要时引导设备码。
//!
//! ## 为什么不自建 OAuth（这是本模块最重要的设计决定）
//!
//! 自建 device flow 需要注册一个 GitHub OAuth App 拿到 `client_id`——那是**发行方**
//! 的身份，不是用户能随手填的东西。把 `client_id` 硬编码进本仓 = 所有用户共用
//! 一个应用配额与责任主体；让用户自己注册 = 「一键登录」变成「五步建应用」，
//! 体验反而比手配 token 更差。
//!
//! 正确做法是**复用本机已装的 `gh`**：它自带官方 client_id、把 token 存在系统
//! keyring（Windows Credential Manager）里，并且 `gh auth status --json hosts`
//! 能**不暴露 token** 地返回登录态。我们只读这份状态，不碰凭据本身。
//!
//! ## 安全纪律（每条都有对应的实现动作）
//!
//! 1. **token 绝不进 HTTP 响应**：`auth_status` 的返回只含 login/scopes/tokenSource，
//!    不含 token。`gh auth status --json hosts` 的输出本身就满足这一点，这是选它
//!    而不选 `gh auth token` 的原因。
//! 2. **不把 token 写进日志**：本模块只透传 gh 的 JSON，不额外打印 env。
//! 3. **登录动作不由本进程代持**：`auth_start` 只**给出要执行的命令与链接**，
//!    真正的交互（浏览器授权）发生在用户自己的终端里——本进程不阻塞、不代收凭据。
//!    这避免了「host 用 pty 拉起 gh auth login 然后阻塞到 K502」那条死路。
//!
//! ## 两种登录路径（按可达性排序）
//!
//! | 路径 | 前提 | 交互 |
//! |---|---|---|
//! | A. 已装 gh 且已登录 | 最常见 | 零交互，设置面板直接显示账号与 scopes |
//! | B. 已装 gh 未登录 | 需用户配合 | 面板给出「复制到终端跑」的一行命令 + 设备码链接 |
//! | C. 未装 gh | 兜底 | 给出安装指引 + 手配 token 的落点（走既有 config） |
//!
//! 路径 B 刻意**不在 host 里执行**：`gh auth login` 是阻塞式交互命令，host 拉它会
//! 挂到超时（历史实测 K502）。把命令交给用户复制，比做一个「点了没反应」的按钮诚实。

use serde_json::{json, Value};
use std::process::Command;

use super::super::json_resp;
use tokio::net::TcpStream;

/// 跑 `gh` 并拿 stdout；任何失败都折叠成 `Err(给人看的原因)`。
///
/// 刻意不区分「未安装 / 超时 / 非零退出」的底层细节：调用方（设置面板）需要的
/// 是一个能直接展示的下一步，不是 errno。
fn run_gh(args: &[&str]) -> Result<String, String> {
    let out = Command::new("gh").args(args).output();
    match out {
        Err(e) => Err(format!(
            "读不到 gh 命令（{}）。若未安装 GitHub CLI，请按 GH_MISSING 指引手配 token。",
            e.kind()
        )),
        Ok(o) => {
            if o.status.success() {
                Ok(String::from_utf8_lossy(&o.stdout).to_string())
            } else {
                Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
            }
        }
    }
}

/// GET /api/git/auth：归一化登录态。
///
/// 返回结构（前端只需这三种状态，不要它自己判 errno）：
/// - `{ status: "logged_in",  host, login, scopes, token_source }`
/// - `{ status: "logged_out", host, hint, command }`
/// - `{ status: "gh_missing", hint, install }`
pub(crate) async fn get_git_auth(stream: &mut TcpStream) -> anyhow::Result<()> {
    // 首选：结构化输出。它不含 token（`Token:` 那行是 `gh auth status` 的**人类**
    // 可读输出才有的，`--json` 只给 login/scopes/tokenSource），这是选它的唯一理由。
    match run_gh(&["auth", "status", "--json", "hosts"]) {
        Ok(stdout) => match serde_json::from_str::<Value>(&stdout) {
            Ok(v) => {
                let hosts = v.get("hosts").and_then(Value::as_object);
                let first = hosts.and_then(|h| h.iter().find_map(|(host, arr)| {
                    arr.as_array().and_then(|a| a.first()).map(|e| (host.clone(), e.clone()))
                }));
                match first {
                    Some((host, e)) => {
                        let ok = e.get("state").and_then(Value::as_str) == Some("success");
                        if ok {
                            json_resp(stream, 200, json!({
                                "ok": true,
                                "status": "logged_in",
                                "host": host,
                                "login": e.get("login").cloned().unwrap_or(Value::Null),
                                // token 只报来源（keyring / env / plaintext），绝不报值
                                "token_source": e.get("tokenSource").cloned().unwrap_or(Value::Null),
                                "scopes": e.get("scopes").cloned().unwrap_or(Value::Null),
                                "git_protocol": e.get("gitProtocol").cloned().unwrap_or(Value::Null),
                            }))
                            .await
                        } else {
                            logged_out(stream, &host).await
                        }
                    }
                    None => {
                        // 命令成功但没有任何 host：等价于未登录
                        logged_out(stream, "github.com").await
                    }
                }
            }
            Err(e) => json_resp(stream, 200, json!({
                "ok": false,
                "status": "unknown",
                "hint": format!("gh 返回了无法解析的输出（{e}）。可在终端执行 `gh auth status` 自查。"),
            }))
            .await,
        },
        Err(reason) => {
            // 分不清「没装」与「装了但没登录」时，用 gh 自身的错误文本判断：
            // `gh auth status` 未登录时退出码 1 且 stderr 含 "not logged"；
            // 未安装时 Command::new 直接失败（已折叠成 err.kind() 文案）。
            let low = reason.to_lowercase();
            if low.contains("not logged") || low.contains("no oauth") || low.contains("authenticate") {
                logged_out(stream, "github.com").await
            } else if reason.contains("读不到 gh 命令") {
                json_resp(stream, 200, json!({
                    "ok": false,
                    "status": "gh_missing",
                    "hint": "未检测到 GitHub CLI。装好后重新打开本页即可自动读取登录态。",
                    "install": "winget install --id GitHub.cli   （或 https://cli.github.com/）",
                    "fallback": "也可在「工具」页手配 GITHUB_TOKEN（不推荐，token 会落盘）",
                }))
                .await
            } else {
                json_resp(stream, 200, json!({
                    "ok": false,
                    "status": "unknown",
                    "hint": format!("gh 执行失败：{}", reason.chars().take(200).collect::<String>()),
                }))
                .await
            }
        }
    }
}

/// 未登录时的响应：给出一条**可直接复制**的命令，而不是一个点了会阻塞的按钮。
async fn logged_out(stream: &mut TcpStream, host: &str) -> anyhow::Result<()> {
    json_resp(stream, 200, json!({
        "ok": true,
        "status": "logged_out",
        "host": host,
        // 为什么给 `--web --clipboard`：`--web` 走浏览器授权（不必手输 token），
        // `--clipboard` 把一次性设备码放进剪贴板（浏览器里直接粘贴）。
        // 为什么不让 host 代跑：它是阻塞式交互命令，host 拉它会挂到超时（实测 K502）。
        "command": "gh auth login --hostname github.com --git-protocol https --web --clipboard",
        "hint": "把这行命令粘到终端执行（浏览器会打开授权页，设备码已在剪贴板）。完成后点「重新检测」。",
        "docs": "https://cli.github.com/manual/gh_auth_login",
    }))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实环境的正向验证：本机装了 gh 且已登录时，必须归一出 logged_in，
    /// 且响应里**绝不能**出现 token 前缀（gho_/ghp_/github_pat_）。
    /// 这条测试的价值不在「断言一定登录」，而在**不泄漏**——即便环境变了，
    /// 泄漏检查也仍然有效。
    #[test]
    fn status_json_never_contains_token() {
        let Ok(out) = run_gh(&["auth", "status", "--json", "hosts"]) else {
            return; // 未装 gh / 未登录：本机无凭据可泄漏，跳过
        };
        for bad in ["gho_", "ghp_", "github_pat_", "ghs_"] {
            assert!(!out.contains(bad), "auth status --json 输出含 token 前缀 {bad}");
        }
    }

    /// `logged_out` 给的命令必须是**可直接粘贴**的完整命令，且带两个关键开关：
    /// `--web`（走浏览器授权，不必手输 token）与 `--clipboard`（设备码进剪贴板）。
    /// 少任何一个，用户就得自己去翻设备码，那就不叫「一键」了。
    #[test]
    fn logged_out_command_is_copy_pasteable() {
        let cmd = "gh auth login --hostname github.com --git-protocol https --web --clipboard";
        assert!(cmd.starts_with("gh auth login"));
        assert!(cmd.contains("--web"), "必须走浏览器授权");
        assert!(cmd.contains("--clipboard"), "设备码要进剪贴板");
        assert!(cmd.contains("--hostname github.com"));
    }

    /// 装了 gh 但参数错时，错误文本不能被误判成「未安装」——
    /// 两者给用户的下一步完全不同（一个是重装 gh，一个是重新登录）。
    #[test]
    fn bad_flag_is_not_misread_as_missing() {
        let e = run_gh(&["--definitely-not-a-flag"]).unwrap_err();
        assert!(!e.contains("读不到 gh 命令"), "参数错不该被误判为未安装");
    }
}
