//! W17 多窗口多实例：host spawn 自身起新实例（VSCode 式多窗口）。
//!
//! 每个「窗口」= 一个独立 host 进程 = 独立端口 + 独立会话数据 + 绑定一个工作区。
//! 代码定位（plugins/agent-kernel/web-dist/skills 缺省）锚定编译期 workspace_dir()，
//! 多实例共享且共享正确；需要隔离的只有三个数据路径（memory/stream/config）+ 端口。
//!
//! 实例目录：`<代码根>/.instances/<名>/`——memory/、stream/、config.json、instance.json。
//! 用户项目目录零污染（数据统一挂代码根，.gitignore 排除）。
//!
//! 实例平等：每个实例都带同一套 /api/instances API，任意窗口都能再开新窗口。
//! 实例名自动取工作区文件夹名（前端免填）；同一工作区重复创建 → 在线即复用（返回
//! 原 url），离线即复活（换新端口，会话/config 保留）；主实例启动恢复上次工作区
//! （.last-workspace，随每次启动记忆）。
//!
//! 复活语义（迭代九）：meta 存在 = 想要该实例。主实例启动时对「未手工停止且已死」
//! 的注册实例自动重新 spawn（换新端口，memory/config/会话延续）；DELETE 手工停止
//! 打 stopped 标记 → 不自动复活（同工作区 POST 复活路径重写 meta 自然清除标记）。
//! 探活必须验身份：仅 TCP 连通会「探到别人家」（重启后端口被其他实例/进程复用），
//! 以 `/api/config` 回显的 instance_name 比对为准。start-window.cmd 直启的实例不注册
//! instance.json（手工生命周期），不参与自动复活。

use serde_json::{json, Value};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 实例注册表与数据根：`<代码根>/.instances`。
pub fn instances_root() -> PathBuf {
    crate::config::workspace_dir().join(".instances")
}

fn instance_dir(name: &str) -> PathBuf {
    instances_root().join(name)
}

/// 上次工作区记忆文件（`.instances/.last-workspace`，内容=路径一行）。
/// 主实例启动：env 未设 WORKSPACE_ROOT 时恢复它；任何实例启动后都回写（下次启动即上次工作区）。
fn last_workspace_file() -> PathBuf {
    instances_root().join(".last-workspace")
}

/// 记忆当前工作区（启动时调用；失败仅告警不阻断——记忆是增强不是依赖）。
/// canonicalize 产生的 `\\?\` 扩展长度前缀在此剥离（回显友好，is_dir/新建实例均可用）。
pub fn remember_workspace(ws: &str) {
    let ws = ws.strip_prefix(r"\\?\").unwrap_or(ws);
    let f = last_workspace_file();
    if let Some(p) = f.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    match std::fs::write(&f, ws) {
        Ok(_) => tracing::info!("已记忆工作区: {ws}"),
        Err(e) => tracing::warn!("记忆工作区失败（忽略）: {e}"),
    }
}

/// 恢复上次工作区（主实例启动、env 未显式指定时调用）；无记忆或目录已消失 → None。
pub fn recall_workspace() -> Option<String> {
    let ws = std::fs::read_to_string(last_workspace_file()).ok()?;
    let ws = ws.trim().to_string();
    if !ws.is_empty() && Path::new(&ws).is_dir() {
        Some(ws)
    } else {
        None
    }
}

/// 实例名规则：1-32 字符，不含路径/Windows 非法字符与控制字符，不为 `.`/`..`，首尾无空格/点
/// （= Windows 目录名安全子集；允许中文等 Unicode——实例名自动取工作区文件夹名）。
pub fn validate_name(name: &str) -> Result<(), String> {
    let invalid = |c: char| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control();
    let ok = !name.is_empty()
        && name.chars().count() <= 32
        && name != "." && name != ".."
        && !name.starts_with([' ', '.'])
        && !name.ends_with([' ', '.'])
        && !name.chars().any(invalid);
    if ok {
        Ok(())
    } else {
        Err("实例名需 1-32 字符，不含 / \\ : * ? \" < > | 与控制字符，首尾无空格/点".into())
    }
}

/// 实例名自动派生：取工作区文件夹名（路径末段），替换非法字符，空则回退 `ws`（超长截 32）。
pub fn derive_name(workspace: &str) -> String {
    let last = workspace
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim();
    // 驱动器根（"C:"）派生无意义 → 回退
    if last.len() == 2 && last.as_bytes()[1] == b':' && last.as_bytes()[0].is_ascii_alphabetic() {
        return "ws".to_string();
    }
    let name: String = last
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    let name = name.trim_matches([' ', '.']);
    let mut name = if name.is_empty() { "ws".to_string() } else { name.to_string() };
    if name.chars().count() > 32 {
        name = name.chars().take(32).collect();
    }
    name
}

/// 端口探测：从 start 起递增，找到第一个能 bind 的端口（防与主实例/其他实例冲突）。
pub fn find_free_port(start: u16) -> Option<u16> {
    (start..start + 200).find(|&p| TcpListener::bind(("127.0.0.1", p)).is_ok())
}

/// 主 config.json（编译期代码根下）：建实例时复制一份，key 免重配。
fn main_config_file() -> PathBuf {
    crate::config::config_file()
}

/// 读实例注册信息（instance.json）。
fn read_instance_meta(dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join("instance.json")).ok()?).ok()
}

/// TCP 探活（connect 无超时版，仅本地端口环回，开销可忽略）。
fn port_alive(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port))
        .map(|_| true)
        .unwrap_or(false)
}

/// 身份探活：端口连通 **且** 该端口服务的 `/api/config` 回显的 instance_name 与期望一致。
/// 仅 TCP 探活会「探到别人家」（重启后端口被其他实例/进程复用 → 假在线、复用分派
/// 指错窗口），必须验身份——子实例启动带 REACT_INSTANCE_NAME，网关 /api/config 回显。
/// 同步实现（读超时 1.5s 兜底；connection: close 对端写完即关）。
fn identity_alive(port: u16, expect_name: &str) -> bool {
    use std::io::{Read, Write};
    if port == 0 || expect_name.is_empty() {
        return false;
    }
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
    let req = format!("GET /api/config HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf); // 超时按已收字节判定（响应小，通常已完整）
    String::from_utf8_lossy(&buf).contains(&format!("\"instance_name\":\"{expect_name}\""))
}

/// 工作区路径规范化比较（casefold + 尾部斜杠归一；仅用于同名冲突判定，不做文件操作）。
fn same_workspace(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches(['/', '\\']).to_lowercase();
    norm(a) == norm(b)
}

/// spawn 自身 exe 为子实例（env 继承后覆盖 7 项，见 create_instance 文档）。返回 pid。
/// 首建（create_instance）与复活（revive_instances）共用：实例目录/config 由调用方准备
/// （复活保留旧 config/会话），meta 落盘亦归调用方（两者 meta 字段口径不同）。
fn spawn_self(dir: &Path, name: &str, workspace: &str, port: u16) -> Result<u64, String> {
    let ws = PathBuf::from(workspace);
    let exe = std::env::current_exe().map_err(|e| format!("定位自身 exe 失败: {e}"))?;
    let child = std::process::Command::new(&exe)
        .env("REACT_FRONTEND", "web")
        .env("WORKSPACE_ROOT", ws.canonicalize().unwrap_or(ws))
        .env("WEB_ADDR", format!("127.0.0.1:{port}"))
        .env("MEMORY_DATA_DIR", dir.join("memory"))
        .env("AGENT_STREAM_DIR", dir.join("stream"))
        .env("CONFIG_FILE", dir.join("config.json"))
        .env("REACT_INSTANCE_NAME", name)
        .spawn()
        .map_err(|e| format!("spawn 实例进程失败: {e}"))?;
    Ok(u64::from(child.id()))
}

/// 建实例并 spawn 自身（`name` 传 None → 自动取工作区文件夹名）。同步实现（含就绪等待，
/// 最多 ~15s），调用方需放 spawn_blocking。
///
/// 同名目录已存在时（目录即注册表）：
/// - meta.workspace 与本次相同且在线（身份探活）→ 直接复用（返回原 url，不重复 spawn）
/// - meta.workspace 与本次相同但离线 → 复活（换新端口，memory/config 保留，会话延续；
///   stopped 标记随 meta 重写自然清除）
/// - meta.workspace 与本次不同 → 名字尾部加 `-2`/`-3`… 找空位
/// - 目录存在但无 instance.json（脏目录/start-window.cmd 手工实例）→ 视为被占加后缀
///   （不收编：无 meta 即无法身份探活，避免与手工实例双进程同目录互踩）
///
/// spawn 自身 env 继承父进程后覆盖 7 项：WORKSPACE_ROOT / WEB_ADDR / MEMORY_DATA_DIR /
/// AGENT_STREAM_DIR / CONFIG_FILE / REACT_INSTANCE_NAME / REACT_FRONTEND。
/// 其余（LLM_*、SEARCH_* 等）照常继承——子实例默认同模型配置（config.json 亦复制）。
pub fn create_instance(name: Option<&str>, workspace: &str) -> Result<Value, String> {
    let ws = PathBuf::from(workspace);
    if !ws.is_dir() {
        return Err(format!("工作区路径不存在或不是目录: {workspace}"));
    }
    // ── 定名：显式指定 → 校验；否则自动派生 + 同名冲突分派（复用/复活/加后缀）──
    let name = match name {
        Some(n) => {
            validate_name(n)?;
            n.to_string()
        }
        None => {
            let base = derive_name(workspace);
            let dir = instance_dir(&base);
            if !dir.exists() {
                base
            } else if let Some(meta) = read_instance_meta(&dir) {
                let prev = meta.get("workspace").and_then(Value::as_str).unwrap_or("");
                if same_workspace(prev, workspace) {
                    let port = meta.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
                    if identity_alive(port, &base) {
                        // 同工作区已在线：直接复用（前窗重开同一项目不重复起进程）
                        return Ok(json!({
                            "ok": true, "reused": true, "name": base, "port": port,
                            "url": format!("http://127.0.0.1:{port}"),
                            "pid": meta.get("pid").and_then(Value::as_u64).unwrap_or(0),
                        }));
                    }
                    base // 同工作区离线 → 复活
                } else {
                    // 同名不同工作区：加后缀找空位
                    (2..100)
                        .find(|i| !instance_dir(&format!("{base}-{i}")).exists())
                        .map(|i| format!("{base}-{i}"))
                        .ok_or("实例目录命名冲突（2-99 后缀均被占用）")?
                }
            } else {
                // 目录存在但无 instance.json（脏目录）：视为被占，加后缀
                (2..100)
                    .find(|i| !instance_dir(&format!("{base}-{i}")).exists())
                    .map(|i| format!("{base}-{i}"))
                    .ok_or("实例目录命名冲突（2-99 后缀均被占用）")?
            }
        }
    };
    let dir = instance_dir(&name);
    let port = find_free_port(8711).ok_or("8711-8910 无空闲端口")?;
    std::fs::create_dir_all(dir.join("memory")).map_err(|e| format!("建实例目录失败: {e}"))?;
    std::fs::create_dir_all(dir.join("stream")).map_err(|e| format!("建实例目录失败: {e}"))?;
    // 首次建窗：复制主 config.json（独立热写域，key 免重配）；复活保留旧 config
    let cfg = dir.join("config.json");
    if !cfg.exists() {
        let main_cfg = main_config_file();
        if main_cfg.exists() {
            std::fs::copy(&main_cfg, &cfg).map_err(|e| format!("复制主 config 失败: {e}"))?;
        }
    }
    let pid = spawn_self(&dir, &name, workspace, port)?;
    let meta = json!({
        "name": name,
        "pid": pid,
        "port": port,
        "workspace": workspace,
        "created_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    });
    std::fs::write(dir.join("instance.json"), serde_json::to_string_pretty(&meta).unwrap())
        .map_err(|e| format!("写 instance.json 失败: {e}"))?;
    remember_workspace(workspace); // 下次主实例启动默认恢复该工作区
    // 等待子实例 HTTP 就绪（最多 ~15s），避免前端打开过早拿连接拒绝
    for _ in 0..75 {
        if port_alive(port) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(json!({"ok": true, "name": name, "port": port, "url": format!("http://127.0.0.1:{port}"), "pid": pid}))
}

/// 弹原生文件夹选择对话框（host 所在机器；PowerShell STA FolderBrowserDialog）。
/// 用户取消/无选择 → Err。同步阻塞直至用户操作完毕，调用方需放 spawn_blocking。
///
/// 结果走临时文件而非 stdout：外层受限环境会向子进程 stdout 追加自身错误文本
/// 且无分隔符直拼（曾把沙箱的 "can't open file for write !" 直接拼进路径尾部），
/// stdout 解析不可靠——只认结果文件，stdout/stderr 全部忽略。
pub fn pick_folder() -> Result<String, String> {
    let out_file = std::env::temp_dir().join(format!(
        "react-agent-pick-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&out_file); // 清理同参残留（幂等）
    let out_ps = out_file.to_string_lossy().replace('\'', "''");
    // 置顶两代方案（用户实测第一代「时好时坏」）：
    // 一代：owner 窗体 Show()+Focus() 抢前台 → 依赖 Windows 前台锁放行。后台进程（host spawn
    //      的 powershell）申请激活**可能被防抢焦点机制拒绝**（取决于点击后 1-2s 内用户是否
    //      有其他焦点操作）→ 激活被拒时弹窗落在非置顶层被浏览器压住——竞态，随机复现。
    // 二代（当前）：**不再依赖前台激活**。SetWindowPos(HWND_TOPMOST) 无需前台权限，后台进程
    //      可钉自己的窗口 → ShowDialog 期间用 Timer 每 150ms 经 EnumWindows 找出 $top 的
    //      owned 可见窗（即对话框本体，GetWindow(GW_OWNER)==$top 精确圈定，绝不误伤浏览器）
    //      钉到 HWND_TOPMOST。$top.Show()+Focus() 保留（激活放行时顺带拿到焦点）。
    // 脚本用 raw string + __OUT__ 占位替换（内含 C# 块大量花括号，format! 转义不可读）。
    let script = r#"
Add-Type -AssemblyName System.Windows.Forms | Out-Null
Add-Type -AssemblyName System.Drawing | Out-Null
$top = New-Object System.Windows.Forms.Form
$top.TopMost = $true; $top.ShowInTaskbar = $false; $top.FormBorderStyle = 'None'
$top.Size = New-Object System.Drawing.Size(1, 1)
$top.StartPosition = 'Manual'; $top.Location = New-Object System.Drawing.Point(-32000, -32000)
$top.Show(); $top.Focus() | Out-Null
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class WinPin {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lp);
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr hWnd, uint uCmd);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
}
'@
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 150
$timer.Add_Tick({
  $cb = [WinPin+EnumWindowsProc]{ param($h, $lp)
    if (([WinPin]::GetWindow($h, 4) -eq $top.Handle) -and [WinPin]::IsWindowVisible($h)) {
      [WinPin]::SetWindowPos($h, [IntPtr](-1), 0, 0, 0, 0, 19) | Out-Null
    }
    $true }
  [WinPin]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
})
$timer.Start()
$d = New-Object System.Windows.Forms.FolderBrowserDialog
$d.Description = '选择工作区文件夹'; $d.ShowNewFolderButton = $true
$r = $d.ShowDialog($top)
$timer.Stop(); $top.Hide()
if ($r -eq [System.Windows.Forms.DialogResult]::OK) {
[System.IO.File]::WriteAllText('__OUT__', $d.SelectedPath) }
"#
    .replace("__OUT__", &out_ps);
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-STA", "-WindowStyle", "Hidden", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW：GUI 进程 spawn 控制台程序不闪黑框
        .output()
        .map_err(|e| format!("启动文件夹选择器失败: {e}"))?;
    let path = std::fs::read_to_string(&out_file)
        .unwrap_or_default()
        .trim_start_matches('\u{feff}') // 防编码器写 BOM
        .trim()
        .to_string();
    let _ = std::fs::remove_file(&out_file);
    if path.is_empty() {
        if out.status.success() {
            return Err("未选择文件夹".into());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("文件夹选择器异常: {}", err.trim()));
    }
    Ok(path)
}

/// 实例清单：扫 .instances/ 读 instance.json + 探活标注 online/offline。
pub fn list_instances() -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(instances_root()) else {
        return out;
    };
    for entry in rd.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(mut meta) = read_instance_meta(&entry.path()) else {
            continue;
        };
        // online = 身份探活（端口连通且 /api/config 回显的实例名一致）——仅测端口会
        // 「探到别人家」：重启后端口被其他实例/进程复用时假报在线（迭代五遗留观察）
        let name = meta.get("name").and_then(Value::as_str).unwrap_or("");
        let port = meta.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
        let online = identity_alive(port, name);
        meta["online"] = json!(online);
        meta["url"] = json!(format!("http://127.0.0.1:{port}"));
        out.push(meta);
    }
    out.sort_by(|a, b| {
        a.get("name").and_then(Value::as_str).unwrap_or("").cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
    out
}

/// 停止实例：杀 instance.json 记录的 pid，并打 `stopped:true` 标记（数据目录保留；
/// 主实例启动的自动复活跳过手工停止的实例，同工作区 POST 复活路径重写 meta 清除标记）。
pub fn stop_instance(name: &str) -> Result<Value, String> {
    validate_name(name)?;
    let dir = instance_dir(name);
    let mut meta = read_instance_meta(&dir).ok_or_else(|| format!("实例 {name} 不存在"))?;
    let pid = meta.get("pid").and_then(Value::as_u64).ok_or("instance.json 缺 pid")?;
    let out = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()
        .map_err(|e| format!("taskkill 失败: {e}"))?;
    if !out.status.success() {
        // 进程可能已退出（探活误报/用户手工关）——清 pid 视为已停
        tracing::warn!("taskkill pid {pid} 非零退出（可能已退出）: {}", String::from_utf8_lossy(&out.stderr));
    }
    meta["stopped"] = json!(true); // 手工停止语义：不参与启动自动复活（写失败仅影响该语义）
    let _ = std::fs::write(dir.join("instance.json"), serde_json::to_string_pretty(&meta).unwrap_or_default());
    Ok(json!({"ok": true, "name": name, "stopped": pid}))
}

/// 主实例启动自动复活（W17 迭代九）：扫 `.instances/`，对「未手工停止（无 stopped
/// 标记）且已死（身份探活失败）」的注册实例按原目录/工作区重新 spawn——换新端口，
/// memory/config/会话延续（数据目录即注册表，复活沿用原 dir）。
/// 后台线程调用：串行 spawn + 逐个就绪等待（防端口探测竞态与启动风暴），不阻塞主实例。
/// 复活不改 .last-workspace（那是主实例自己的工作区记忆）。
pub fn revive_instances() -> usize {
    let Ok(rd) = std::fs::read_dir(instances_root()) else {
        return 0;
    };
    let mut revived = 0usize;
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(meta) = read_instance_meta(&dir) else {
            continue;
        };
        let name = meta.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let port = meta.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
        let ws = meta.get("workspace").and_then(Value::as_str).unwrap_or("").to_string();
        if name.is_empty() || ws.is_empty() || validate_name(&name).is_err() {
            continue;
        }
        if meta.get("stopped").and_then(Value::as_bool).unwrap_or(false) {
            tracing::info!("实例 {name} 曾手工停止，跳过自动复活");
            continue;
        }
        if identity_alive(port, &name) {
            continue; // 仍在运行（杀主不杀子：子进程独立于父，存活则无需复活）
        }
        if !Path::new(&ws).is_dir() {
            tracing::warn!("实例 {name} 工作区已消失，跳过自动复活: {ws}");
            continue;
        }
        let Some(new_port) = find_free_port(8711) else {
            tracing::warn!("实例 {name} 复活失败：8711-8910 无空闲端口");
            continue;
        };
        match spawn_self(&dir, &name, &ws, new_port) {
            Ok(pid) => {
                // meta 回写新 pid/port（保留 created_at）；重写不带 stopped → 标记自然清除
                let mut m = meta.clone();
                m["pid"] = json!(pid);
                m["port"] = json!(new_port);
                let _ = std::fs::write(
                    dir.join("instance.json"),
                    serde_json::to_string_pretty(&m).unwrap_or_default(),
                );
                tracing::info!("已复活实例 {name}: http://127.0.0.1:{new_port} (pid {pid})");
                revived += 1;
            }
            Err(e) => tracing::warn!("实例 {name} 复活失败: {e}"),
        }
        // 就绪等待（最多 ~15s）再轮到下一个
        for _ in 0..75 {
            if identity_alive(new_port, &name) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    if revived > 0 {
        tracing::info!("子实例自动复活完成: {revived} 个");
    }
    revived
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 实例名校验_含中文() {
        assert!(validate_name("b").is_ok());
        assert!(validate_name("ws-2_3").is_ok());
        assert!(validate_name("AI错误收集").is_ok()); // 自动派生自中文文件夹名
        assert!(validate_name("my-app").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err()); // 路径分隔符拒绝（目录名安全底线）
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a b c").is_ok()); // 中间空格允许（合法目录名）
        assert!(validate_name(" a").is_err()); // 首空格拒绝
        assert!(validate_name(&"好".repeat(33)).is_err()); // 超长（chars 计数）
    }

    #[test]
    fn 实例名派生_取文件夹名() {
        assert_eq!(derive_name("D:\\projects\\my-app"), "my-app");
        assert_eq!(derive_name("/home/u/AI错误收集/"), "AI错误收集");
        assert_eq!(derive_name("C:\\"), "ws"); // 空末段回退
        assert_eq!(derive_name("D:/a/b/trailing..."), "trailing"); // 尾点裁剪（Windows 目录名限制）
        assert_eq!(derive_name("D:\\x\\a*b?c"), "a-b-c"); // 非法字符替换
        assert_eq!(derive_name(&format!("D:\\{}", "长".repeat(40))), "长".repeat(32)); // 截 32
    }

    #[test]
    fn 工作区相同判定_大小写与尾斜杠() {
        assert!(same_workspace("C:\\A\\b", "c:\\a\\b\\"));
        assert!(!same_workspace("C:\\A\\b", "C:\\A\\b2"));
    }

    #[test]
    fn 端口探测返回可绑定端口() {
        let p = find_free_port(8711).expect("应找到空闲端口");
        assert!(TcpListener::bind(("127.0.0.1", p)).is_ok());
    }

    /// 身份探活：起一个只会回固定 /api/config 响应的假服务（两次 accept 供两次探活）。
    #[test]
    fn 身份探活_名字匹配才在线() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let body = r#"{"ok":true,"config":{"instance_name":"ws-a","model":"m"}}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let h = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok((mut s, _)) = l.accept() {
                    use std::io::{Read, Write};
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf);
                    let _ = s.write_all(resp.as_bytes());
                } // s 在此 drop → connection close → 客户端 read_to_end 收尾
            }
        });
        assert!(identity_alive(port, "ws-a"), "名字一致 → 在线");
        assert!(!identity_alive(port, "ws-b"), "名字不符 = 别人家，必须离线");
        h.join().unwrap();
    }

    #[test]
    fn 身份探活_端口未监听即离线() {
        // bind 后立刻 drop：拿到一个当前必然无人监听的端口
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(!identity_alive(port, "ws-a"));
        assert!(!identity_alive(0, "ws-a")); // port=0 卫语句
        assert!(!identity_alive(port, "")); // 空名卫语句
    }
}
