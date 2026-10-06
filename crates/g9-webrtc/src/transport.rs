//! WebRTC transport implementing `MediaTransport`, built on webrtc-rs 0.11.
//!
//! Topology (view-only, VM → browser):
//! - A `MediaEngine` registers H.264 (packetization-mode=1) and Opus.
//! - One `TrackLocalStaticSample` for video, one for audio.
//! - A local WebSocket signaling server (`signaling_server`) accepts a browser,
//!   creates a `RTCPeerConnection`, adds the tracks (sendonly), applies the browser
//!   offer, and returns the answer. ICE is trickled both ways. No DataChannel, no
//!   input — purely one-way media.
//! - `send_video`/`send_audio` push into bounded channels; `writer_loop` drains them
//!   and `write_sample`s to the track. On overflow we drop stale frames so the
//!   encoder is never blocked. webrtc-rs performs RFC 6184 RTP payloading + SRTP.
//! - On viewer connect we invoke `on_viewer_join` so the engine forces an IDR,
//!   guaranteeing the new viewer gets decodable config promptly.

use async_trait::async_trait;
use g9_core::frame::SharedEncodedFrame;
use g9_core::transport::{AudioPacket, MediaTransport, TransportState, TransportStats};
use g9_core::Result;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::media::Sample;

const VIDEO_QUEUE_DEPTH: usize = 8;
const AUDIO_QUEUE_DEPTH: usize = 32;

pub struct WebRtcTransport {
    name: String,
    bind_addr: String,
    port: u16,
    fps: u32,
    state: Arc<AtomicU8>,
    viewers: Arc<AtomicU32>,
    bytes_sent: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    video_tx: mpsc::Sender<SharedEncodedFrame>,
    video_rx: Mutex<Option<mpsc::Receiver<SharedEncodedFrame>>>,
    audio_tx: mpsc::Sender<AudioPacket>,
    audio_rx: Mutex<Option<mpsc::Receiver<AudioPacket>>>,
    on_viewer_join: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>,
}

impl WebRtcTransport {
    pub fn new(bind_addr: impl Into<String>, port: u16) -> Self {
        Self::with_fps(bind_addr, port, 60)
    }

    pub fn with_fps(bind_addr: impl Into<String>, port: u16, fps: u32) -> Self {
        let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE_DEPTH);
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE_DEPTH);
        Self {
            name: "webrtc".into(),
            bind_addr: bind_addr.into(),
            port,
            fps,
            state: Arc::new(AtomicU8::new(TransportState::Idle as u8)),
            viewers: Arc::new(AtomicU32::new(0)),
            bytes_sent: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            video_tx,
            video_rx: Mutex::new(Some(video_rx)),
            audio_tx,
            audio_rx: Mutex::new(Some(audio_rx)),
            on_viewer_join: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_on_viewer_join<F: Fn() + Send + Sync + 'static>(&self, f: F) {
        *self.on_viewer_join.lock() = Some(Box::new(f));
    }

    pub fn viewer_url(&self) -> String {
        format!("http://{}:{}/", self.bind_addr, self.port)
    }

    /// Build the webrtc-rs API with H.264 + Opus registered.
    fn build_api() -> Result<webrtc::api::API> {
        let mut m = MediaEngine::default();
        // H.264, packetization-mode=1, baseline-ish profile — broadly browser-compatible.
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 102,
                ..Default::default()
            },
            RTPCodecType::Video,
        )
        .map_err(|e| g9_core::Error::transport(format!("register H264: {e}")))?;
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .map_err(|e| g9_core::Error::transport(format!("register Opus: {e}")))?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)
            .map_err(|e| g9_core::Error::transport(format!("interceptors: {e}")))?;

        // ICE setup for same-machine / LAN connectivity. The default webrtc-rs
        // SettingEngine excludes loopback candidates, so a browser on 127.0.0.1 can
        // never pair with us (ICE goes to `failed` after ~30s — exactly the symptom
        // observed). Enable loopback host candidates and allow UDP so the host
        // candidate pair succeeds without needing external STUN.
        let mut se = webrtc::api::setting_engine::SettingEngine::default();
        se.set_network_types(vec![
            webrtc::ice::network_type::NetworkType::Udp4,
            webrtc::ice::network_type::NetworkType::Udp6,
        ]);
        // Accept ALL interfaces when gathering host candidates, including loopback.
        // webrtc-ice excludes loopback by default, which blocks a browser on
        // 127.0.0.1 from pairing with us (ICE → failed). Accepting every interface
        // yields a usable host candidate for same-machine and LAN viewers.
        se.set_interface_filter(Box::new(|_name: &str| -> bool { true }));

        Ok(APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .with_setting_engine(se)
            .build())
    }

    /// Create the shared video/audio tracks and the writer loops that drain the
    /// bounded queues into them.
    fn spawn_writers(&self) -> (Arc<TrackLocalStaticSample>, Arc<TrackLocalStaticSample>) {
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                ..Default::default()
            },
            "video".to_owned(),
            "glitch9-video".to_owned(),
        ));
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                ..Default::default()
            },
            "audio".to_owned(),
            "glitch9-audio".to_owned(),
        ));

        // Video writer: drain the bounded queue, drop stale frames, write samples.
        if let Some(mut rx) = self.video_rx.lock().take() {
            let track = video_track.clone();
            let fps = self.fps.max(1);
            let bytes_sent = self.bytes_sent.clone();
            let dropped = self.dropped.clone();
            tokio::spawn(async move {
                let frame_dur = Duration::from_secs(1) / fps;
                while let Some(frame) = rx.recv().await {
                    // Coalesce: if more frames are already queued, keep only the most
                    // recent (prefer a keyframe) so a slow viewer can't add latency.
                    let mut latest = frame;
                    while let Ok(next) = rx.try_recv() {
                        if latest.is_key() && !next.is_key() {
                            // keep the keyframe we already have; drop the delta
                            dropped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        dropped.fetch_add(1, Ordering::Relaxed);
                        latest = next;
                    }
                    let sample = Sample {
                        data: latest.data.clone(),
                        duration: frame_dur,
                        ..Default::default()
                    };
                    if track.write_sample(&sample).await.is_ok() {
                        bytes_sent.fetch_add(latest.data.len() as u64, Ordering::Relaxed);
                    }
                }
            });
        }

        // Audio writer.
        if let Some(mut rx) = self.audio_rx.lock().take() {
            let track = audio_track.clone();
            tokio::spawn(async move {
                while let Some(pkt) = rx.recv().await {
                    let sample = Sample {
                        data: pkt.data.clone(),
                        duration: Duration::from_millis(20),
                        ..Default::default()
                    };
                    let _ = track.write_sample(&sample).await;
                }
            });
        }

        (video_track, audio_track)
    }
}

#[async_trait]
impl MediaTransport for WebRtcTransport {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self) -> Result<()> {
        self.state
            .store(TransportState::Connecting as u8, Ordering::Relaxed);

        let api = Arc::new(Self::build_api()?);
        let (video_track, audio_track) = self.spawn_writers();

        // Shared config for every viewer PeerConnection (STUN for ICE; no TURN POC).
        let rtc_config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let server = crate::signaling::SignalingServer {
            bind_addr: self.bind_addr.clone(),
            port: self.port,
            api,
            rtc_config,
            video_track: video_track as Arc<dyn TrackLocal + Send + Sync>,
            audio_track: audio_track as Arc<dyn TrackLocal + Send + Sync>,
            viewers: self.viewers.clone(),
            state: self.state.clone(),
            on_viewer_join: self.on_viewer_join.clone(),
        };

        tokio::spawn(async move {
            if let Err(e) = server.run().await {
                tracing::error!(target: "g9::webrtc", "signaling server stopped: {e}");
            }
        });

        tracing::info!(target: "g9::webrtc", "WebRTC viewer+signaling on {}:{}", self.bind_addr, self.port);
        Ok(())
    }

    fn send_video(&self, frame: SharedEncodedFrame) {
        match self.video_tx.try_send(frame) {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {}
        }
    }

    fn send_audio(&self, packet: AudioPacket) {
        let _ = self.audio_tx.try_send(packet);
    }

    fn state(&self) -> TransportState {
        match self.state.load(Ordering::Relaxed) {
            1 => TransportState::Connecting,
            2 => TransportState::Connected,
            3 => TransportState::Reconnecting,
            4 => TransportState::Failed,
            5 => TransportState::Stopped,
            _ => TransportState::Idle,
        }
    }

    fn stats(&self) -> TransportStats {
        TransportStats {
            state: Some(self.state()),
            viewers: Some(self.viewers.load(Ordering::Relaxed)),
            bytes_sent: Some(self.bytes_sent.load(Ordering::Relaxed)),
            dropped_frames: Some(self.dropped.load(Ordering::Relaxed)),
            ..Default::default()
        }
    }

    async fn stop(&self) {
        self.state
            .store(TransportState::Stopped as u8, Ordering::Relaxed);
    }
}
