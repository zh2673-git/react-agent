//! sandbox-run：bash 进程沙箱助手（宿主层 fail-closed，05 §2.1）。
//!
//! 用法：
//!   sandbox-run probe                          # 探测受限令牌可用（exit 0 = 可用）
//!   sandbox-run exec <timeout_ms> <command>    # 受限令牌下经 cmd /c 执行，stdout 输出单行 JSON
//!
//! 成功（子进程已执行，无论其退出码）：exit 0 + stdout 单行 JSON：
//!   {"exit_code":u32,"timeout":bool,"output_b64":"<base64>"}   （base64 由调用方按本地编码解码）
//! 沙箱建立失败（fail-closed，绝不回退无沙箱执行）：
//!   exit 3 = 平台不支持；exit 4 = 受限令牌创建/进程创建失败；exit 2 = 用法错误。错误细节走 stderr。

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("probe") => std::process::exit(exec(5_000, "exit 0")),
        Some("exec") => {
            let Some(timeout_ms) = args.get(1).and_then(|s| s.parse::<u32>().ok()) else {
                eprintln!("用法: sandbox-run exec <timeout_ms> <command>");
                std::process::exit(2);
            };
            let command = args[2..].join(" ");
            if command.trim().is_empty() {
                eprintln!("用法: sandbox-run exec <timeout_ms> <command>（command 为空）");
                std::process::exit(2);
            }
            std::process::exit(exec(timeout_ms, &command));
        }
        _ => {
            eprintln!("用法: sandbox-run probe | exec <timeout_ms> <command>");
            std::process::exit(2);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    // fail-closed：非 Windows 平台无受限令牌实现，拒绝执行（绝不静默回退无沙箱直跑）。
    eprintln!("sandbox-run: 当前平台无受限令牌沙箱实现（fail-closed）。如接受无沙箱运行请设 BASH_SANDBOX=off");
    std::process::exit(3);
}

/// 受限令牌级别：NORMALUSER（剥离管理员 SID 与高危特权，仍为本用户身份）。
/// 更严的 CONSTRAINED（SRP 白名单）在默认策略下会拦掉一切子进程，不可用为默认档。
#[cfg(windows)]
const SAFER_LEVEL: u32 = 0x20000; // SAFER_LEVELID_NORMALUSER

#[cfg(windows)]
fn exec(timeout_ms: u32, command: &str) -> i32 {
    use std::mem::size_of;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{BOOL, CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};
    use windows::Win32::Security::AppLocker::{
        SaferCloseLevel, SaferComputeTokenFromLevel, SaferCreateLevel,
        SAFER_COMPUTE_TOKEN_FROM_LEVEL_FLAGS, SAFER_SCOPEID_USER,
    };
    use windows::Win32::Security::{SAFER_LEVEL_HANDLE, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    unsafe {
        // 1. 受限令牌（fail-closed：建不出来就拒绝执行）。NORMALUSER 从调用者自身令牌
        //    派生受限版本，CreateProcessAsUserW 因此不需要 SE_ASSIGNPRIMARYTOKEN 特权。
        let mut level = SAFER_LEVEL_HANDLE::default();
        if let Err(e) = SaferCreateLevel(SAFER_SCOPEID_USER, SAFER_LEVEL, 0, &mut level, None) {
            eprintln!("sandbox-run: SaferCreateLevel 失败: {e}");
            return 4;
        }
        let mut token = HANDLE::default();
        let computed = SaferComputeTokenFromLevel(level, HANDLE::default(), &mut token, SAFER_COMPUTE_TOKEN_FROM_LEVEL_FLAGS(0), None);
        let _ = SaferCloseLevel(level);
        if let Err(e) = computed {
            eprintln!("sandbox-run: 受限令牌创建失败: {e}");
            return 4;
        }

        // 2. 输出管道：同一写端同时作子进程 stdout/stderr（合并语义与直跑版一致）。
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: BOOL(1),
            lpSecurityDescriptor: std::ptr::null_mut(),
        };
        let (mut read_end, mut write_end) = (HANDLE::default(), HANDLE::default());
        if let Err(e) = CreatePipe(&mut read_end, &mut write_end, Some(&sa), 0) {
            eprintln!("sandbox-run: CreatePipe 失败: {e}");
            return 4;
        }
        let _ = SetHandleInformation(write_end, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);

        // 3. 经 cmd /c 在受限令牌下执行（cwd 继承助手进程 = WORKSPACE_ROOT）。
        let mut cmdline: Vec<u16> = format!("cmd.exe /c {command}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut si = STARTUPINFOW::default();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = write_end;
        si.hStdError = write_end;
        let mut pi = PROCESS_INFORMATION::default();
        let created = CreateProcessAsUserW(
            token,
            PCWSTR::null(),
            PWSTR(cmdline.as_mut_ptr()),
            None,
            None,
            true,
            Default::default(),
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        );
        let _ = CloseHandle(write_end); // 父端副本必须关闭，读端才有 EOF
        if let Err(e) = created {
            eprintln!("sandbox-run: 受限令牌下进程创建失败: {e}");
            let _ = CloseHandle(read_end);
            let _ = CloseHandle(token);
            return 4;
        }
        let _ = CloseHandle(token);
        let _ = CloseHandle(pi.hThread);

        // 4. 后台读管道（子进程输出可超管道缓冲，必须边读边等），主线程限时等待。
        //    HANDLE 非 Send，线程间只传裸 usize。
        let read_raw = read_end.0 as usize;
        let reader = std::thread::spawn(move || {
            let read_end = HANDLE(read_raw as *mut core::ffi::c_void);
            let mut out = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let mut n: u32 = 0;
                match ReadFile(read_end, Some(&mut buf), Some(&mut n), None) {
                    Ok(()) if n > 0 => out.extend_from_slice(&buf[..n as usize]),
                    _ => break, // EOF / broken pipe
                }
            }
            let _ = CloseHandle(read_end);
            out
        });
        let mut timed_out = false;
        let wait = WaitForSingleObject(pi.hProcess, timeout_ms);
        if wait.0 == 0x102 {
            // WAIT_TIMEOUT
            timed_out = true;
            let _ = TerminateProcess(pi.hProcess, 1);
        }
        let output = reader.join().unwrap_or_default();
        let mut exit_code: u32 = 1;
        let _ = GetExitCodeProcess(pi.hProcess, &mut exit_code);
        let _ = CloseHandle(pi.hProcess);

        // 5. 单行 JSON 结果（base64：由调用方按本地编码解码，保持与直跑版一致的 GBK/UTF-8 行为）
        println!(
            "{{\"exit_code\":{},\"timeout\":{},\"output_b64\":\"{}\"}}",
            exit_code,
            timed_out,
            b64(&output)
        );
        0
    }
}

/// 手写标准 base64（免引入依赖；仅编码，调用方解码）。
#[cfg(windows)]
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = ((c[0] as u32) << 16) | ((*c.get(1).unwrap_or(&0) as u32) << 8) | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18 & 63) as usize] as char);
        s.push(T[(n >> 12 & 63) as usize] as char);
        s.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        s.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    s
}

#[cfg(windows)]
#[test]
fn b64_matches_known_vectors() {
    assert_eq!(b64(b""), "");
    assert_eq!(b64(b"f"), "Zg==");
    assert_eq!(b64(b"fo"), "Zm8=");
    assert_eq!(b64(b"foo"), "Zm9v");
}
