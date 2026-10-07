//! WHEP (WebRTC-HTTP Egress Protocol) SUBSCRIBER — the engine-side counterpart to
//! the browser's WHIP publisher. Used to pull the player's facecam (camera H.264 +
//! microphone Opus) that the browser published to the SFU, so the engine can
//! composite the camera over the game and mix the mic into the broadcast audio.
//!
//! Receive-only: we POST a recvonly SDP offer to the WHEP URL (plain HTTP over TCP,
//! like `whip.rs`), take the SDP answer, then collect inbound RTP. Each depacketized
//! H.264 access unit is forwarded as Annex-B bytes; each Opus frame as raw payload.
//! Decoding (H.264 → texture) and audio mixing happen downstream in the engine.

use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_remote::TrackRemote;

/// Which video codec a facecam video sample carries. Browsers that cannot send
/// WebRTC H.264 (Brave, Firefox without OpenH264) publish VP8, so the engine must
/// handle both and tell the compositor which decoder to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacecamVideoCodec {
    H264,
    Vp8,
}

/// A decoded-but-still-compressed media sample pulled from the facecam.
pub enum FacecamSample {
    /// One video access unit: H.264 in Annex-B (start-code prefixed) form, or a
    /// raw VP8 coded frame, tagged by `FacecamVideoCodec`.
    Video(FacecamVideoCodec, Bytes),
    /// One Opus packet (raw payload, 48 kHz).
    Audio(Bytes),
}

/// Handle to a live facecam subscription. Dropping or calling `close()` tears the
/// WHEP resource down and ends the sample stream.
pub struct WhepSubscriber {
    closed: Arc<AtomicBool>,
    resource_url: Arc<Mutex<Option<String>>>,
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
}

impl WhepSubscriber {
    /// Connect to `whep_url` and start receiving. Samples are delivered on the
    /// returned channel; the channel closes when the subscription ends.
    pub async fn connect(
        whep_url: &str,
    ) -> anyhow::Result<(Self, mpsc::Receiver<FacecamSample>)> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()
            .map_err(|e| anyhow::anyhow!("register codecs: {e}"))?;
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)
            .map_err(|e| anyhow::anyhow!("interceptors: {e}"))?;
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .map_err(|e| anyhow::anyhow!("new_peer_connection: {e}"))?,
        );

        // Receive-only: one video + one audio transceiver.
        pc.add_transceiver_from_kind(RTPCodecType::Video, None)
            .await
            .map_err(|e| anyhow::anyhow!("add video transceiver: {e}"))?;
        pc.add_transceiver_from_kind(RTPCodecType::Audio, None)
            .await
            .map_err(|e| anyhow::anyhow!("add audio transceiver: {e}"))?;
        for t in pc.get_transceivers().await {
            let _ = t.set_direction(RTCRtpTransceiverDirection::Recvonly).await;
        }

        let (tx, rx) = mpsc::channel::<FacecamSample>(256);
        let closed = Arc::new(AtomicBool::new(false));

        // Pump each inbound track: depacketize H.264 (video) / forward Opus (audio).
        let tx_on_track = tx.clone();
        let closed_on_track = closed.clone();
        pc.on_track(Box::new(move |track, _receiver, _transceiver| {
            let tx = tx_on_track.clone();
            let closed = closed_on_track.clone();
            Box::pin(async move {
                let kind = track.kind();
                // Inspect the negotiated codec so we depacketize/decode correctly:
                // the publishing browser may send H.264 or VP8 for video.
                let mime = track.codec().capability.mime_type.to_lowercase();
                tracing::info!(target: "g9::whep-sub", "facecam track: kind={kind:?} codec={mime}");
                tokio::spawn(async move {
                    if kind == RTPCodecType::Video {
                        if mime.contains("vp8") {
                            pump_vp8(track, tx, closed).await;
                        } else {
                            // Default to H.264 for video/H264 (and anything else we
                            // don't explicitly branch), matching prior behavior.
                            pump_h264(track, tx, closed).await;
                        }
                    } else if kind == RTPCodecType::Audio {
                        pump_opus(track, tx, closed).await;
                    }
                });
            })
        }));

        // Offer, gather ICE (non-trickle), POST to WHEP, apply the answer.
        let offer = pc
            .create_offer(None)
            .await
            .map_err(|e| anyhow::anyhow!("create_offer: {e}"))?;
        let mut gather = pc.gathering_complete_promise().await;
        pc.set_local_description(offer)
            .await
            .map_err(|e| anyhow::anyhow!("set_local_description: {e}"))?;
        let _ = gather.recv().await;
        let local = pc
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("no local description"))?;

        let (answer_sdp, resource) = whep_post(whep_url, &local.sdp).await?;
        let answer = RTCSessionDescription::answer(answer_sdp)
            .map_err(|e| anyhow::anyhow!("parse answer: {e}"))?;
        pc.set_remote_description(answer)
            .await
            .map_err(|e| anyhow::anyhow!("set_remote_description: {e}"))?;

        tracing::info!(target: "g9::whep-sub", "facecam subscribed: {}", whep_url);
        Ok((
            Self {
                closed,
                resource_url: Arc::new(Mutex::new(resource)),
                pc,
            },
            rx,
        ))
    }

    /// Stop receiving and release the WHEP resource (best-effort).
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // Copy the URL out and drop the guard BEFORE awaiting — holding a
        // parking_lot guard across .await makes the future non-Send (tokio::spawn
        // requires Send).
        let url = self.resource_url.lock().clone();
        if let Some(url) = url {
            let _ = whep_delete(&url).await;
        }
        let _ = self.pc.close().await;
    }
}

/// Depacketize an inbound H.264 track into Annex-B access units.
async fn pump_h264(
    track: Arc<TrackRemote>,
    tx: mpsc::Sender<FacecamSample>,
    closed: Arc<AtomicBool>,
) {
    use webrtc::rtp::codecs::h264::H264Packet;
    use webrtc::rtp::packetizer::Depacketizer;

    let mut depacketizer = H264Packet::default();
    while !closed.load(Ordering::SeqCst) {
        let (packet, _) = match track.read_rtp().await {
            Ok(v) => v,
            Err(_) => break,
        };
        if packet.payload.is_empty() {
            continue;
        }
        match depacketizer.depacketize(&packet.payload) {
            Ok(au) if !au.is_empty() => {
                if tx
                    .send(FacecamSample::Video(FacecamVideoCodec::H264, au))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            _ => {}
        }
    }
}

/// Depacketize an inbound VP8 track into raw VP8 coded frames.
async fn pump_vp8(
    track: Arc<TrackRemote>,
    tx: mpsc::Sender<FacecamSample>,
    closed: Arc<AtomicBool>,
) {
    use webrtc::rtp::codecs::vp8::Vp8Packet;
    use webrtc::rtp::packetizer::Depacketizer;

    // The RTP VP8 depacketizer strips the per-packet VP8 payload descriptor but
    // does NOT reassemble a frame that spans multiple RTP packets. libvpx needs a
    // COMPLETE coded frame, so we accumulate depacketized payloads until the RTP
    // marker bit (last packet of a frame), then emit the assembled frame.
    let mut depacketizer = Vp8Packet::default();
    let mut frame_buf: Vec<u8> = Vec::new();
    while !closed.load(Ordering::SeqCst) {
        let (packet, _) = match track.read_rtp().await {
            Ok(v) => v,
            Err(_) => break,
        };
        if packet.payload.is_empty() {
            continue;
        }
        match depacketizer.depacketize(&packet.payload) {
            Ok(part) => frame_buf.extend_from_slice(&part),
            Err(_) => {
                // Corrupt packet: drop the partial frame to avoid feeding libvpx a
                // misassembled bitstream; the next keyframe recovers decoding.
                frame_buf.clear();
                continue;
            }
        }
        // Marker bit set => last packet of this frame; emit the complete frame.
        if packet.header.marker && !frame_buf.is_empty() {
            let frame = Bytes::from(std::mem::take(&mut frame_buf));
            if tx
                .send(FacecamSample::Video(FacecamVideoCodec::Vp8, frame))
                .await
                .is_err()
            {
                break;
            }
        }
    }
}

/// Forward inbound Opus RTP payloads (already one frame per packet).
async fn pump_opus(
    track: Arc<TrackRemote>,
    tx: mpsc::Sender<FacecamSample>,
    closed: Arc<AtomicBool>,
) {
    while !closed.load(Ordering::SeqCst) {
        let (packet, _) = match track.read_rtp().await {
            Ok(v) => v,
            Err(_) => break,
        };
        if packet.payload.is_empty() {
            continue;
        }
        if tx
            .send(FacecamSample::Audio(packet.payload.clone()))
            .await
            .is_err()
        {
            break;
        }
    }
}

/// POST the recvonly SDP offer to the WHEP URL; return (answer SDP, resource URL).
/// Plain HTTP over raw TCP (the SFU WHEP endpoint is reachable on the LAN/public
/// net); mirrors `whip.rs::whip_post`.
async fn whep_post(url: &str, sdp_offer: &str) -> anyhow::Result<(String, Option<String>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let (host, port, path) = parse_http_url(url)?;
    let mut stream = TcpStream::connect((host.as_str(), port)).await?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sdp_offer}",
        sdp_offer.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed WHEP response"))?;
    let status_line = head.lines().next().unwrap_or("");
    if !(status_line.contains(" 201") || status_line.contains(" 200")) {
        anyhow::bail!("WHEP server returned: {status_line} — body: {}", body.trim());
    }
    // Resolve the resource URL from the Location header (relative or absolute).
    let location = head
        .lines()
        .find_map(|l| l.strip_prefix("Location:").or_else(|| l.strip_prefix("location:")))
        .map(|v| v.trim().to_string())
        .map(|loc| resolve_location(url, &loc));
    Ok((body.to_string(), location))
}

/// DELETE the WHEP resource to release the SFU subscriber (best-effort).
async fn whep_delete(url: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    let (host, port, path) = parse_http_url(url)?;
    let mut stream = TcpStream::connect((host.as_str(), port)).await?;
    let req = format!("DELETE {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Join a base URL and a (possibly relative) Location into an absolute URL.
fn resolve_location(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    // Relative path: splice onto the base's scheme+authority.
    if let Some(rest) = base.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or(rest);
        let sep = if location.starts_with('/') { "" } else { "/" };
        return format!("http://{authority}{sep}{location}");
    }
    location.to_string()
}

/// Parse `http://host:port/path` into (host, port, path). http only.
fn parse_http_url(url: &str) -> anyhow::Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("WHEP url must be http:// (got {url})"))?;
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
