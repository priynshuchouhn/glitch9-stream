//! `glitch9-stream` — the engine binary. Parses CLI, wires the pipeline, runs outputs.
//!
//! Pipeline (one capture, shared GPU frame, fanned-out encoded frames):
//!   DXGI capture → GPU BGRA→NV12 → NVENC H.264 → {WebRTC, RTMPS}
//!   WASAPI → PCM → {Opus→WebRTC, AAC→RTMPS}

mod cli;
mod pipeline;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_level);

    match cli.command() {
        Command::ListDisplays => cli::list_displays(),
        Command::Run(cfg) => {
            // Tokio runtime for the async transports + signaling.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(pipeline::run(cfg))
        }
    }
}

fn init_tracing(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("g9={level},glitch9_stream={level}")));
    fmt().with_env_filter(filter).with_target(true).init();
}
