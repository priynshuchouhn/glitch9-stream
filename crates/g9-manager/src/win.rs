//! Windows implementation: enumerate RDP sessions via the WTS API and launch one
//! engine per gamer session, directly in that session, using the session's user
//! token (`WTSQueryUserToken` + `CreateProcessAsUserW`). Must run elevated (SYSTEM
//! or admin) to obtain other sessions' tokens.

use crate::cli::{Config, GamerSession};
use anyhow::{Context, Result};
use std::ffi::c_void;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_ATTRIBUTE_NORMAL,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    WTSEnumerateSessionsW, WTSFreeMemory, WTSQuerySessionInformationW, WTSQueryUserToken,
    WTSUserName, WTSActive, WTS_SESSION_INFOW, WTS_CURRENT_SERVER_HANDLE,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// Enumerate Active sessions whose username matches the configured pattern.
pub fn enumerate_gamer_sessions(cfg: &Config) -> Result<Vec<GamerSession>> {
    let mut out = Vec::new();
    unsafe {
        let mut info_ptr: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count: u32 = 0;
        WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info_ptr, &mut count)
            .context("WTSEnumerateSessionsW")?;
        let infos = std::slice::from_raw_parts(info_ptr, count as usize);
        for info in infos {
            if info.State != WTSActive {
                continue;
            }
            if let Some(user) = query_username(info.SessionId) {
                if matches_pattern(&user, &cfg.user_pattern) {
                    out.push(GamerSession {
                        id: info.SessionId,
                        user,
                    });
                }
            }
        }
        WTSFreeMemory(info_ptr as *mut c_void);
    }
    out.sort_by_key(|s| s.id);
    Ok(out)
}

/// Read the username for a session (WTSUserName). Returns None if empty/unavailable.
unsafe fn query_username(session_id: u32) -> Option<String> {
    let mut buf: PWSTR = PWSTR::null();
    let mut bytes: u32 = 0;
    if WTSQuerySessionInformationW(
        WTS_CURRENT_SERVER_HANDLE,
        session_id,
        WTSUserName,
        &mut buf,
        &mut bytes,
    )
    .is_err()
        || buf.is_null()
    {
        return None;
    }
    let s = buf.to_string().ok().filter(|s| !s.is_empty());
    WTSFreeMemory(buf.as_ptr() as *mut c_void);
    s
}

/// Minimal matcher for the usernames we care about. Supports the default
/// `^gamer\d+$` (a "gamer" prefix followed by digits) without pulling in a regex
/// crate; any other pattern is treated as a case-insensitive substring match.
fn matches_pattern(user: &str, pattern: &str) -> bool {
    let u = user.to_ascii_lowercase();
    if pattern == r"^gamer\d+$" {
        u.strip_prefix("gamer")
            .map(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
    } else {
        // Strip common regex anchors and treat the rest as a substring.
        let p = pattern.trim_start_matches('^').trim_end_matches('$').to_ascii_lowercase();
        u.contains(&p)
    }
}

/// Launch the engine inside `session` using that session's user token.
fn launch_in_session(session: &GamerSession, cfg: &Config) -> Result<u32> {
    let port = session.port(cfg.base_port);
    let dir = cfg.log_dir.trim_end_matches('\\');
    let log = format!("{dir}\\session-{}.log", session.id);

    // Launch the ENGINE DIRECTLY — not via cmd.exe. On locked-down gaming hosts,
    // group policy / AppLocker often blocks cmd.exe for gamer accounts (observed:
    // 0x800704EC "blocked by group policy"), but the engine .exe is allowed. So:
    //   - lpApplicationName = engine path (no shell),
    //   - G9_PUBLIC_IP injected into the user's environment block,
    //   - stdout/stderr redirected to the per-session log via STARTUPINFO handles.
    let cmdline = format!(
        "\"{engine}\" --bind 0.0.0.0 --port {port} --display 0 --width {w} --height {h} \
         --fps {fps} --bitrate {br} --audio true",
        engine = cfg.engine, port = port, w = cfg.width, h = cfg.height,
        fps = cfg.fps, br = cfg.bitrate,
    );

    unsafe {
        // Session user token → process runs in that session/desktop.
        let mut token: HANDLE = HANDLE::default();
        WTSQueryUserToken(session.id, &mut token)
            .with_context(|| format!("WTSQueryUserToken(session {})", session.id))?;

        // Environment block for the user, then force G9_PUBLIC_IP into it. The block
        // is a double-null-terminated list of "NAME=VALUE\0"; we rebuild it with our
        // var appended.
        let mut env: *mut c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, token, false).is_ok();
        let mut env_vec = build_env_with(env, "G9_PUBLIC_IP", &cfg.public_ip);
        if have_env && !env.is_null() {
            let _ = DestroyEnvironmentBlock(env);
        }

        // Inheritable log file handle for stdout+stderr.
        let mut sa = SECURITY_ATTRIBUTES::default();
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = true.into();
        let log_w: Vec<u16> = log.encode_utf16().chain(std::iter::once(0)).collect();
        let log_handle = CreateFileW(
            PCWSTR(log_w.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&sa),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            HANDLE::default(),
        )
        .with_context(|| format!("CreateFileW({log})"))?;

        let mut cmd_utf16: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        let mut si = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        si.lpDesktop = PWSTR(desktop.as_mut_ptr());
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = log_handle;
        si.hStdError = log_handle;
        let mut pi = PROCESS_INFORMATION::default();

        let mut cwd_utf16: Vec<u16> =
            dir.encode_utf16().chain(std::iter::once(0)).collect();

        let flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;
        let result = CreateProcessAsUserW(
            token,
            PCWSTR::null(),
            PWSTR(cmd_utf16.as_mut_ptr()),
            None,
            None,
            true, // inherit handles (the log file)
            flags,
            Some(env_vec.as_mut_ptr() as *const c_void),
            PCWSTR(cwd_utf16.as_mut_ptr()),
            &si,
            &mut pi,
        );

        let _ = CloseHandle(log_handle);
        let _ = CloseHandle(token);

        result.with_context(|| format!("CreateProcessAsUserW(session {})", session.id))?;
        let pid = pi.dwProcessId;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        Ok(pid)
    }
}

/// Copy the user's environment block (double-null-terminated UTF-16 "NAME=VALUE"
/// entries) and append/override `name=value`. Returns a fresh double-null block.
unsafe fn build_env_with(env: *mut c_void, name: &str, value: &str) -> Vec<u16> {
    let mut entries: Vec<Vec<u16>> = Vec::new();
    if !env.is_null() {
        let p = env as *const u16;
        let mut i = 0isize;
        loop {
            // Read one null-terminated entry.
            let start = i;
            while *p.offset(i) != 0 {
                i += 1;
            }
            if i == start {
                break; // empty entry => end of block
            }
            let len = (i - start) as usize;
            let slice = std::slice::from_raw_parts(p.offset(start), len);
            entries.push(slice.to_vec());
            i += 1; // skip the null
        }
    }
    let upper = format!("{}=", name).to_ascii_uppercase();
    entries.retain(|e| {
        let s = String::from_utf16_lossy(e);
        !s.to_ascii_uppercase().starts_with(&upper)
    });
    entries.push(format!("{name}={value}").encode_utf16().collect());

    let mut block: Vec<u16> = Vec::new();
    for e in entries {
        block.extend_from_slice(&e);
        block.push(0);
    }
    block.push(0); // final terminating null
    block
}

const TASK_NAME: &str = "glitch9-broadcast";

/// Public `start`: usable by a NON-SYSTEM admin (e.g. g9admin). Since
/// WTSQueryUserToken needs SE_TCB (SYSTEM-only), a non-SYSTEM caller can't launch
/// into other sessions directly. If we're SYSTEM, do it directly; otherwise
/// trigger the SYSTEM scheduled task created by `deploy`.
pub fn start(cfg: &Config) -> Result<()> {
    if is_system() {
        return start_system(cfg);
    }
    tracing::info!("not running as SYSTEM; triggering the '{TASK_NAME}' SYSTEM task to start broadcasts");
    let run_st = std::process::Command::new("schtasks")
        .args(["/run", "/tn", TASK_NAME])
        .status()
        .context("schtasks /run")?;
    if !run_st.success() {
        anyhow::bail!(
            "could not run the '{TASK_NAME}' task. Run `glitch9-manager deploy` once \
             (as an admin) to register it, then retry `start`."
        );
    }
    // Give the task a moment, then report status.
    std::thread::sleep(std::time::Duration::from_secs(4));
    status(cfg)
}

/// The actual SYSTEM-side start. Invoked directly when already SYSTEM, or by the
/// scheduled task (`start-system`).
pub fn start_system(cfg: &Config) -> Result<()> {
    let sessions = enumerate_gamer_sessions(cfg)?;
    if sessions.is_empty() {
        tracing::warn!("no active gamer sessions found (pattern {})", cfg.user_pattern);
        return Ok(());
    }
    for s in &sessions {
        match launch_in_session(s, cfg) {
            Ok(pid) => tracing::info!(
                "started broadcast: {} (session {}) -> port {} [pid {}]  http://{}:{}/",
                s.user, s.id, s.port(cfg.base_port), pid, cfg.public_ip, s.port(cfg.base_port)
            ),
            Err(e) => tracing::error!("failed to start session {} ({}): {e:#}", s.id, s.user),
        }
    }
    Ok(())
}

/// Register a scheduled task that runs `glitch9-manager start-system` as SYSTEM, so
/// a non-SYSTEM admin can start broadcasts via `start` (which triggers it). The task
/// carries the full CLI config so the SYSTEM run uses the same settings.
pub fn deploy(cfg: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let exe = exe.to_string_lossy().to_string();
    // Reconstruct the config as CLI args so the SYSTEM task behaves identically.
    let tr = format!(
        "\"{exe}\" start-system --base-port {bp} --public-ip {ip} --engine \"{eng}\" \
         --width {w} --height {h} --fps {fps} --bitrate {br} --user-pattern \"{pat}\" \
         --log-dir \"{ld}\"",
        bp = cfg.base_port, ip = cfg.public_ip, eng = cfg.engine, w = cfg.width,
        h = cfg.height, fps = cfg.fps, br = cfg.bitrate, pat = cfg.user_pattern, ld = cfg.log_dir,
    );
    let status = std::process::Command::new("schtasks")
        .args([
            "/create", "/tn", TASK_NAME, "/tr", &tr, "/sc", "once", "/st", "00:00",
            "/ru", "SYSTEM", "/rl", "HIGHEST", "/f",
        ])
        .status()
        .context("schtasks /create")?;
    if status.success() {
        tracing::info!("deployed SYSTEM task '{TASK_NAME}'. Now `glitch9-manager start` works as a normal admin.");
        Ok(())
    } else {
        anyhow::bail!("schtasks /create failed (run deploy from an elevated admin shell)")
    }
}

pub fn undeploy() -> Result<()> {
    let _ = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", TASK_NAME, "/f"])
        .status();
    tracing::info!("removed task '{TASK_NAME}'");
    Ok(())
}

/// Are we running as the SYSTEM account? (SYSTEM's username is "SYSTEM" under the
/// NT AUTHORITY domain.) Cheap check via whoami.
fn is_system() -> bool {
    std::process::Command::new("whoami")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .eq_ignore_ascii_case("nt authority\\system")
        })
        .unwrap_or(false)
}

pub fn stop(_cfg: &Config) -> Result<()> {
    // Kill all engine instances. Simple and sufficient: the engine binary name is
    // unique to this service.
    let status = std::process::Command::new("taskkill")
        .args(["/im", "glitch9-stream.exe", "/f"])
        .status()
        .context("spawn taskkill")?;
    if status.success() {
        tracing::info!("stopped all broadcast engines");
    } else {
        tracing::info!("no running broadcast engines to stop");
    }
    Ok(())
}

pub fn status(cfg: &Config) -> Result<()> {
    let sessions = enumerate_gamer_sessions(cfg)?;
    println!("Active gamer sessions and broadcast ports:");
    for s in &sessions {
        let port = s.port(cfg.base_port);
        let live = port_listening(port);
        println!(
            "  {:<8} session {:<3} port {}  {}",
            s.user,
            s.id,
            port,
            if live {
                format!("LIVE  http://{}:{}/", cfg.public_ip, port)
            } else {
                "stopped".to_string()
            }
        );
    }
    Ok(())
}

/// Is anything LISTENING on this TCP port? Uses netstat (no extra deps).
fn port_listening(port: u16) -> bool {
    let out = std::process::Command::new("netstat").arg("-ano").output();
    if let Ok(out) = out {
        let text = String::from_utf8_lossy(&out.stdout);
        let needle = format!(":{} ", port);
        text.lines()
            .any(|l| l.contains(&needle) && l.contains("LISTENING"))
    } else {
        false
    }
}
