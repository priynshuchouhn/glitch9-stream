//! WHIP publish transport: the engine encodes ONCE and publishes its H.264+Opus
//! stream to an SFU (MediaMTX) via WHIP (WebRTC-HTTP Ingestion Protocol, RFC 9725).
//! The SFU fans the stream out to N spectators via WHEP — so the GPU does one encode
//! regardless of viewer count, and the SFU (with its public IP) handles NAT traversal
//! for arbitrary viewers. This replaces the direct engine→browser path for production.
//!
//! WHIP handshake is dead simple: HTTP POST the SDP offer (Content-Type
//! application/sdp, Bearer token) to the WHIP URL; the server replies 201 Created
//! with the SDP answer in the body and a Location header for the session resource.
//! On stop we DELETE that resource. We do the HTTP by hand over TCP (no new deps);
//! the SFU endpoint is plain http:// on the LAN/public net.

use async_trait::async_trait;
use g9_core::frame::SharedEncodedFrame;
use g9_core::transport::{AudioPacket, MediaTransport, TransportState, TransportStats};
use g9_core::Result;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

const VIDEO_QUEUE_DEPTH: usize = 8;
const AUDIO_QUEUE_DEPTH: usize = 32;

/// Publishes the encoded stream to an SFU via WHIP.
pub struct WhipTransport {
    name: String,
    /// Full WHIP endpoint, e.g. http://46.232.234.68:8889/session-<id>/whip
    whip_url: String,
    /// Bearer token for publish auth (SFU publisher credential).
    token: String,
    /// Public IP the engine advertises in its ICE candidates so the SFU can reach it.
    public_ip: Option<String>,
    fps: u32,
    state: Arc<AtomicU8>,
    bytes_sent: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    video_tx: mpsc::Sender<SharedEncodedFrame>,
    video_rx: Mutex<Option<mpsc::Receiver<SharedEncodedFrame>>>,
    audio_tx: mpsc::Sender<AudioPacket>,
    audio_rx: Mutex<Option<mpsc::Receiver<AudioPacket>>>,
}

impl WhipTransport {
    pub fn new(
        whip_url: impl Into<String>,
        token: impl Into<String>,
        public_ip: Option<String>,
        fps: u32,
    ) -> Self {
        let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE_DEPTH);
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE_DEPTH);
        Self {
            name: "whip".into(),
            whip_url: whip_url.into(),
            token: token.into(),
            public_ip,
            fps,
            state: Arc::new(AtomicU8::new(TransportState::Idle as u8)),
            bytes_sent: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            video_tx,
            video_rx: Mutex::new(Some(video_rx)),
            audio_tx,
            audio_rx: Mutex::new(Some(audio_rx)),
        }
    }

    fn build_api(public_ip: &Option<String>) -> Result<webrtc::api::API> {
        let mut m = MediaEngine::default();
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=64002a"
                            .to_owned(),
                    rtcp_feedback: vec![
                        RTCPFeedback { typ: "nack".to_owned(), parameter: "".to_owned() },
                        RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
                        RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
                    ],
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

        // The engine is the ICE controlling side CONNECTING OUT to the SFU, so its
        // advertised candidates must be reachable by the SFU. Advertise the VM's
        // public IP via nat_1to1 (all interfaces) when known; otherwise default
        // candidate gathering (fine when the SFU is on the same LAN).
        let mut se = webrtc::api::setting_engine::SettingEngine::default();
        se.set_network_types(vec![
            webrtc::ice::network_type::NetworkType::Udp4,
            webrtc::ice::network_type::NetworkType::Udp6,
        ]);
        let ip = public_ip
            .clone()
            .or_else(|| std::env::var("G9_PUBLIC_IP").ok())
            .filter(|s| !s.trim().is_empty());
        if let Some(ip) = ip {
            se.set_interface_filter(Box::new(|_n: &str| true));
            se.set_nat_1to1_ips(
                ip.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType::Host,
            );
        }
        se.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);

        Ok(APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .with_setting_engine(se)
            .build())
    }

    fn make_tracks(&self) -> (Arc<TrackLocalStaticSample>, Arc<TrackLocalStaticSample>) {
        let video = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                ..Default::default()
            },
            "video".to_owned(),
            "glitch9-video".to_owned(),
        ));
        let audio = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                ..Default::default()
            },
            "audio".to_owned(),
            "glitch9-audio".to_owned(),
        ));
        (video, audio)
    }

    fn spawn_writers(
        &self,
        video_track: Arc<TrackLocalStaticSample>,
        audio_track: Arc<TrackLocalStaticSample>,
    ) {
        if let Some(mut rx) = self.video_rx.lock().take() {
            let track = video_track;
            let fps = self.fps.max(1);
            let bytes_sent = self.bytes_sent.clone();
            let dropped = self.dropped.clone();
            tokio::spawn(async move {
                let frame_dur = Duration::from_secs(1) / fps;
                while let Some(frame) = rx.recv().await {
                    let mut latest = frame;
                    while let Ok(next) = rx.try_recv() {
                        if latest.is_key() && !next.is_key() {
                            dropped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        dropped.fetch_add(1, Ordering::Relaxed);
                        latest = next;
                    }
                    let sample = Sample { data: latest.data.clone(), duration: frame_dur, ..Default::default() };
                    if track.write_sample(&sample).await.is_ok() {
                        bytes_sent.fetch_add(latest.data.len() as u64, Ordering::Relaxed);
                    }
                }
            });
        }
        if let Some(mut rx) = self.audio_rx.lock().take() {
            let track = audio_track;
            tokio::spawn(async move {
                while let Some(pkt) = rx.recv().await {
                    let sample = Sample { data: pkt.data.clone(), duration: Duration::from_millis(20), ..Default::default() };
                    let _ = track.write_sample(&sample).await;
                }
            });
        }
    }
}

#[async_trait]
impl MediaTransport for WhipTransport {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self) -> Result<()> {
        self.state.store(TransportState::Connecting as u8, Ordering::Relaxed);
        let api = Self::build_api(&self.public_ip)?;
        let (video_track, audio_track) = self.make_tracks();

        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .map_err(|e| g9_core::Error::transport(format!("new_peer_connection: {e}")))?,
        );
        // Sendonly: we publish, never receive.
        use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
        let _vsender: Arc<RTCRtpSender> = pc
            .add_track(video_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|e| g9_core::Error::transport(format!("add video track: {e}")))?;
        let _asender = pc
            .add_track(audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|e| g9_core::Error::transport(format!("add audio track: {e}")))?;
        for t in pc.get_transceivers().await {
            let _ = t.set_direction(RTCRtpTransceiverDirection::Sendonly).await;
        }

        // Offer, then gather all candidates (non-trickle: WHIP sends one complete SDP).
        let offer = pc
            .create_offer(None)
            .await
            .map_err(|e| g9_core::Error::transport(format!("create_offer: {e}")))?;
        let mut gather = pc.gathering_complete_promise().await;
        pc.set_local_description(offer)
            .await
            .map_err(|e| g9_core::Error::transport(format!("set_local_description: {e}")))?;
        let _ = gather.recv().await;
        let local = pc
            .local_description()
            .await
            .ok_or_else(|| g9_core::Error::transport("no local description"))?;

        // WHIP POST the offer SDP; get the answer back.
        let answer_sdp = whip_post(&self.whip_url, &self.token, &local.sdp)
            .await
            .map_err(|e| g9_core::Error::transport(format!("WHIP POST: {e}")))?;
        let answer = RTCSessionDescription::answer(answer_sdp)
            .map_err(|e| g9_core::Error::transport(format!("parse answer: {e}")))?;
        pc.set_remote_description(answer)
            .await
            .map_err(|e| g9_core::Error::transport(format!("set_remote_description: {e}")))?;

        // Log state transitions; mark Connected when the publish link is up.
        let state = self.state.clone();
        pc.on_peer_connection_state_change(Box::new(move |s| {
            use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as St;
            tracing::info!(target: "g9::whip", "publish peer state: {s}");
            let state = state.clone();
            Box::pin(async move {
                let v = match s {
                    St::Connected => TransportState::Connected as u8,
                    St::Failed => TransportState::Failed as u8,
                    St::Disconnected => TransportState::Reconnecting as u8,
                    _ => TransportState::Connecting as u8,
                };
                state.store(v, Ordering::Relaxed);
            })
        }));

        self.spawn_writers(video_track, audio_track);
        // Keep the PeerConnection alive for the process lifetime.
        std::mem::forget(pc);
        tracing::info!(target: "g9::whip", "publishing to SFU via WHIP: {}", self.whip_url);
        Ok(())
    }

    fn send_video(&self, frame: SharedEncodedFrame) {
        match self.video_tx.try_send(frame) {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Full(_)) => { self.dropped.fetch_add(1, Ordering::Relaxed); }
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
            bytes_sent: Some(self.bytes_sent.load(Ordering::Relaxed)),
            dropped_frames: Some(self.dropped.load(Ordering::Relaxed)),
            ..Default::default()
        }
    }

    async fn stop(&self) {
        self.state.store(TransportState::Stopped as u8, Ordering::Relaxed);
    }
}

/// Minimal WHIP POST over raw TCP (no HTTP-client dependency). Sends the SDP offer
/// with Bearer auth; returns the SDP answer from the 201 response body. Plain http
/// only (the SFU endpoint is http:// on the LAN/public net).
async fn whip_post(url: &str, token: &str, sdp_offer: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let (host, port, path) = parse_http_url(url)?;
    let mut stream = TcpStream::connect((host.as_str(), port)).await?;
    let auth = if token.is_empty() {
        String::new()
    } else {
        format!("Authorization: Bearer {token}\r\n")
    };
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/sdp\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{sdp_offer}",
        sdp_offer.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed WHIP response"))?;
    let status_line = head.lines().next().unwrap_or("");
    // WHIP success is 201 Created (some servers use 200).
    if !(status_line.contains(" 201") || status_line.contains(" 200")) {
        anyhow::bail!("WHIP server returned: {status_line} — body: {}", body.trim());
    }
    Ok(body.to_string())
}

/// Parse `http://host:port/path` into (host, port, path). http only.
fn parse_http_url(url: &str) -> anyhow::Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("WHIP url must be http:// (got {url})"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(80)),
        None => (authority.to_string(), 80u16),
    };
    Ok((host, port, path.to_string()))
}
