//! Non-Windows stub. The manager drives Windows RDP sessions, so it only runs on
//! Windows. This lets the workspace type-check on macOS/Linux.

use crate::cli::{Command, Config};

pub fn run(_command: &Command, _cfg: &Config) -> anyhow::Result<()> {
    anyhow::bail!("glitch9-manager runs on Windows only (uses the WTS session API)")
}
