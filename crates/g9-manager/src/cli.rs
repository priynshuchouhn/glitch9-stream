//! CLI definition and shared config for the broadcast manager.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "glitch9-manager", version, about = "Per-session broadcast manager for glitch9-stream")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Regex matched against session usernames to decide which to broadcast.
    #[arg(long, default_value = r"^gamer\d+$", global = true)]
    pub user_pattern: String,

    /// Base TCP port; a session's port = base_port + session_id.
    #[arg(long, default_value_t = 8080, global = true)]
    pub base_port: u16,

    /// Public IP advertised in ICE candidates (passed to each engine as G9_PUBLIC_IP).
    #[arg(long, default_value = "103.171.97.176", global = true)]
    pub public_ip: String,

    /// Path to the engine binary.
    #[arg(long, default_value = r"C:\glitch9-stream\target\release\glitch9-stream.exe", global = true)]
    pub engine: String,

    #[arg(long, default_value_t = 1920, global = true)]
    pub width: u32,
    #[arg(long, default_value_t = 1080, global = true)]
    pub height: u32,
    #[arg(long, default_value_t = 30, global = true)]
    pub fps: u32,
    #[arg(long, default_value_t = 3_000_000, global = true)]
    pub bitrate: u32,

    /// Directory for per-session engine logs.
    #[arg(long, default_value = r"C:\glitch9-stream", global = true)]
    pub log_dir: String,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// List active gamer sessions and whether their broadcast port is live.
    Status,
    /// Launch one engine per active gamer session.
    ///
    /// Needs SYSTEM (WTSQueryUserToken requires SE_TCB). If run without it, this
    /// auto-elevates by triggering the SYSTEM scheduled task created by `deploy`.
    Start,
    /// Stop all engine instances. Does not require SYSTEM.
    Stop,
    /// One-time: register a SYSTEM scheduled task so a non-SYSTEM admin (e.g.
    /// g9admin) can start broadcasts. Run once after copying the binary.
    Deploy,
    /// Remove the scheduled task created by `deploy`.
    Undeploy,
    /// INTERNAL: the actual SYSTEM-side start, invoked by the scheduled task.
    /// (Equivalent to `start` but never tries to re-elevate.)
    StartSystem,
    /// Run continuously (SYSTEM): spawn a broadcast worker when a session's game
    /// starts and stop it when the game exits. The intended production mode.
    Watch {
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 5)]
        interval: u64,
    },
}

/// Resolved config shared by the platform implementations.
#[derive(Debug, Clone)]
pub struct Config {
    pub user_pattern: String,
    pub base_port: u16,
    pub public_ip: String,
    pub engine: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub log_dir: String,
}

impl Cli {
    pub fn to_config(&self) -> Config {
        Config {
            user_pattern: self.user_pattern.clone(),
            base_port: self.base_port,
            public_ip: self.public_ip.clone(),
            engine: self.engine.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate: self.bitrate,
            log_dir: self.log_dir.clone(),
        }
    }
}

/// A gamer session we may broadcast.
#[derive(Debug, Clone)]
pub struct GamerSession {
    pub id: u32,
    pub user: String,
}

impl GamerSession {
    pub fn port(&self, base_port: u16) -> u16 {
        base_port.saturating_add(self.id as u16)
    }
}
