//! Pipeline wiring: capture → convert → encode → fan-out to transports, plus the
//! metrics loop and the shared-vs-dual-encoder decision.
//!
//! The video capture/convert/encode runs on a dedicated OS thread (D3D11/NVENC are
//! synchronous and GPU-bound). Encoded frames are wrapped in `Arc` and handed to each
//! transport's non-blocking `send_video`. Transports own their networking on tokio.

use crate::cli::RunConfig;
use anyhow::Result;
use g9_core::metrics::{Metrics, PipelineCounters};
use g9_core::profile::EncoderProfile;
use g9_core::transport::MediaTransport;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub async fn run(cfg: RunConfig) -> Result<()> {
    tracing::info!("glitch9-stream build: commit {}", env!("G9_GIT_HASH"));
    tracing::info!(
        "glitch9-stream starting: outputs={:?} {}x{}@{} {} bps audio={}",
        cfg.outputs,
        cfg.video.width,
        cfg.video.height,
        cfg.video.fps,
        cfg.video.bitrate_bps,
        cfg.audio.enabled
    );

    // --- Build encoder profiles for the requested outputs ---
    let webrtc_profile = cfg.outputs.webrtc.then(|| {
        EncoderProfile::webrtc(cfg.video.width, cfg.video.height, cfg.video.fps, cfg.video.bitrate_bps)
    });
    let youtube_profile = cfg.outputs.youtube.then(|| {
        EncoderProfile::youtube(cfg.video.width, cfg.video.height, cfg.video.fps, cfg.video.bitrate_bps)
    });

    // --- Decide Mode A (shared encoder) vs Mode B (dual encoders) ---
    let mode = decide_encoder_mode(webrtc_profile.as_ref(), youtube_profile.as_ref());
    tracing::info!("encoder mode: {}", mode.describe());

    // Shared "please emit a keyframe now" flag. The WebRTC transport sets it when a
    // viewer connects or sends a PLI/FIR; the video thread checks it each iteration
    // and forces an IDR on every encoder. This is what makes a joining viewer get a
    // decodable frame promptly instead of waiting for the periodic GOP keyframe.
    let force_keyframe = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // --- Build transports ---
    let mut transports: Vec<Arc<dyn MediaTransport>> = Vec::new();
    if cfg.outputs.webrtc {
        let t = Arc::new(g9_webrtc::WebRtcTransport::with_fps(
            cfg.signaling.bind_addr.clone(),
            cfg.signaling.port,
            cfg.video.fps,
        ));
        // Wire keyframe-on-demand: viewer-join and PLI both set the shared flag.
        let flag = force_keyframe.clone();
        t.set_on_viewer_join(move || {
            flag.store(true, Ordering::SeqCst);
        });
        tracing::info!("WebRTC viewer: {}", t.viewer_url());
        transports.push(t);
    }
    if cfg.outputs.youtube {
        let rtmp = cfg
            .rtmp
            .as_ref()
            .expect("rtmp config present when youtube output enabled")
            .clone();
        let t = Arc::new(g9_rtmp::RtmpTransport::new("youtube", rtmp));
        transports.push(t);
    }

    // Start all transports. A failure in one must not abort the others.
    for t in &transports {
        if let Err(e) = t.start().await {
            tracing::error!("transport {} failed to start: {}", t.name(), e);
        }
    }

    let metrics = Metrics::new();

    // --- Spawn the video capture/encode thread (real work happens on Windows+NVIDIA) ---
    let video_handle = spawn_video_thread(
        cfg_snapshot(&cfg),
        mode,
        transports.clone(),
        metrics.clone(),
        force_keyframe.clone(),
    );

    // --- Spawn the audio thread (WASAPI → Opus/AAC → transports), if enabled ---
    let audio_handle = if cfg.audio.enabled {
        spawn_audio_thread(
            cfg.audio.clone(),
            cfg.outputs.clone(),
            transports.clone(),
        )
    } else {
        tracing::info!("audio disabled (--audio false); video-only");
        None
    };

    // --- Metrics loop ---
    let stats_interval = Duration::from_secs(cfg.stats_interval_secs.max(1));
    let transports_for_stats = transports.clone();
    let metrics_for_stats = metrics.clone();
    let geom = (cfg.video.width, cfg.video.height);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(stats_interval);
        let mut prev = MetricsSnapshot::default();
        let interval_s = stats_interval.as_secs_f64();
        loop {
            ticker.tick().await;
            prev = print_metrics(&metrics_for_stats, &transports_for_stats, geom, interval_s, prev);
        }
    });

    // --- Wait for Ctrl-C, then shut down cleanly ---
    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutdown requested");
    for t in &transports {
        t.stop().await;
    }
    // Signal the worker threads to stop and join them.
    SHUTDOWN.store(true, Ordering::SeqCst);
    if let Some(h) = video_handle {
        let _ = h.join();
    }
    if let Some(h) = audio_handle {
        let _ = h.join();
    }
    Ok(())
}

/// Spawn the audio capture→encode thread. Captures WASAPI once, encodes to Opus
/// (for WebRTC) and AAC (for YouTube) as needed, and routes packets to the matching
/// transports by name. Returns None (logs) if audio can't start on this platform.
fn spawn_audio_thread(
    audio_cfg: g9_core::config::AudioConfig,
    outputs: g9_core::config::Outputs,
    transports: Vec<Arc<dyn MediaTransport>>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("g9-audio".into())
        .spawn(move || {
            if let Err(e) = audio_loop(audio_cfg, outputs, transports) {
                tracing::warn!("audio pipeline stopped: {e}");
            }
        })
        .ok()
}

/// WASAPI capture-once → feed Opus + AAC → route to transports.
fn audio_loop(
    audio_cfg: g9_core::config::AudioConfig,
    outputs: g9_core::config::Outputs,
    transports: Vec<Arc<dyn MediaTransport>>,
) -> g9_core::Result<()> {
    use g9_audio::{AacEncoder, OpusEncoder, WasapiCapture};

    let mut capture = WasapiCapture::new(audio_cfg.sample_rate, audio_cfg.channels)?;

    // Opus only if a WebRTC output exists; AAC only if a YouTube output exists.
    let mut opus = if outputs.webrtc {
        Some(OpusEncoder::new(
            audio_cfg.sample_rate,
            audio_cfg.channels,
            audio_cfg.opus_bitrate_bps,
        )?)
    } else {
        None
    };
    let mut aac = if outputs.youtube {
        Some(AacEncoder::new(
            audio_cfg.sample_rate,
            audio_cfg.channels,
            audio_cfg.aac_bitrate_bps,
        )?)
    } else {
        None
    };

    // Resolve transports by name so each codec goes to the right destination.
    let webrtc_t = transports.iter().find(|t| t.name() == "webrtc").cloned();
    let youtube_t = transports.iter().find(|t| t.name() == "youtube").cloned();

    tracing::info!("audio pipeline running (opus={}, aac={})", opus.is_some(), aac.is_some());

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        match capture.read()? {
            Some(pcm) => {
                // Opus → WebRTC
                if let (Some(enc), Some(t)) = (opus.as_mut(), webrtc_t.as_ref()) {
                    for pkt in enc.encode(&pcm)? {
                        t.send_audio(pkt);
                    }
                }
                // AAC → YouTube
                if let (Some(enc), Some(t)) = (aac.as_mut(), youtube_t.as_ref()) {
                    for pkt in enc.encode(&pcm)? {
                        t.send_audio(pkt);
                    }
                }
            }
            None => {
                // No audio ready; brief sleep to avoid a busy spin.
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
    Ok(())
}

/// Global shutdown flag read by the video thread.
static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Snapshot of the fields the video thread needs (so we don't move the whole config).
/// fps/bitrate are carried for completeness/logging; the active values live in the
/// per-output EncoderProfiles built in `run()`.
#[derive(Clone)]
#[allow(dead_code)]
pub struct VideoThreadCfg {
    pub display_index: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}
fn cfg_snapshot(cfg: &RunConfig) -> VideoThreadCfg {
    VideoThreadCfg {
        display_index: cfg.video.display_index,
        width: cfg.video.width,
        height: cfg.video.height,
        fps: cfg.video.fps,
        bitrate_bps: cfg.video.bitrate_bps,
    }
}

/// Encoder mode decision.
pub enum EncoderMode {
    /// One NVENC session serves all outputs.
    Shared(EncoderProfile),
    /// Separate NVENC sessions (shared NV12 input — capture/convert still once).
    Dual {
        webrtc: Option<EncoderProfile>,
        youtube: Option<EncoderProfile>,
    },
}

impl EncoderMode {
    pub fn describe(&self) -> String {
        match self {
            EncoderMode::Shared(p) => format!(
                "Mode A (shared) — 1 NVENC, preset={}, tuning={}, gop={}",
                p.preset, p.tuning, p.gop_frames
            ),
            EncoderMode::Dual { webrtc, youtube } => format!(
                "Mode B (dual) — 2 NVENC from 1 captured frame (webrtc={}, youtube={})",
                webrtc.is_some(),
                youtube.is_some()
            ),
        }
    }
}

/// If both outputs are on and their profiles are compatible, share one encoder.
/// If only one output is on, that single profile is the shared encoder.
/// Otherwise, dual encoders over one shared captured+converted NV12 frame.
fn decide_encoder_mode(
    webrtc: Option<&EncoderProfile>,
    youtube: Option<&EncoderProfile>,
) -> EncoderMode {
    match (webrtc, youtube) {
        (Some(w), Some(y)) => {
            if w.compatible_with(y) {
                // Prefer the stricter (low-latency) profile for the shared encoder.
                EncoderMode::Shared(w.clone())
            } else {
                EncoderMode::Dual {
                    webrtc: Some(w.clone()),
                    youtube: Some(y.clone()),
                }
            }
        }
        (Some(w), None) => EncoderMode::Shared(w.clone()),
        (None, Some(y)) => EncoderMode::Shared(y.clone()),
        (None, None) => EncoderMode::Dual {
            webrtc: None,
            youtube: None,
        },
    }
}

/// Spawn the synchronous capture→convert→encode loop on its own thread.
/// Returns None (and logs) if the platform/hardware isn't available.
fn spawn_video_thread(
    cfg: VideoThreadCfg,
    mode: EncoderMode,
    transports: Vec<Arc<dyn MediaTransport>>,
    metrics: Metrics,
    force_keyframe: Arc<std::sync::atomic::AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name("g9-video".into())
        .spawn(move || {
            if let Err(e) = video_loop(cfg, mode, transports, metrics, force_keyframe) {
                tracing::error!("video pipeline stopped: {e}");
            }
        })
        .ok();
    handle
}

/// The actual GPU pipeline. On non-Windows this returns Unsupported immediately
/// (no fake frames are produced — see rule #33).
fn video_loop(
    cfg: VideoThreadCfg,
    mode: EncoderMode,
    transports: Vec<Arc<dyn MediaTransport>>,
    metrics: Metrics,
    force_keyframe: Arc<std::sync::atomic::AtomicBool>,
) -> g9_core::Result<()> {
    use g9_capture::{Capturer, D3DContext};
    use g9_convert::Nv12Converter;
    use g9_core::time::PtsClock;
    use g9_encode::NvencEncoder;

    let ctx = D3DContext::new(None)?; // prefers NVIDIA adapter
    let mut capturer = Capturer::new(&ctx, cfg.display_index)?;
    let mut converter = Nv12Converter::new_with_ctx(&ctx, cfg.width, cfg.height)?;
    let clock = PtsClock::start_now();

    // Build encoder(s) per mode. Dual mode reuses the SAME converted NV12 texture.
    // All encoders share the capture D3D11 device so NVENC registers the NV12
    // texture directly (zero-copy) rather than copying through system memory.
    let mut encoders: Vec<NvencEncoder> = Vec::new();
    match mode {
        EncoderMode::Shared(p) => encoders.push(NvencEncoder::new_with_ctx(&ctx, p, clock.clone())?),
        EncoderMode::Dual { webrtc, youtube } => {
            if let Some(p) = webrtc {
                encoders.push(NvencEncoder::new_with_ctx(&ctx, p, clock.clone())?);
            }
            if let Some(p) = youtube {
                encoders.push(NvencEncoder::new_with_ctx(&ctx, p, clock.clone())?);
            }
        }
    }

    tracing::info!("video pipeline running: {} encoder(s)", encoders.len());

    let mut geometry_checked = false;
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        // 1) Capture one frame (GPU texture). Timeout keeps the loop responsive.
        let t_cap = std::time::Instant::now();
        let frame = match capturer.acquire_frame(16) {
            Ok(Some(f)) => f,
            Ok(None) => continue, // no new frame within timeout; try again
            Err(g9_core::Error::CaptureReinit) => {
                tracing::warn!("capture target changed; reinitializing");
                capturer = Capturer::new(&ctx, cfg.display_index)?;
                continue;
            }
            Err(e) => return Err(e),
        };
        metrics.capture_latency.observe(t_cap.elapsed());
        PipelineCounters::inc(&metrics.counters.frames_captured);

        // One-time geometry sanity check. The NV12 converter was initialized for
        // cfg.width x cfg.height, but DXGI captures the display's ACTUAL size. If
        // they differ (e.g. engine run at 1920x1080 but the desktop is 1440x900),
        // the Video Processor is fed a texture of the wrong size and emits garbage
        // or black — the stream then shows a black screen even though everything
        // else (encode/ICE/DTLS) is healthy. Warn loudly with the exact fix.
        if !geometry_checked {
            geometry_checked = true;
            if frame.width != cfg.width || frame.height != cfg.height {
                tracing::warn!(
                    "GEOMETRY MISMATCH: captured display is {}x{} but engine is encoding \
                     {}x{}. This usually causes a BLACK stream. Re-run with \
                     --width {} --height {} to match the display.",
                    frame.width, frame.height, cfg.width, cfg.height,
                    frame.width, frame.height
                );
            } else {
                tracing::info!(
                    "capture geometry OK: {}x{} matches encode size",
                    frame.width, frame.height
                );
            }
        }

        // 2) GPU BGRA → NV12 (stays on the GPU, 0 CPU readback).
        // NOTE: we never Map()/read the pixels to system memory, so
        // counters.cpu_readbacks stays 0 by construction (asserted in metrics).
        let t_cvt = std::time::Instant::now();
        let nv12 = converter.convert(&frame)?;
        metrics.convert_latency.observe(t_cvt.elapsed());
        PipelineCounters::inc(&metrics.counters.frames_converted);

        // 3) Encode with each NVENC session (shared NV12 input in dual mode).
        // If a viewer joined or sent a PLI, force the next encoded frame to be an
        // IDR (with in-band SPS/PPS) so the viewer gets a decodable keyframe now.
        if force_keyframe.swap(false, Ordering::SeqCst) {
            for enc in encoders.iter_mut() {
                enc.force_idr();
            }
        }
        let t_enc = std::time::Instant::now();
        for enc in encoders.iter_mut() {
            if let Some(encoded) = enc.encode(&nv12)? {
                PipelineCounters::inc(&metrics.counters.frames_encoded);
                let shared = Arc::new(encoded);
                // 4) Fan out to transports (non-blocking).
                for t in &transports {
                    t.send_video(shared.clone());
                }
            }
        }
        metrics.encode_latency.observe(t_enc.elapsed());
    }
    Ok(())
}

/// Cumulative counters captured at the previous metrics tick, so we can compute
/// per-interval rates (FPS) instead of lifetime totals.
#[derive(Default, Clone, Copy)]
pub struct MetricsSnapshot {
    captured: u64,
    encoded: u64,
}

/// Emit one metrics snapshot (spec §26). FPS values are measured over the interval.
/// Any value we cannot measure is reported as such — never fabricated.
fn print_metrics(
    metrics: &Metrics,
    transports: &[Arc<dyn MediaTransport>],
    geom: (u32, u32),
    interval_s: f64,
    prev: MetricsSnapshot,
) -> MetricsSnapshot {
    let c = &metrics.counters;
    let captured = PipelineCounters::get(&c.frames_captured);
    let encoded = PipelineCounters::get(&c.frames_encoded);
    let cap_fps = (captured.saturating_sub(prev.captured)) as f64 / interval_s;
    let enc_fps = (encoded.saturating_sub(prev.encoded)) as f64 / interval_s;

    tracing::info!(
        "metrics: capture={:.0}fps encode={:.0}fps res={}x{} \
         lat(capture={:.1}ms convert={:.1}ms encode={:.1}ms) \
         cpu_readbacks={} dropped(capture={} encoder={})",
        cap_fps,
        enc_fps,
        geom.0,
        geom.1,
        metrics.capture_latency.millis(),
        metrics.convert_latency.millis(),
        metrics.encode_latency.millis(),
        PipelineCounters::get(&c.cpu_readbacks),
        PipelineCounters::get(&c.dropped_capture),
        PipelineCounters::get(&c.dropped_encoder),
    );
    for t in transports {
        let s = t.stats();
        tracing::info!(
            "  {}: state={:?} viewers={:?} bitrate={:?} rtt={:?} loss={:?} jitter={:?} \
             bytes_sent={:?} reconnects={:?} dropped={:?}",
            t.name(),
            s.state,
            s.viewers,
            s.bitrate_bps,
            s.rtt_ms,
            s.packet_loss,
            s.jitter_ms,
            s.bytes_sent,
            s.reconnects,
            s.dropped_frames,
        );
    }
    MetricsSnapshot { captured, encoded }
}
