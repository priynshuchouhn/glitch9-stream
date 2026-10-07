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

/// Removes an obsolete readiness marker without making playback depend on disk I/O.
fn clear_ready_file(path: Option<&str>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

pub async fn run(cfg: RunConfig) -> Result<()> {
    clear_ready_file(cfg.ready_file.as_deref());
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

    // Shared facecam state (populated by the WHEP subscriber task below when a
    // facecam URL is configured; consumed by the video/audio worker threads).
    let facecam = crate::facecam::FacecamState::new();

    // --- Build encoder profiles for the requested outputs ---
    let webrtc_profile = cfg.outputs.webrtc.then(|| {
        EncoderProfile::webrtc(
            cfg.video.width,
            cfg.video.height,
            cfg.video.fps,
            cfg.video.bitrate_bps,
        )
    });
    let youtube_profile = cfg.outputs.youtube.then(|| {
        EncoderProfile::youtube(
            cfg.video.width,
            cfg.video.height,
            cfg.video.fps,
            cfg.video.bitrate_bps,
        )
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
    // Adaptive-bitrate target shared from the WebRTC transport to the video thread.
    // None when WebRTC isn't an output (RTMP-only runs at the fixed profile bitrate).
    let mut abr_target: Option<Arc<std::sync::atomic::AtomicU32>> = None;
    if cfg.outputs.webrtc {
        match &cfg.signaling.whip {
            // Production: publish ONCE to the SFU via WHIP; the SFU fans out to
            // viewers (handles NAT/scale). On-connect IDR is driven by the SFU's
            // RTCP (PLI); force a keyframe periodically via the normal GOP.
            Some(whip) => {
                let public_ip = std::env::var("G9_PUBLIC_IP")
                    .ok()
                    .filter(|s| !s.trim().is_empty());
                let t = Arc::new(g9_webrtc::WhipTransport::new(
                    whip.url.clone(),
                    whip.token.clone(),
                    public_ip,
                    cfg.video.fps,
                ));
                tracing::info!("WebRTC publish (WHIP) -> {}", whip.url);
                transports.push(t);
                // Force an initial IDR shortly after startup so the SFU/first viewer
                // gets a decodable keyframe (WHIP has no on-join callback).
                force_keyframe.store(true, Ordering::SeqCst);
            }
            // Direct mode (dev/LAN): serve browsers from the local signaling server.
            None => {
                let t = Arc::new(g9_webrtc::WebRtcTransport::with_params(
                    cfg.signaling.bind_addr.clone(),
                    cfg.signaling.port,
                    cfg.video.fps,
                    cfg.video.bitrate_bps,
                ));
                let flag = force_keyframe.clone();
                t.set_on_viewer_join(move || {
                    flag.store(true, Ordering::SeqCst);
                });
                abr_target = Some(t.target_bitrate_handle());
                tracing::info!("WebRTC viewer: {}", t.viewer_url());
                transports.push(t);
            }
        }
    }
    if cfg.outputs.youtube {
        let rtmp = cfg
            .rtmp
            .as_ref()
            .expect("rtmp config present when youtube output enabled")
            .clone();
        let t = Arc::new(g9_rtmp::RtmpTransport::new(
            "youtube",
            rtmp,
            force_keyframe.clone(),
        ));
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
        abr_target,
        // The video thread composites the facecam over the game when present.
        cfg.facecam_whep.as_ref().map(|_| facecam.clone()),
    );

    // --- Spawn the audio thread (WASAPI → Opus/AAC → transports), if enabled ---
    let audio_handle = if cfg.audio.enabled {
        spawn_audio_thread(
            cfg.audio.clone(),
            cfg.outputs.clone(),
            transports.clone(),
            // The audio thread mixes the facecam mic into the broadcast when present.
            cfg.facecam_whep.as_ref().map(|_| facecam.clone()),
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
    let ready_file = cfg.ready_file.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(stats_interval);
        let mut prev = MetricsSnapshot::default();
        let mut previous_whip_bytes = 0;
        let interval_s = stats_interval.as_secs_f64();
        loop {
            ticker.tick().await;
            prev = print_metrics(
                &metrics_for_stats,
                &transports_for_stats,
                geom,
                interval_s,
                prev,
            );
            if let Some(path) = ready_file.as_deref() {
                let current_whip_bytes = transports_for_stats.iter().find_map(|transport| {
                    let stats = transport.stats();
                    (transport.name() == "whip"
                        && stats.state == Some(g9_core::transport::TransportState::Connected))
                    .then(|| stats.bytes_sent.unwrap_or(0))
                });
                let publishing =
                    current_whip_bytes.is_some_and(|bytes| bytes > previous_whip_bytes);
                previous_whip_bytes = current_whip_bytes.unwrap_or(0);
                if publishing {
                    let _ = std::fs::write(path, b"ready\n");
                } else {
                    clear_ready_file(Some(path));
                }
            }
        }
    });

    // --- Facecam (optional): subscribe to the player's browser-published camera +
    //     mic via WHEP. The camera is composited over the game (video thread) and
    //     the mic is mixed into the broadcast audio (audio thread). When no facecam
    //     URL is configured this is skipped and the broadcast is game-only.
    let facecam_task = cfg.facecam_whep.clone().map(|whep_url| {
        let state = facecam.clone();
        tokio::spawn(async move {
            // The engine starts well before the player's browser opens the webcam
            // tab, grants camera permission, and publishes to the SFU. So the first
            // WHEP subscribe typically 404s ("no stream available"). Retry with
            // bounded backoff until the publisher appears, and re-subscribe if the
            // facecam later drops (player closes/reopens the cam), until shutdown.
            const RETRY_MIN_MS: u64 = 1_000;
            const RETRY_MAX_MS: u64 = 5_000;
            let mut backoff_ms = RETRY_MIN_MS;
            while !SHUTDOWN.load(Ordering::SeqCst) {
                match g9_webrtc::WhepSubscriber::connect(&whep_url).await {
                    Ok((sub, mut rx)) => {
                        tracing::info!("facecam: subscribed to {}", whep_url);
                        backoff_ms = RETRY_MIN_MS; // reset after a successful connect
                        while let Some(sample) = rx.recv().await {
                            if SHUTDOWN.load(Ordering::SeqCst) {
                                break;
                            }
                            match sample {
                                g9_webrtc::FacecamSample::Video(codec, au) => {
                                    let codec = match codec {
                                        g9_webrtc::FacecamVideoCodec::H264 => {
                                            g9_capture::FacecamCodec::H264
                                        }
                                        g9_webrtc::FacecamVideoCodec::Vp8 => {
                                            g9_capture::FacecamCodec::Vp8
                                        }
                                    };
                                    state.push_video(codec, au);
                                }
                                g9_webrtc::FacecamSample::Audio(pkt) => state.push_audio(pkt),
                            }
                        }
                        sub.close().await;
                        // Channel closed: the facecam stream ended. Loop to re-subscribe
                        // in case the player brings their camera back.
                        if SHUTDOWN.load(Ordering::SeqCst) {
                            break;
                        }
                        tracing::info!("facecam: stream ended; will try to re-subscribe");
                    }
                    Err(e) => {
                        // Expected while the browser hasn't published yet. Logged at
                        // info so operators can see the facecam is waiting for the
                        // publisher (vs a real failure) during go-live bring-up.
                        tracing::info!(
                            "facecam: subscribe not ready ({e:#}); retrying in {backoff_ms}ms"
                        );
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RETRY_MAX_MS);
            }
        })
    });

    // --- Wait for Ctrl-C, then shut down cleanly ---
    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutdown requested");
    if let Some(h) = facecam_task {
        h.abort();
    }
    clear_ready_file(cfg.ready_file.as_deref());
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
    facecam: Option<crate::facecam::FacecamState>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("g9-audio".into())
        .spawn(move || {
            if let Err(e) = audio_loop(audio_cfg, outputs, transports, facecam) {
                tracing::warn!("audio pipeline stopped: {e}");
            }
        })
        .ok()
}

/// Sum `mic` interleaved f32 PCM into `base` in place, clamping to [-1, 1]. Mixes
/// as many samples as overlap (the mic packet may be shorter/longer than the
/// current loopback chunk); any excess mic samples are dropped for this chunk.
/// Both are assumed 48 kHz stereo (the broadcast audio format), so no resampling.
fn mix_into(base: &mut [f32], mic: &[f32]) {
    let n = base.len().min(mic.len());
    for i in 0..n {
        base[i] = (base[i] + mic[i]).clamp(-1.0, 1.0);
    }
}

/// Resample interleaved stereo microphone PCM to the WASAPI device rate. Browser
/// Opus always decodes at 48 kHz, while the VM loopback device commonly runs at
/// 44.1 kHz; mixing the arrays without conversion causes drift and dropped voice.
fn resample_stereo(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if in_rate == out_rate || input.len() < 4 {
        return input.to_vec();
    }
    let input_frames = input.len() / 2;
    let output_frames = ((input_frames as u64 * out_rate as u64) / in_rate as u64) as usize;
    let step = in_rate as f64 / out_rate as f64;
    let mut out = Vec::with_capacity(output_frames * 2);
    for output_frame in 0..output_frames {
        let pos = output_frame as f64 * step;
        let left = (pos.floor() as usize).min(input_frames - 1);
        let right = (left + 1).min(input_frames - 1);
        let fraction = (pos - left as f64) as f32;
        for channel in 0..2 {
            let a = input[left * 2 + channel];
            let b = input[right * 2 + channel];
            out.push(a + (b - a) * fraction);
        }
    }
    out
}

/// WASAPI capture-once → feed Opus + AAC → route to transports.
fn audio_loop(
    audio_cfg: g9_core::config::AudioConfig,
    outputs: g9_core::config::Outputs,
    transports: Vec<Arc<dyn MediaTransport>>,
    facecam: Option<crate::facecam::FacecamState>,
) -> g9_core::Result<()> {
    use g9_audio::{AacEncoder, OpusDecoder, OpusEncoder, WasapiCapture};

    let mut capture = WasapiCapture::new(audio_cfg.sample_rate, audio_cfg.channels)?;

    // CRITICAL: WASAPI shared-mode loopback uses the DEVICE mix format, not what we
    // requested. The encoders must be built for the ACTUAL captured format or the
    // PCM is misinterpreted (wrong rate/channels) → choppy, laggy, wrong-pitch audio.
    let actual_rate = capture.sample_rate();
    let actual_channels = capture.channels();
    tracing::info!(
        "audio capture format: {} Hz, {} ch (requested {} Hz, {} ch)",
        actual_rate,
        actual_channels,
        audio_cfg.sample_rate,
        audio_cfg.channels
    );
    if actual_rate != 48000 {
        tracing::info!(
            "WASAPI mix is {} Hz; Opus encoder will resample to 48000 for WebRTC",
            actual_rate
        );
    }

    // Opus only if a WebRTC output exists; AAC only if a YouTube output exists.
    let mut opus = if outputs.webrtc {
        Some(OpusEncoder::new(
            actual_rate,
            actual_channels,
            audio_cfg.opus_bitrate_bps,
        )?)
    } else {
        None
    };
    let mut aac = if outputs.youtube {
        Some(AacEncoder::new(
            actual_rate,
            actual_channels,
            audio_cfg.aac_bitrate_bps,
        )?)
    } else {
        None
    };

    // Resolve transports by name so each codec goes to the right destination.
    // Opus feeds the WebRTC-family output, which is either the direct-serve
    // transport ("webrtc", dev/LAN) or the SFU publisher ("whip", production).
    // Matching only "webrtc" silently dropped audio in WHIP mode — the Opus track
    // was negotiated but never fed, so the SFU showed video only.
    let webrtc_t = transports
        .iter()
        .find(|t| matches!(t.name(), "webrtc" | "whip"))
        .cloned();
    let youtube_t = transports.iter().find(|t| t.name() == "youtube").cloned();

    // Facecam mic: decode the player's Opus voice (published from the browser,
    // always 48 kHz / 2 ch per the WHIP sender) so it can be mixed into the system
    // audio before re-encode. Only when a facecam is present.
    let mut mic_decoder = match facecam.as_ref() {
        Some(_) => OpusDecoder::new(48_000, 2).ok(),
        None => None,
    };
    let mut mic_samples = std::collections::VecDeque::<f32>::new();
    let mut mic_packets_received: u64 = 0;

    tracing::info!(
        "audio pipeline running (opus={}, aac={}, webrtc_audio={}, facecam_mic={})",
        opus.is_some(),
        aac.is_some(),
        webrtc_t.is_some(),
        mic_decoder.is_some(),
    );

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        match capture.read()? {
            Some(mut pcm) => {
                // Mix the player's microphone (facecam) into the system/game audio.
                // Decode queued Opus voice packets to PCM and sum into `pcm` with a
                // simple clamp. Both are interleaved f32; mic is 48 kHz/2ch, which
                // matches the broadcast path (loopback is typically 48 kHz too).
                if let (Some(dec), Some(fc)) = (mic_decoder.as_mut(), facecam.as_ref()) {
                    for packet in fc.drain_audio() {
                        if let Ok(mic_pcm) = dec.decode(&packet) {
                            mic_packets_received += 1;
                            mic_samples.extend(resample_stereo(&mic_pcm, 48_000, actual_rate));
                            if mic_packets_received == 1 {
                                tracing::info!(
                                    "facecam microphone: receiving and mixing Opus audio"
                                );
                            }
                        }
                    }
                    let mixed: Vec<f32> = (0..pcm.samples.len())
                        .map(|_| mic_samples.pop_front().unwrap_or(0.0))
                        .collect();
                    mix_into(&mut pcm.samples, &mixed);
                }
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

/// Which transport(s) may receive the bitstream produced by an encoder.
///
/// A dual-mode encoder has its own H.264 reference-frame history. Mixing the two
/// encoded streams on either transport makes every interleaved P-frame reference
/// the wrong history and produces severe decoder corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderRoute {
    All,
    WebRtc,
    Youtube,
}

impl EncoderRoute {
    fn accepts(self, transport_name: &str) -> bool {
        match self {
            Self::All => true,
            Self::WebRtc => matches!(transport_name, "webrtc" | "whip"),
            Self::Youtube => transport_name == "youtube",
        }
    }

    fn uses_webrtc_abr(self) -> bool {
        self != Self::Youtube
    }
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
    abr_target: Option<Arc<std::sync::atomic::AtomicU32>>,
    facecam: Option<crate::facecam::FacecamState>,
) -> Option<std::thread::JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name("g9-video".into())
        .spawn(move || {
            if let Err(e) = video_loop(
                cfg,
                mode,
                transports,
                metrics,
                force_keyframe,
                abr_target,
                facecam,
            ) {
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
    abr_target: Option<Arc<std::sync::atomic::AtomicU32>>,
    facecam: Option<crate::facecam::FacecamState>,
) -> g9_core::Result<()> {
    use g9_capture::{Capturer, D3DContext, FacecamCompositor, GpuFrameCache};
    use g9_convert::Nv12Converter;
    use g9_core::time::PtsClock;
    use g9_encode::NvencEncoder;

    let ctx = D3DContext::new(None)?; // prefers NVIDIA adapter
    let mut capturer = Capturer::new(&ctx, cfg.display_index)?;
    let mut converter = Nv12Converter::new_with_ctx(&ctx, cfg.width, cfg.height)?;
    let clock = PtsClock::start_now();

    // Facecam compositor (optional): decodes the camera H.264 and blends it over
    // the game texture before NV12 conversion. Built lazily so a facecam that
    // fails to initialize never breaks the game-only broadcast.
    let mut compositor: Option<FacecamCompositor> = match &facecam {
        Some(_) => match FacecamCompositor::new(&ctx, cfg.width, cfg.height) {
            Ok(c) => {
                tracing::info!("facecam compositor ready; will composite camera over game");
                Some(c)
            }
            Err(e) => {
                tracing::warn!("facecam compositor unavailable: {e}; broadcasting game only");
                None
            }
        },
        None => None,
    };
    // Count facecam frames pulled by the video thread, for first-frames diagnostics.
    let mut facecam_takes: u64 = 0;
    let mut composite_errors: u64 = 0;
    // DXGI reports only desktop changes. When the game is static/minimized, reuse
    // a safe GPU copy so incoming camera frames still advance on the broadcast.
    let mut frame_cache = facecam.as_ref().map(|_| GpuFrameCache::new());
    let mut cache_errors: u64 = 0;

    // Build encoder(s) per mode. Dual mode reuses the SAME converted NV12 texture.
    // All encoders share the capture D3D11 device so NVENC registers the NV12
    // texture directly (zero-copy) rather than copying through system memory.
    let mut encoders: Vec<(NvencEncoder, EncoderRoute)> = Vec::new();
    match mode {
        EncoderMode::Shared(p) => encoders.push((
            NvencEncoder::new_with_ctx(&ctx, p, clock.clone())?,
            EncoderRoute::All,
        )),
        EncoderMode::Dual { webrtc, youtube } => {
            if let Some(p) = webrtc {
                encoders.push((
                    NvencEncoder::new_with_ctx(&ctx, p, clock.clone())?,
                    EncoderRoute::WebRtc,
                ));
            }
            if let Some(p) = youtube {
                encoders.push((
                    NvencEncoder::new_with_ctx(&ctx, p, clock.clone())?,
                    EncoderRoute::Youtube,
                ));
            }
        }
    }

    tracing::info!("video pipeline running: {} encoder(s)", encoders.len());

    // Frame-rate limiter. DXGI presents at the display's native rate (often 60-75fps
    // on this VM), but NVENC is configured for cfg.fps and its CBR is sized for that
    // rate. If we encode EVERY captured frame, we feed NVENC ~2x its declared rate,
    // so the real bitrate overshoots the target (seen as 6.6 Mbps when 3 Mbps was
    // requested) and a remote viewer drops ~40-60% of packets -> black. Throttle to
    // the configured fps by skipping frames that arrive before the next frame slot.
    let target_fps = cfg.fps.max(1);
    let frame_interval = Duration::from_secs_f64(1.0 / target_fps as f64);
    let mut next_frame_at = std::time::Instant::now();
    let mut next_abr_check = std::time::Instant::now();

    let mut geometry_checked = false;
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        // 1) Capture one frame (GPU texture). Timeout keeps the loop responsive.
        let t_cap = std::time::Instant::now();
        let (frame, fresh_capture) = match capturer.acquire_frame(16) {
            Ok(Some(f)) => (f, true),
            Ok(None) => match frame_cache.as_ref().and_then(GpuFrameCache::latest) {
                Some(f) => (f, false),
                None => continue,
            },
            Err(g9_core::Error::CaptureReinit) => {
                tracing::warn!("capture target changed; reinitializing");
                capturer = Capturer::new(&ctx, cfg.display_index)?;
                continue;
            }
            Err(e) => return Err(e),
        };
        if fresh_capture {
            metrics.capture_latency.observe(t_cap.elapsed());
            PipelineCounters::inc(&metrics.counters.frames_captured);
        }

        // Rate-limit: if this frame arrived before its slot, drop it (don't encode).
        // We still counted the capture above (for capture-fps visibility) but skip
        // convert+encode so the encoder stays at the configured fps.
        let now = std::time::Instant::now();
        if now < next_frame_at {
            continue;
        }
        next_frame_at = now + frame_interval;

        // Copy only frames selected for encoding, avoiding an unnecessary 60 fps
        // GPU copy when capture runs faster than the configured output rate.
        if fresh_capture {
            if let Some(cache) = frame_cache.as_mut() {
                if let Err(e) = cache.update(&ctx, &frame) {
                    cache_errors += 1;
                    if cache_errors <= 5 || cache_errors % 300 == 0 {
                        tracing::warn!("desktop frame-cache error #{cache_errors}: {e}");
                    }
                }
            }
        }

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
                    frame.width,
                    frame.height,
                    cfg.width,
                    cfg.height,
                    frame.width,
                    frame.height
                );
            } else {
                tracing::info!(
                    "capture geometry OK: {}x{} matches encode size",
                    frame.width,
                    frame.height
                );
            }
        }

        // 1b) Composite the facecam over the game texture (GPU) when present. The
        // compositor pulls the newest decoded camera frame and blends it into a
        // corner of `frame`; on any error it leaves the game frame untouched.
        let mut composited_frame = None;
        if let (Some(comp), Some(fc)) = (compositor.as_mut(), facecam.as_ref()) {
            if let Some((codec, au)) = fc.take_video() {
                facecam_takes += 1;
                if facecam_takes <= 5 {
                    tracing::info!(
                        "facecam video thread: took {codec:?} AU ({} bytes) #{facecam_takes}",
                        au.len()
                    );
                }
                if let Err(e) = comp.update_camera(codec, &au) {
                    if facecam_takes <= 10 {
                        tracing::info!("facecam update_camera error: {e}");
                    }
                }
            }
            match comp.composite(&frame) {
                Ok(output) => composited_frame = output,
                Err(e) => {
                    composite_errors += 1;
                    if composite_errors <= 5 || composite_errors % 300 == 0 {
                        tracing::warn!("facecam composite error #{composite_errors}: {e}");
                    }
                }
            }
        }

        // 2) GPU BGRA → NV12 (stays on the GPU, 0 CPU readback).
        // NOTE: we never Map()/read the pixels to system memory, so
        // counters.cpu_readbacks stays 0 by construction (asserted in metrics).
        let t_cvt = std::time::Instant::now();
        let frame_to_convert = composited_frame.as_ref().unwrap_or(&frame);
        let nv12 = converter.convert(frame_to_convert)?;
        metrics.convert_latency.observe(t_cvt.elapsed());
        PipelineCounters::inc(&metrics.counters.frames_converted);

        // 3) Encode with each NVENC session (shared NV12 input in dual mode).
        // If a viewer joined or sent a PLI, force the next encoded frame to be an
        // IDR (with in-band SPS/PPS) so the viewer gets a decodable keyframe now.
        if force_keyframe.swap(false, Ordering::SeqCst) {
            for (enc, _) in encoders.iter_mut() {
                enc.force_idr();
            }
        }

        // Adaptive bitrate: apply the latest WebRTC target to the encoder(s). The
        // transport updates this from RTCP Receiver Reports (loss-based control).
        // Only reconfigure when it actually changed, and at most ~twice a second.
        if let Some(abr) = &abr_target {
            let now = std::time::Instant::now();
            if now >= next_abr_check {
                next_abr_check = now + Duration::from_millis(500);
                let target = abr.load(Ordering::Relaxed);
                for (enc, route) in encoders.iter_mut() {
                    // WebRTC receiver feedback must never change the fixed-rate
                    // YouTube encoder in dual mode.
                    if !route.uses_webrtc_abr() {
                        continue;
                    }
                    if enc.current_bitrate_bps() != target {
                        if let Err(e) = enc.set_bitrate(target) {
                            tracing::warn!("ABR set_bitrate({target}) failed: {e}");
                        }
                    }
                }
            }
        }
        let t_enc = std::time::Instant::now();
        for (enc, route) in encoders.iter_mut() {
            if let Some(encoded) = enc.encode(&nv12)? {
                PipelineCounters::inc(&metrics.counters.frames_encoded);
                let shared = Arc::new(encoded);
                // 4) Fan out only to transports assigned to this encoder. In
                // shared mode the one bitstream intentionally feeds all outputs.
                for t in &transports {
                    if route.accepts(t.name()) {
                        t.send_video(shared.clone());
                    }
                }
            }
        }
        metrics.encode_latency.observe(t_enc.elapsed());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::EncoderRoute;

    #[test]
    fn dual_encoder_routes_are_isolated() {
        assert!(EncoderRoute::WebRtc.accepts("webrtc"));
        assert!(EncoderRoute::WebRtc.accepts("whip"));
        assert!(!EncoderRoute::WebRtc.accepts("youtube"));
        assert!(EncoderRoute::Youtube.accepts("youtube"));
        assert!(!EncoderRoute::Youtube.accepts("webrtc"));
        assert!(!EncoderRoute::Youtube.accepts("whip"));
    }

    #[test]
    fn youtube_encoder_ignores_webrtc_abr() {
        assert!(EncoderRoute::All.uses_webrtc_abr());
        assert!(EncoderRoute::WebRtc.uses_webrtc_abr());
        assert!(!EncoderRoute::Youtube.uses_webrtc_abr());
    }
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
