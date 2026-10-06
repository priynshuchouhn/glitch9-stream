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
    WTSEnumerateProcessesW, WTSEnumerateSessionsW, WTSFreeMemory, WTSQuerySessionInformationW,
    WTSQueryUserToken, WTSUserName, WTSActive, WTS_PROCESS_INFOW, WTS_SESSION_INFOW,
    WTS_CURRENT_SERVER_HANDLE,
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

/// Resolve the public IP to advertise. If config has one, use it. Otherwise
/// auto-detect so a single build runs on any VM: try an external echo service,
/// then fall back to the primary outbound local address.
pub fn resolve_public_ip(cfg: &Config) -> String {
    if let Some(ip) = cfg.public_ip.as_ref().filter(|s| !s.trim().is_empty()) {
        return ip.trim().to_string();
    }
    // 1) External echo (works when the VM has outbound internet; gives the routable
    //    public IP even behind 1:1 NAT). Short timeout; best-effort.
    for url in ["https://api.ipify.org", "https://ifconfig.me/ip", "https://icanhazip.com"] {
        if let Ok(out) = std::process::Command::new("curl")
            .args(["-s", "--max-time", "4", url])
            .output()
        {
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if is_ipv4(&ip) {
                tracing::info!("auto-detected public IP {} (via {})", ip, url);
                return ip;
            }
        }
    }
    // 2) Fallback: primary outbound IPv4 from the routing table.
    if let Some(ip) = primary_local_ipv4() {
        tracing::warn!("using primary local IPv4 {} (no external IP detected)", ip);
        return ip;
    }
    tracing::warn!("could not detect public IP; defaulting to 0.0.0.0 (LAN viewers only)");
    "0.0.0.0".to_string()
}

fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

/// Primary outbound IPv4 via PowerShell routing lookup (no external calls).
fn primary_local_ipv4() -> Option<String> {
    let ps = "(Get-NetIPConfiguration | Where-Object { $_.IPv4DefaultGateway -ne $null } | \
              Select-Object -First 1).IPv4Address.IPAddress";
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .output()
        .ok()?;
    let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if is_ipv4(&ip) { Some(ip) } else { None }
}

/// Orchestration signal: a session is "broadcastable" if session-api dropped a
/// `broadcast.json` in that gamer's config dir (C:\glitch9-prod\configs\gamerN\).
/// Mirrors the idle-watchdog / .rhino_apikey per-session config pattern. The file's
/// mere presence means "an active session wants to be broadcast"; teardown removes it.
pub fn session_has_broadcast_config(cfg: &Config, user: &str) -> bool {
    broadcast_config_path(cfg, user).is_file()
}

fn broadcast_config_path(cfg: &Config, user: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "{}\\{}\\broadcast.json",
        cfg.config_root.trim_end_matches('\\'),
        user
    ))
}

/// Per-session broadcast parameters that session-api writes into broadcast.json.
/// When `sfu_whip_base` is present, the worker publishes to the SFU via WHIP for
/// room `session-<sessionId>`; otherwise it falls back to direct serving.
#[derive(Default)]
pub struct BroadcastConfig {
    pub session_id: String,
    pub sfu_whip_base: Option<String>, // e.g. http://46.232.234.68:8889
    pub whip_token: Option<String>,
}

impl BroadcastConfig {
    /// The SFU room name for this session (what viewers' WHEP URL also uses).
    pub fn room(&self) -> String {
        format!("session-{}", self.session_id)
    }
    /// Full WHIP publish URL, if the SFU base is set.
    pub fn whip_url(&self) -> Option<String> {
        self.sfu_whip_base
            .as_ref()
            .map(|b| format!("{}/{}/whip", b.trim_end_matches('/'), self.room()))
    }
}

/// Read + minimally-parse a session's broadcast.json (dependency-free scrape of the
/// string fields we need). Returns a default (empty) config if absent/unparseable.
pub fn read_broadcast_config(cfg: &Config, user: &str) -> BroadcastConfig {
    let mut bc = BroadcastConfig::default();
    let text = match std::fs::read_to_string(broadcast_config_path(cfg, user)) {
        Ok(t) => t,
        Err(_) => return bc,
    };
    bc.session_id = json_str(&text, "sessionId").unwrap_or_default();
    bc.sfu_whip_base = json_str(&text, "sfuWhipBase").filter(|s| !s.is_empty());
    bc.whip_token = json_str(&text, "whipToken").filter(|s| !s.is_empty());
    bc
}

/// Extract a JSON string value for `key` (tiny scrape; handles "key": "value").
fn json_str(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let i = s.find(&pat)? + pat.len();
    let rest = &s[i..];
    let colon = rest.find(':')? + 1;
    let after = rest[colon..].trim_start();
    let after = after.strip_prefix('"')?;
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// Should this session be broadcast, per the configured source?
pub fn session_is_active(cfg: &Config, session: &GamerSession) -> bool {
    match cfg.source {
        crate::cli::Source::Orchestration => session_has_broadcast_config(cfg, &session.user),
        crate::cli::Source::Process => session_has_game(session.id),
    }
}

/// Processes that are part of the OS/shell/streaming infra, NOT a game. If a session
/// has any process beyond these, we treat it as "a game is running". Lowercased.
const INFRA_PROCESSES: &[&str] = &[
    // Windows shell / session infrastructure
    "explorer.exe", "svchost.exe", "sihost.exe", "taskhostw.exe", "rdpclip.exe",
    "conhost.exe", "ctfmon.exe", "dllhost.exe", "shellhost.exe", "runtimebroker.exe",
    "wwahost.exe", "dwm.exe", "csrss.exe", "winlogon.exe", "userinit.exe",
    "fontdrvhost.exe", "searchhost.exe", "startmenuexperiencehost.exe",
    "textinputhost.exe", "smartscreen.exe", "wmiprvse.exe", "audiodg.exe",
    "applicationframehost.exe", "systemsettings.exe", "lsass.exe", "services.exe",
    // Vendor/host agents + our own stack (never count these as a game)
    "rhinostream.exe", "rhinostreamv2.exe", "glitch9-stream.exe", "glitch9-manager.exe",
    "azurearcsystray.exe", "xboxstat.exe", "gigabytedownloadassistant.exe",
    "mstsc.exe", "psexec64.exe", "psexesvc.exe", "cmd.exe", "powershell.exe",
    "nvcontainer.exe", "nvidia web helper.exe", "nvdisplay.container.exe",
];

/// Does this session have an active game? True if it has any process that isn't
/// known infrastructure. Enumerates processes server-wide (one WTS call) and filters
/// to the session.
pub fn session_has_game(session_id: u32) -> bool {
    unsafe {
        let mut info_ptr: *mut WTS_PROCESS_INFOW = std::ptr::null_mut();
        let mut count: u32 = 0;
        if WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info_ptr, &mut count)
            .is_err()
        {
            return false;
        }
        let procs = std::slice::from_raw_parts(info_ptr, count as usize);
        let mut found = false;
        for p in procs {
            if p.SessionId != session_id || p.pProcessName.is_null() {
                continue;
            }
            if let Ok(name) = p.pProcessName.to_string() {
                let name_l = name.to_ascii_lowercase();
                if !INFRA_PROCESSES.contains(&name_l.as_str()) {
                    found = true;
                    break;
                }
            }
        }
        WTSFreeMemory(info_ptr as *mut c_void);
        found
    }
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
    // If the session's broadcast.json names an SFU, publish via WHIP (production:
    // encode once, SFU fans out). Otherwise serve browsers directly (dev/LAN).
    let bc = read_broadcast_config(cfg, &session.user);
    let cmdline = match bc.whip_url() {
        Some(whip_url) => format!(
            "\"{engine}\" --publish-whip {whip} --display 0 --width {w} --height {h} \
             --fps {fps} --bitrate {br} --audio true",
            engine = cfg.engine, whip = whip_url, w = cfg.width, h = cfg.height,
            fps = cfg.fps, br = cfg.bitrate,
        ),
        None => format!(
            "\"{engine}\" --bind 0.0.0.0 --port {port} --display 0 --width {w} --height {h} \
             --fps {fps} --bitrate {br} --audio true",
            engine = cfg.engine, port = port, w = cfg.width, h = cfg.height,
            fps = cfg.fps, br = cfg.bitrate,
        ),
    };

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
        let public_ip = resolve_public_ip(cfg);
        let mut env_vec = build_env_with(env, "G9_PUBLIC_IP", &public_ip);
        if have_env && !env.is_null() {
            let _ = DestroyEnvironmentBlock(env);
        }
        // Pass the WHIP publish token via env (kept off the command line / logs).
        if let Some(tok) = bc.whip_token.as_ref() {
            env_vec = add_env_var(env_vec, "G9_WHIP_TOKEN", tok);
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

/// Append/override a var in an already-built UTF-16 double-null env block.
fn add_env_var(block: Vec<u16>, name: &str, value: &str) -> Vec<u16> {
    // Split the existing block into entries (strip the trailing double-null).
    let mut entries: Vec<Vec<u16>> = Vec::new();
    let mut cur: Vec<u16> = Vec::new();
    for &w in &block {
        if w == 0 {
            if !cur.is_empty() {
                entries.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(w);
        }
    }
    let upper = format!("{}=", name).to_ascii_uppercase();
    entries.retain(|e| !String::from_utf16_lossy(e).to_ascii_uppercase().starts_with(&upper));
    entries.push(format!("{name}={value}").encode_utf16().collect());
    let mut out: Vec<u16> = Vec::new();
    for e in entries {
        out.extend_from_slice(&e);
        out.push(0);
    }
    out.push(0);
    out
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
/// scheduled task (`start-system`). Loads the persisted config so a task launched
/// with no args still uses the settings chosen at `deploy` time.
pub fn start_system(base: &Config) -> Result<()> {
    let cfg = &load_config_file(base);
    let sessions = enumerate_gamer_sessions(cfg)?;
    if sessions.is_empty() {
        tracing::warn!("no active gamer sessions found (pattern {})", cfg.user_pattern);
        return Ok(());
    }
    let public_ip = resolve_public_ip(cfg);
    for s in &sessions {
        // Only spawn a worker when the session is active per the configured source
        // (orchestration broadcast.json, or the process-scan fallback). Don't burn a
        // GPU encoder on an idle/unprovisioned desktop.
        if !session_is_active(cfg, s) {
            tracing::info!("skip {} (session {}): not an active broadcast session", s.user, s.id);
            continue;
        }
        match launch_in_session(s, cfg) {
            Ok(pid) => tracing::info!(
                "started broadcast: {} (session {}) -> port {} [pid {}]  http://{}:{}/",
                s.user, s.id, s.port(cfg.base_port), pid, public_ip, s.port(cfg.base_port)
            ),
            Err(e) => tracing::error!("failed to start session {} ({}): {e:#}", s.id, s.user),
        }
    }
    Ok(())
}

/// Watch loop: continuously reconcile broadcast workers against active-game sessions.
/// Spawns a worker when a session's game starts; stops it when the game exits. Runs
/// until killed. Must be SYSTEM (same token requirement as start). Poll interval in
/// seconds.
/// Per-session worker tracking for the watchdog.
#[derive(Default)]
struct WorkerState {
    /// `bytes_sent` from the last /healthz poll (to detect a stalled encoder).
    last_bytes: u64,
    /// Consecutive polls where the worker was unhealthy (not listening, no /healthz,
    /// or bytes_sent flat). Restart when this crosses the threshold.
    unhealthy_polls: u32,
    /// Monotonic poll count since (re)spawn — gives a startup grace period before
    /// we judge "bytes not climbing" as unhealthy (first frames take a moment).
    polls_since_spawn: u32,
}

pub fn watch(base: &Config, interval_secs: u64) -> Result<()> {
    let cfg = &load_config_file(base);
    // After this many consecutive unhealthy polls, restart the worker. With a 5s
    // interval that's ~15s of sustained trouble before a restart — long enough to
    // not thrash on a transient blip, short enough to recover a crashed worker fast.
    const UNHEALTHY_RESTART_THRESHOLD: u32 = 3;
    // Grace polls after spawn before judging bytes-not-climbing (startup + an idle
    // desktop that legitimately produces no frames until there's motion).
    const SPAWN_GRACE_POLLS: u32 = 4;

    tracing::info!(
        "watch: reconciling + health-checking broadcasts every {}s (source={:?}, pattern {})",
        interval_secs, cfg.source, cfg.user_pattern
    );
    let mut workers: std::collections::HashMap<u32, WorkerState> = std::collections::HashMap::new();

    loop {
        let sessions = enumerate_gamer_sessions(cfg).unwrap_or_default();
        let active: std::collections::HashSet<u32> = sessions
            .iter()
            .filter(|s| session_is_active(cfg, s))
            .map(|s| s.id)
            .collect();

        // Enforce a per-VM concurrent-broadcast cap (GPU-budget guard). Spawning is
        // only allowed while we're under the cap; existing workers are always kept
        // health-checked.
        let cap = cfg.max_broadcasts;

        for s in &sessions {
            if !active.contains(&s.id) {
                continue;
            }
            let port = s.port(cfg.base_port);
            let listening = port_listening(port);
            let health = if listening { poll_health(port) } else { None };

            match workers.get_mut(&s.id) {
                // Known worker — health-check it.
                Some(st) => {
                    st.polls_since_spawn += 1;
                    let healthy = match &health {
                        Some(h) => {
                            // Healthy if listening and bytes advanced, OR still within
                            // the startup grace window (idle desktop = legit 0 bytes).
                            let advanced = h.bytes_sent > st.last_bytes;
                            st.last_bytes = h.bytes_sent;
                            advanced || st.polls_since_spawn <= SPAWN_GRACE_POLLS
                        }
                        None => false, // not listening / no /healthz = unhealthy
                    };
                    if healthy {
                        st.unhealthy_polls = 0;
                    } else {
                        st.unhealthy_polls += 1;
                        tracing::warn!(
                            "worker session {} (port {}) unhealthy ({}/{}): listening={}",
                            s.id, port, st.unhealthy_polls, UNHEALTHY_RESTART_THRESHOLD, listening
                        );
                        if st.unhealthy_polls >= UNHEALTHY_RESTART_THRESHOLD {
                            tracing::error!(
                                "worker session {} (port {}) unhealthy -> restarting", s.id, port
                            );
                            stop_port(port);
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            match launch_in_session(s, cfg) {
                                Ok(pid) => tracing::info!(
                                    "restarted broadcast {} (session {}) port {} [pid {}]",
                                    s.user, s.id, port, pid
                                ),
                                Err(e) => tracing::error!("restart session {} failed: {e:#}", s.id),
                            }
                            *st = WorkerState::default();
                        }
                    }
                }
                // No worker yet — spawn if under the GPU-budget cap.
                None => {
                    if workers.len() as u32 >= cap {
                        tracing::warn!(
                            "session {} active but broadcast cap {} reached — not spawning (GPU budget)",
                            s.id, cap
                        );
                        continue;
                    }
                    match launch_in_session(s, cfg) {
                        Ok(pid) => {
                            tracing::info!(
                                "session active -> broadcast {} (session {}) port {} [pid {}]",
                                s.user, s.id, port, pid
                            );
                            workers.insert(s.id, WorkerState::default());
                        }
                        Err(e) => tracing::error!("spawn session {} failed: {e:#}", s.id),
                    }
                }
            }
        }

        // Stop workers whose session ended (or vanished).
        let to_stop: Vec<u32> = workers.keys().copied().filter(|id| !active.contains(id)).collect();
        for id in to_stop {
            let port = cfg.base_port.saturating_add(id as u16);
            tracing::info!("session ended -> stopping broadcast for session {} (port {})", id, port);
            stop_port(port);
            workers.remove(&id);
        }

        std::thread::sleep(std::time::Duration::from_secs(interval_secs.max(1)));
    }
}

/// Health snapshot parsed from a worker's `GET /healthz`.
struct Health {
    bytes_sent: u64,
}

/// Poll `http://127.0.0.1:<port>/healthz`; returns None if unreachable/unparseable.
fn poll_health(port: u16) -> Option<Health> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let out = std::process::Command::new("curl")
        .args(["-s", "--max-time", "3", &url])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // Minimal JSON scrape (no serde dep): find "bytes_sent": N.
    let bytes_sent = extract_u64(&text, "\"bytes_sent\":")?;
    Some(Health { bytes_sent })
}

/// Extract the integer following `key` in `s` (tiny, dependency-free JSON scrape).
fn extract_u64(s: &str, key: &str) -> Option<u64> {
    let i = s.find(key)? + key.len();
    let rest = &s[i..];
    let digits: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Stop the single engine listening on `port` (used when a session's game ends).
/// Finds the PID via netstat and kills it, so other sessions' workers keep running.
fn stop_port(port: u16) {
    let out = std::process::Command::new("netstat").arg("-ano").output();
    if let Ok(out) = out {
        let text = String::from_utf8_lossy(&out.stdout);
        let needle = format!(":{} ", port);
        for line in text.lines() {
            if line.contains(&needle) && line.contains("LISTENING") {
                if let Some(pid) = line.split_whitespace().last() {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/pid", pid, "/f"])
                        .status();
                }
            }
        }
    }
}

/// Register a scheduled task that runs `glitch9-manager start-system` as SYSTEM, so
/// a non-SYSTEM admin can start broadcasts via `start` (which triggers it). The task
/// carries the full CLI config so the SYSTEM run uses the same settings.
pub fn deploy(cfg: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let exe = exe.to_string_lossy().to_string();

    // schtasks caps /tr at 261 chars, so we can't inline all flags. Persist the
    // config next to the exe and have `start-system` load it; the task command is
    // then just `"<exe>" start-system`.
    write_config_file(&exe, cfg)?;

    // The task runs `watch` as SYSTEM: it continuously spawns a broadcast worker
    // when a session's game starts and stops it when the game exits. `start`
    // (as any admin) just kicks this task; stopping the task stops the watcher.
    let tr = format!("\"{exe}\" watch");
    let status = std::process::Command::new("schtasks")
        .args([
            "/create", "/tn", TASK_NAME, "/tr", &tr, "/sc", "once", "/st", "00:00",
            "/ru", "SYSTEM", "/rl", "HIGHEST", "/f",
        ])
        .status()
        .context("schtasks /create")?;
    if status.success() {
        tracing::info!(
            "deployed SYSTEM task '{TASK_NAME}' (watch mode). `glitch9-manager start` \
             launches the watcher; it then auto-manages one broadcast per active-game session."
        );
        Ok(())
    } else {
        anyhow::bail!("schtasks /create failed (run deploy from an elevated admin shell)")
    }
}

/// Path of the persisted config (next to the manager exe).
fn config_path(exe: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(exe);
    p.set_file_name("manager-config.txt");
    p
}

/// Persist config as simple `key=value` lines so the SYSTEM task reads identical
/// settings (deploy writes it; start_system loads it).
fn write_config_file(exe: &str, cfg: &Config) -> Result<()> {
    let source = match cfg.source {
        crate::cli::Source::Process => "process",
        crate::cli::Source::Orchestration => "orchestration",
    };
    let body = format!(
        "base_port={}\npublic_ip={}\nengine={}\nwidth={}\nheight={}\nfps={}\nbitrate={}\nuser_pattern={}\nlog_dir={}\nsource={}\nconfig_root={}\nmax_broadcasts={}\n",
        cfg.base_port, cfg.public_ip.clone().unwrap_or_default(), cfg.engine, cfg.width,
        cfg.height, cfg.fps, cfg.bitrate, cfg.user_pattern, cfg.log_dir, source, cfg.config_root,
        cfg.max_broadcasts,
    );
    std::fs::write(config_path(exe), body).context("write manager-config.txt")?;
    Ok(())
}

/// Load persisted config if present, overlaying onto the given base config.
fn load_config_file(base: &Config) -> Config {
    let mut cfg = base.clone();
    let exe = match std::env::current_exe() {
        Ok(e) => e.to_string_lossy().to_string(),
        Err(_) => return cfg,
    };
    let text = match std::fs::read_to_string(config_path(&exe)) {
        Ok(t) => t,
        Err(_) => return cfg,
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "base_port" => if let Ok(x) = v.parse() { cfg.base_port = x },
            "public_ip" => cfg.public_ip = if v.is_empty() { None } else { Some(v) },
            "engine" => cfg.engine = v,
            "source" => cfg.source = if v == "process" {
                crate::cli::Source::Process
            } else {
                crate::cli::Source::Orchestration
            },
            "config_root" => cfg.config_root = v,
            "max_broadcasts" => if let Ok(x) = v.parse() { cfg.max_broadcasts = x },
            "width" => if let Ok(x) = v.parse() { cfg.width = x },
            "height" => if let Ok(x) = v.parse() { cfg.height = x },
            "fps" => if let Ok(x) = v.parse() { cfg.fps = x },
            "bitrate" => if let Ok(x) = v.parse() { cfg.bitrate = x },
            "user_pattern" => cfg.user_pattern = v,
            "log_dir" => cfg.log_dir = v,
            _ => {}
        }
    }
    cfg
}

pub fn undeploy() -> Result<()> {
    let _ = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", TASK_NAME, "/f"])
        .status();
    tracing::info!("removed task '{TASK_NAME}'");
    Ok(())
}

const SERVICE_NAME: &str = "Glitch9Broadcast";

/// Install the watcher as an auto-start LocalSystem Windows service (production).
/// Persists config next to the exe (so `run-service` loads identical settings),
/// then registers via `sc create`. Must run elevated.
pub fn install_service(cfg: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let exe = exe.to_string_lossy().to_string();
    write_config_file(&exe, cfg)?;

    // binPath points the SCM at our run-service entry. Quote the exe path.
    let bin = format!("\"{exe}\" run-service");
    let create = std::process::Command::new("sc")
        .args([
            "create", SERVICE_NAME, "binPath=", &bin, "start=", "auto",
            "obj=", "LocalSystem", "DisplayName=", "Glitch9 Broadcast Manager",
        ])
        .status()
        .context("sc create")?;
    if !create.success() {
        // Already exists? Update the binPath instead.
        let _ = std::process::Command::new("sc")
            .args(["config", SERVICE_NAME, "binPath=", &bin, "start=", "auto"])
            .status();
    }
    // Restart on failure (SCM auto-recovery): reset count daily, 5s/10s/30s backoff.
    let _ = std::process::Command::new("sc")
        .args(["failure", SERVICE_NAME, "reset=", "86400", "actions=", "restart/5000/restart/10000/restart/30000"])
        .status();
    let _ = std::process::Command::new("sc").args(["description", SERVICE_NAME,
        "Spawns/stops glitch9-stream broadcast workers per active game session."]).status();
    // Start it now.
    let start = std::process::Command::new("sc").args(["start", SERVICE_NAME]).status();
    tracing::info!(
        "installed service '{SERVICE_NAME}' (auto-start, LocalSystem, auto-restart). start: {:?}",
        start.map(|s| s.success()).unwrap_or(false)
    );
    Ok(())
}

pub fn uninstall_service() -> Result<()> {
    let _ = std::process::Command::new("sc").args(["stop", SERVICE_NAME]).status();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let _ = std::process::Command::new("taskkill").args(["/im", "glitch9-stream.exe", "/f"]).status();
    let del = std::process::Command::new("sc").args(["delete", SERVICE_NAME]).status();
    tracing::info!("uninstalled service '{SERVICE_NAME}': {:?}", del.map(|s| s.success()).unwrap_or(false));
    Ok(())
}

// ── Minimal Windows service dispatcher ─────────────────────────────────────────
// The SCM launches us with `run-service`; we register a control handler, report
// RUNNING, run the watch loop on a thread, and on STOP flip a flag the loop checks.

use std::sync::atomic::AtomicBool;
static SERVICE_STOP: AtomicBool = AtomicBool::new(false);
static mut STATUS_HANDLE: isize = 0;

pub fn run_service() -> Result<()> {
    use windows::core::PWSTR;
    use windows::Win32::System::Services::{
        StartServiceCtrlDispatcherW, SERVICE_TABLE_ENTRYW,
    };
    unsafe {
        let mut name: Vec<u16> = SERVICE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let table = [
            SERVICE_TABLE_ENTRYW {
                lpServiceName: PWSTR(name.as_mut_ptr()),
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRYW {
                lpServiceName: PWSTR::null(),
                lpServiceProc: None,
            },
        ];
        // Blocks until the service stops. If not launched by the SCM (e.g. run from
        // a console), this fails — fall back to running watch directly.
        if StartServiceCtrlDispatcherW(table.as_ptr()).is_err() {
            tracing::warn!("not started by SCM; running watch loop directly");
            return watch(&load_config_file(&default_config()), 5);
        }
    }
    Ok(())
}

unsafe extern "system" fn service_ctrl_handler(control: u32) {
    const SERVICE_CONTROL_STOP: u32 = 0x1;
    const SERVICE_CONTROL_SHUTDOWN: u32 = 0x5;
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        SERVICE_STOP.store(true, std::sync::atomic::Ordering::SeqCst);
        set_service_state(3 /*STOP_PENDING*/, 0);
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows::core::PWSTR) {
    use windows::core::PCWSTR;
    use windows::Win32::System::Services::RegisterServiceCtrlHandlerW;
    let name: Vec<u16> = SERVICE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    match RegisterServiceCtrlHandlerW(PCWSTR(name.as_ptr()), Some(service_ctrl_handler)) {
        Ok(h) => STATUS_HANDLE = h.0 as isize,
        Err(_) => return,
    }
    set_service_state(4 /*RUNNING*/, 0x1 | 0x4 /*ACCEPT_STOP|SHUTDOWN*/);

    // Run the watch loop on a worker thread; poll the stop flag here.
    let handle = std::thread::spawn(|| {
        let base = default_config();
        let _ = watch(&load_config_file(&base), 5);
    });
    while !SERVICE_STOP.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // Stop requested: tear down workers and report stopped.
    let _ = std::process::Command::new("taskkill")
        .args(["/im", "glitch9-stream.exe", "/f"])
        .status();
    set_service_state(1 /*STOPPED*/, 0);
    let _ = handle; // detached; process is stopping
}

/// Report service state to the SCM.
unsafe fn set_service_state(state: u32, controls: u32) {
    use windows::Win32::System::Services::{
        SetServiceStatus, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STATUS_CURRENT_STATE,
        ENUM_SERVICE_TYPE,
    };
    if STATUS_HANDLE == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: ENUM_SERVICE_TYPE(0x10), // SERVICE_WIN32_OWN_PROCESS
        dwCurrentState: SERVICE_STATUS_CURRENT_STATE(state),
        dwControlsAccepted: controls,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(STATUS_HANDLE as *mut _), &status);
}

/// A Config with just the defaults (used when the service loads persisted config).
fn default_config() -> Config {
    Config {
        user_pattern: r"^gamer\d+$".to_string(),
        base_port: 8080,
        public_ip: None,
        engine: r"C:\glitch9-stream\target\release\glitch9-stream.exe".to_string(),
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate: 3_000_000,
        log_dir: r"C:\glitch9-stream".to_string(),
        source: crate::cli::Source::Orchestration,
        config_root: r"C:\glitch9-prod\configs".to_string(),
        max_broadcasts: 4,
    }
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
    // End the watcher task first so it doesn't immediately respawn workers.
    let _ = std::process::Command::new("schtasks")
        .args(["/end", "/tn", TASK_NAME])
        .status();
    // Then kill all engine instances (the binary name is unique to this service).
    let status = std::process::Command::new("taskkill")
        .args(["/im", "glitch9-stream.exe", "/f"])
        .status()
        .context("spawn taskkill")?;
    if status.success() {
        tracing::info!("stopped watcher + all broadcast engines");
    } else {
        tracing::info!("stopped watcher; no running broadcast engines");
    }
    Ok(())
}

pub fn status(base: &Config) -> Result<()> {
    let cfg = &load_config_file(base);
    let ip = resolve_public_ip(cfg);
    let sessions = enumerate_gamer_sessions(cfg)?;
    println!("Active gamer sessions and broadcast ports (source={:?}):", cfg.source);
    for s in &sessions {
        let port = s.port(cfg.base_port);
        let live = port_listening(port);
        let want = session_is_active(cfg, s);
        println!(
            "  {:<8} session {:<3} port {}  {}{}",
            s.user,
            s.id,
            port,
            if live {
                format!("LIVE  http://{}:{}/", ip, port)
            } else {
                "stopped".to_string()
            },
            if want && !live { "  (active session, worker starting)" } else { "" }
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
