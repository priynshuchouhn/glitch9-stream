//! CLI definition and the display-listing command.

use anyhow::Result;
use clap::Parser;
use g9_core::config::{
    AudioConfig, Outputs, RtmpConfig, Secret, SignalingConfig, VideoConfig,
};

/// Glitch9 lightweight streaming engine (DXGI → D3D11 → NVENC → WebRTC/RTMPS).
#[derive(Parser, Debug)]
#[command(name = "glitch9-stream", version, about)]
pub struct Cli {
    /// List available displays and exit.
    #[arg(long)]
    pub list_displays: bool,

    /// DEBUG: capture one frame from --display to this PPM file and exit. Use this
    /// to see what DXGI actually grabs (diagnoses black-stream vs capture issues).
    #[arg(long)]
    pub dump_frame: Option<String>,

    /// Comma-separated outputs: webrtc, youtube, or "webrtc,youtube".
    #[arg(long, default_value = "webrtc")]
    pub output: String,

    /// Display index to capture (see --list-displays).
    #[arg(long, default_value_t = 0)]
    pub display: u32,

    #[arg(long, default_value_t = 1920)]
    pub width: u32,
    #[arg(long, default_value_t = 1080)]
    pub height: u32,
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    /// Target bitrate in bits/sec (4_000_000 – 12_000_000 recommended).
    #[arg(long, default_value_t = 8_000_000)]
    pub bitrate: u32,

    /// Capture + stream audio. Accepts `--audio true` / `--audio false`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub audio: bool,

    /// RTMP(S) ingest URL (no stream key in the URL). Reusable: YouTube/Twitch/custom.
    #[arg(long)]
    pub rtmp_url: Option<String>,

    /// Convenience alias that defaults rtmp_url to YouTube's primary ingest.
    #[arg(long)]
    pub youtube: bool,

    /// Stream key (secret). Prefer the G9_STREAM_KEY env var over this flag.
    /// Never logged.
    #[arg(long)]
    pub stream_key: Option<String>,

    /// Also accepted for symmetry with YouTube docs.
    #[arg(long)]
    pub youtube_stream_key: Option<String>,

    /// Local signaling + viewer server port.
    #[arg(long, default_value_t = 8080)]
    pub port: u16,

    /// Bind address for signaling/viewer. 127.0.0.1 by default; set 0.0.0.0 for LAN.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,

    /// Metrics snapshot interval (seconds).
    #[arg(long, default_value_t = 5)]
    pub stats_interval: u64,

    #[arg(long, default_value = "info")]
    pub log_level: String,
}

/// Fully-resolved run configuration passed to the pipeline.
#[derive(Debug)]
pub struct RunConfig {
    pub outputs: Outputs,
    pub video: VideoConfig,
    pub audio: AudioConfig,
    pub rtmp: Option<RtmpConfig>,
    pub signaling: SignalingConfig,
    pub stats_interval_secs: u64,
}

pub enum Command {
    ListDisplays,
    DumpFrame { display: u32, path: String },
    Run(RunConfig),
}

/// YouTube's primary RTMPS ingest endpoint. Overridable via --rtmp-url so the
/// transport stays reusable for Twitch/Facebook/custom servers.
const YOUTUBE_DEFAULT_INGEST: &str = "rtmps://a.rtmps.youtube.com/live2";

impl Cli {
    pub fn command(self) -> Command {
        if self.list_displays {
            return Command::ListDisplays;
        }
        if let Some(path) = self.dump_frame.clone() {
            return Command::DumpFrame {
                display: self.display,
                path,
            };
        }

        let outputs = Outputs::parse(&self.output).unwrap_or_else(|e| {
            eprintln!("error: {e}");
            std::process::exit(2);
        });

        // Resolve RTMP config only if a YouTube/RTMP output is requested.
        let rtmp = if outputs.youtube {
            let url = self
                .rtmp_url
                .clone()
                .or(if self.youtube {
                    Some(YOUTUBE_DEFAULT_INGEST.to_string())
                } else {
                    None
                })
                .unwrap_or_else(|| YOUTUBE_DEFAULT_INGEST.to_string());

            // Key precedence: env var > --stream-key > --youtube-stream-key.
            let key = std::env::var("G9_STREAM_KEY")
                .ok()
                .or(self.stream_key.clone())
                .or(self.youtube_stream_key.clone())
                .unwrap_or_default();

            if key.is_empty() {
                eprintln!(
                    "error: youtube output selected but no stream key \
                     (set G9_STREAM_KEY or --stream-key)"
                );
                std::process::exit(2);
            }
            Some(RtmpConfig {
                url,
                stream_key: Secret::new(key),
            })
        } else {
            None
        };

        Command::Run(RunConfig {
            outputs,
            video: VideoConfig {
                display_index: self.display,
                width: self.width,
                height: self.height,
                fps: self.fps,
                bitrate_bps: self.bitrate,
            },
            audio: AudioConfig {
                enabled: self.audio,
                ..AudioConfig::default()
            },
            rtmp,
            signaling: SignalingConfig {
                bind_addr: self.bind,
                port: self.port,
            },
            stats_interval_secs: self.stats_interval,
        })
    }
}

/// `--dump-frame` implementation: grab one frame from the display and save it, so we
/// can confirm whether DXGI is capturing real desktop pixels or a black surface.
pub fn dump_frame(display: u32, path: &str) -> Result<()> {
    use g9_capture::{Capturer, D3DContext};
    let ctx = D3DContext::new(None)?;
    let mut cap = Capturer::new(&ctx, display)?;
    let (w, h) = cap.dump_one_frame(&ctx, path)?;
    println!("wrote {w}x{h} frame to {path}");
    println!("open it to see what the capture grabbed; a black image means DXGI is");
    println!("capturing a blank surface (likely RDP/duplication), not the GPU desktop.");
    Ok(())
}

/// `--list-displays` implementation. Uses the capture crate's DXGI enumeration.
pub fn list_displays() -> Result<()> {
    use g9_capture::D3DContext;
    match D3DContext::enumerate_adapters() {
        Ok(adapters) => {
            println!("Adapters:");
            for a in &adapters {
                println!(
                    "  [{}] {}  ({} MB VRAM, feature level {}){}",
                    a.index,
                    a.description,
                    a.dedicated_vram_mb,
                    a.feature_level,
                    if a.is_nvidia { "  [NVIDIA]" } else { "" }
                );
            }
        }
        Err(e) => eprintln!("could not enumerate adapters: {e}"),
    }
    match D3DContext::enumerate_displays() {
        Ok(displays) => {
            println!("Displays:");
            for d in &displays {
                println!(
                    "  --display {}  {}  {}x{} (adapter {}){}",
                    d.index,
                    d.device_name,
                    d.width,
                    d.height,
                    d.adapter_index,
                    if d.is_attached { "" } else { "  [detached]" }
                );
            }
        }
        Err(e) => {
            eprintln!("could not enumerate displays: {e}");
            eprintln!(
                "note: on RDSH/RDP sessions this needs a GPU-backed output \
                 (UseWddmDriver=1). See docs/RUN.md."
            );
        }
    }
    Ok(())
}
