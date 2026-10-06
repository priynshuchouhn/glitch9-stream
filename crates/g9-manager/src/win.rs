//! Windows implementation: enumerate RDP sessions via the WTS API and launch one
//! engine per gamer session, directly in that session, using the session's user
//! token (`WTSQueryUserToken` + `CreateProcessAsUserW`). Must run elevated (SYSTEM
//! or admin) to obtain other sessions' tokens.

use crate::cli::{Config, GamerSession};
use anyhow::{Context, Result};
use std::ffi::c_void;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    WTSEnumerateSessionsW, WTSFreeMemory, WTSQuerySessionInformationW, WTSQueryUserToken,
    WTSUserName, WTS_CONNECTSTATE_CLASS, WTSActive, WTS_SESSION_INFOW, WTS_CURRENT_SERVER_HANDLE,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTUPINFOW,
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
    let log = format!("{}\\session-{}.log", cfg.log_dir.trim_end_matches('\\'), session.id);

    // Build the command line. cmd.exe wrapper lets us redirect the engine's output
    // to a per-session log file and set G9_PUBLIC_IP for ICE.
    let cmdline = format!(
        "cmd.exe /c set G9_PUBLIC_IP={ip}&& \"{engine}\" --bind 0.0.0.0 --port {port} \
         --display 0 --width {w} --height {h} --fps {fps} --bitrate {br} --audio true > \"{log}\" 2>&1",
        ip = cfg.public_ip,
        engine = cfg.engine,
        port = port,
        w = cfg.width,
        h = cfg.height,
        fps = cfg.fps,
        br = cfg.bitrate,
        log = log,
    );

    unsafe {
        // Get the session's user token so the process runs in that session/desktop.
        let mut token: HANDLE = HANDLE::default();
        WTSQueryUserToken(session.id, &mut token)
            .with_context(|| format!("WTSQueryUserToken(session {})", session.id))?;

        // Build the user's environment block (so the engine sees the right env).
        let mut env: *mut c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, token, false).is_ok();

        let mut cmd_utf16: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        let mut si = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        // Target the interactive desktop of the session.
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        si.lpDesktop = PWSTR(desktop.as_mut_ptr());
        let mut pi = PROCESS_INFORMATION::default();

        let flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;
        let result = CreateProcessAsUserW(
            token,
            PCWSTR::null(),
            PWSTR(cmd_utf16.as_mut_ptr()),
            None,
            None,
            false,
            flags,
            if have_env { Some(env) } else { None },
            PCWSTR::null(),
            &si,
            &mut pi,
        );

        if have_env && !env.is_null() {
            let _ = DestroyEnvironmentBlock(env);
        }
        let _ = CloseHandle(token);

        result.with_context(|| format!("CreateProcessAsUserW(session {})", session.id))?;
        let pid = pi.dwProcessId;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        Ok(pid)
    }
}

pub fn start(cfg: &Config) -> Result<()> {
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
