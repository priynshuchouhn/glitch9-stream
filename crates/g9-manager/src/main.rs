//! `glitch9-manager` — multi-session broadcast manager for glitch9-stream.
//!
//! glitch9-stream is the spectator/broadcast service that runs ALONGSIDE RhinoStream
//! (which serves the player). DXGI Desktop Duplication only captures the desktop of
//! the session the process runs in, so broadcasting N gamer sessions means N engine
//! instances — one launched INSIDE each session, each on its own TCP port.
//!
//! This manager enumerates active gamer RDP sessions via the Windows WTS API and
//! launches one engine per session directly in that session using the session's
//! user token (`WTSQueryUserToken` + `CreateProcessAsUser`) — no PsExec needed. It
//! must run elevated (SYSTEM or admin) to obtain other sessions' tokens.
//!
//! Port mapping is deterministic: `base_port + session_id` (session 2 -> 8082, ...),
//! so each session's viewer URL is stable.

mod cli;
#[cfg(windows)]
mod win;
#[cfg(not(windows))]
mod stub;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cfg = cli.to_config();

    #[cfg(windows)]
    {
        match cli.command {
            Command::Status => win::status(&cfg),
            Command::Start => win::start(&cfg),
            Command::StartSystem => win::start_system(&cfg),
            Command::Stop => win::stop(&cfg),
            Command::Deploy => win::deploy(&cfg),
            Command::Undeploy => win::undeploy(),
            Command::Watch { interval } => win::watch(&cfg, interval),
            Command::InstallService => win::install_service(&cfg),
            Command::UninstallService => win::uninstall_service(),
            Command::RunService => win::run_service(),
        }
    }
    #[cfg(not(windows))]
    {
        stub::run(&cli.command, &cfg)
    }
}
