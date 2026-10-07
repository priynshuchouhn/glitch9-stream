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
    pub async fn connect(whep_url: &str) -> anyhow::Result<(Self, mpsc::Receiver<FacecamSample>)> {
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

        // H.264 SPS/PPS (Annex-B) parsed from the answer SDP's sprop-parameter-sets.
        // We subscribe mid-stream and only receive delta slices, so the decoder
        // never sees parameter sets from the wire; we prepend these to the first
        // access units so the decoder can initialize without waiting for an IDR.
        let sps_pps: Arc<Mutex<Option<Bytes>>> = Arc::new(Mutex::new(None));

        // Pump each inbound track: depacketize H.264 (video) / forward Opus (audio).
        let tx_on_track = tx.clone();
        let closed_on_track = closed.clone();
        let pc_on_track = Arc::downgrade(&pc);
        let sps_pps_on_track = sps_pps.clone();
        pc.on_track(Box::new(move |track, _receiver, _transceiver| {
            let tx = tx_on_track.clone();
            let closed = closed_on_track.clone();
            let pc_weak = pc_on_track.clone();
            let sps_pps = sps_pps_on_track.clone();
            Box::pin(async move {
                let kind = track.kind();
                // Inspect the negotiated codec so we depacketize/decode correctly:
                // the publishing browser may send H.264 or VP8 for video.
                let mime = track.codec().capability.mime_type.to_lowercase();
                tracing::info!(target: "g9::whep-sub", "facecam track: kind={kind:?} codec={mime}");
                if kind == RTPCodecType::Video {
                    // Request a keyframe (PLI) so the publisher immediately sends an
                    // IDR with SPS/PPS. We subscribe mid-stream, so without this the
                    // decoder only ever sees delta slices (no parameter sets) and can
                    // never initialize. Repeat until the pump sees a keyframe.
                    spawn_keyframe_requester(pc_weak, track.ssrc(), closed.clone());
                }
                tokio::spawn(async move {
                    if kind == RTPCodecType::Video {
                        if mime.contains("vp8") {
                            pump_vp8(track, tx, closed).await;
                        } else {
                            // Default to H.264 for video/H264 (and anything else we
                            // don't explicitly branch), matching prior behavior.
                            pump_h264(track, tx, closed, sps_pps).await;
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
        // Extract H.264 SPS/PPS from the answer's sprop-parameter-sets so the pump
        // can seed the decoder before any keyframe arrives from the wire.
        if let Some(annex_b) = sps_pps_from_sdp(&answer_sdp) {
            tracing::info!(
                target: "g9::whep-sub",
                "facecam h264 sprop-parameter-sets found ({} bytes Annex-B)",
                annex_b.len()
            );
            *sps_pps.lock() = Some(annex_b);
        } else {
            tracing::info!(target: "g9::whep-sub", "facecam h264: no sprop-parameter-sets in answer SDP");
        }
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

/// Annex-B start code (4-byte) prefixed before each parameter-set / slice NAL.
const ANNEX_B_START: [u8; 4] = [0, 0, 0, 1];

/// True if an Annex-B access unit already contains an SPS (NAL type 7) or PPS
/// (type 8), so we don't redundantly prepend parameter sets.
fn au_has_param_sets(au: &[u8]) -> bool {
    au_has_nal_type(au, 7) || au_has_nal_type(au, 8)
}

fn au_has_nal_type(au: &[u8], wanted: u8) -> bool {
    let mut i = 0;
    while i + 4 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
            let t = au[i + 4] & 0x1f;
            if t == wanted {
                return true;
            }
            i += 4;
        } else if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let t = au[i + 3] & 0x1f;
            if t == wanted {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Parse the H.264 `sprop-parameter-sets` (base64 SPS,PPS) from an SDP and return
/// the parameter sets as Annex-B (each NAL prefixed with a 00 00 00 01 start code).
/// Returns None when the SDP has no H.264 fmtp with sprop-parameter-sets.
fn sps_pps_from_sdp(sdp: &str) -> Option<Bytes> {
    // Find the fmtp line carrying sprop-parameter-sets=<b64-sps>,<b64-pps>.
    let line = sdp.lines().find(|l| l.contains("sprop-parameter-sets="))?;
    let after = line.split("sprop-parameter-sets=").nth(1)?;
    // The value runs until ';' (next fmtp param) or end of line.
    let value = after.split([';', ' ']).next()?.trim();
    if value.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for b64 in value.split(',') {
        if b64.is_empty() {
            continue;
        }
        let nal = base64_decode(b64)?;
        if nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&ANNEX_B_START);
        out.extend_from_slice(&nal);
    }
    if out.is_empty() {
        None
    } else {
        Some(Bytes::from(out))
    }
}

/// Minimal standard-alphabet base64 decoder (SPS/PPS are tiny). Ignores padding
/// and whitespace; returns None on an invalid character.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        if c == b'=' || c == b'\r' || c == b'\n' || c == b' ' {
            continue;
        }
        let v = val(c)?;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Periodically send a Picture Loss Indication (PLI) to the publisher so it emits
/// a keyframe (IDR with SPS/PPS). Because the engine subscribes mid-stream, the
/// first media it sees is delta slices with no parameter sets; the decoder cannot
/// initialize until a keyframe arrives. We PLI on connect and repeat for a short
/// window so a keyframe is produced promptly.
fn spawn_keyframe_requester(
    pc: std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>,
    ssrc: u32,
    closed: Arc<AtomicBool>,
) {
    use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
    tokio::spawn(async move {
        // ~1s cadence, bounded window; a keyframe normally arrives within a second
        // or two. Keeps requesting in case early PLIs are lost.
        for _ in 0..15 {
            if closed.load(Ordering::SeqCst) {
                break;
            }
            let Some(pc) = pc.upgrade() else { break };
            let pli = PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: ssrc,
            };
            match pc.write_rtcp(&[Box::new(pli)]).await {
                Ok(n) => {
                    tracing::info!(target: "g9::whep-sub", "facecam PLI sent (ssrc={ssrc}, {n} bytes)")
                }
                Err(e) => tracing::info!(target: "g9::whep-sub", "facecam PLI write failed: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }
    });
}

/// Depacketize an inbound H.264 track into Annex-B access units. `sps_pps` holds
/// the Annex-B SPS/PPS parsed from the SDP; it is prepended to access units that
/// don't already carry parameter sets, so the decoder can initialize mid-stream.
async fn pump_h264(
    track: Arc<TrackRemote>,
    tx: mpsc::Sender<FacecamSample>,
    closed: Arc<AtomicBool>,
    sps_pps: Arc<Mutex<Option<Bytes>>>,
) {
    use webrtc::rtp::codecs::h264::H264Packet;
    use webrtc::rtp::packetizer::Depacketizer;

    let mut depacketizer = H264Packet::default();
    // H264Packet depacketizes one RTP payload, not a complete video frame. A
    // browser frame commonly spans many FU-A packets; assemble all NAL parts up
    // to the RTP marker bit before handing one access unit to Media Foundation.
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut frame_timestamp: Option<u32> = None;
    let mut rtp_count: u64 = 0;
    let mut au_count: u64 = 0;
    while !closed.load(Ordering::SeqCst) {
        let (packet, _) = match track.read_rtp().await {
            Ok(v) => v,
            Err(_) => break,
        };
        if packet.payload.is_empty() {
            continue;
        }
        if frame_timestamp.is_some_and(|timestamp| timestamp != packet.header.timestamp) {
            // The prior frame lost its marker/packet. Never feed a partial frame
            // to the decoder; restart assembly at the new RTP timestamp.
            frame_buf.clear();
        }
        frame_timestamp = Some(packet.header.timestamp);
        rtp_count += 1;
        match depacketizer.depacketize(&packet.payload) {
            Ok(part) if !part.is_empty() => {
                frame_buf.extend_from_slice(&part);
                if !packet.header.marker {
                    continue;
                }
                au_count += 1;
                let complete_au = std::mem::take(&mut frame_buf);
                frame_timestamp = None;
                // If SDP supplied parameter sets, prepend them to IDRs that do not
                // already contain them. Do not prepend them to every delta frame.
                let params = sps_pps.lock().clone();
                let au = match params {
                    Some(pp)
                        if au_has_nal_type(&complete_au, 5) && !au_has_param_sets(&complete_au) =>
                    {
                        let mut combined = Vec::with_capacity(pp.len() + complete_au.len());
                        combined.extend_from_slice(&pp);
                        combined.extend_from_slice(&complete_au);
                        Bytes::from(combined)
                    }
                    _ => Bytes::from(complete_au),
                };
                if au_count <= 5 {
                    tracing::info!(target: "g9::whep-sub", "facecam h264 pump: rtp={rtp_count} au#{au_count} ({} bytes)", au.len());
                }
                if tx
                    .send(FacecamSample::Video(FacecamVideoCodec::H264, au))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(_) => {
                // Depacketizer consumed the packet but has no complete AU yet
                // (mid-fragment). Log periodically so a never-assembling stream is
                // visible instead of silently producing no video.
                if rtp_count <= 300 && rtp_count % 100 == 0 {
                    tracing::info!(target: "g9::whep-sub", "facecam h264 pump: {rtp_count} rtp packets, still no complete AU");
                }
            }
            Err(e) => {
                frame_buf.clear();
                frame_timestamp = None;
                if rtp_count <= 300 && rtp_count % 100 == 0 {
                    tracing::info!(target: "g9::whep-sub", "facecam h264 pump: depacketize err after {rtp_count} rtp: {e}");
                }
            }
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
        anyhow::bail!(
            "WHEP server returned: {status_line} — body: {}",
            body.trim()
        );
    }
    // MediaMTX returns the WHEP SDP answer with Transfer-Encoding: chunked. The raw
    // body then starts with a hex chunk-size line (e.g. "8a1\r\n...") which is NOT
    // valid SDP and makes the parser fail with `SdpInvalidSyntax: <chunk-size>`.
    // De-chunk when the header advertises chunked encoding before parsing the SDP.
    let is_chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let sdp = if is_chunked {
        dechunk_body(body)
    } else {
        body.to_string()
    };
    // Resolve the resource URL from the Location header (relative or absolute).
    let location = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("Location:")
                .or_else(|| l.strip_prefix("location:"))
        })
        .map(|v| v.trim().to_string())
        .map(|loc| resolve_location(url, &loc));
    Ok((sdp, location))
}

/// Decode an HTTP/1.1 chunked-transfer-encoded body into its payload. Each chunk is
/// `<hex-size>\r\n<data>\r\n`, terminated by a zero-size chunk. Returns the
/// concatenated chunk data (the WHEP SDP answer).
fn dechunk_body(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Read the chunk-size line up to CRLF.
        let line_end = match find_crlf(&bytes[i..]) {
            Some(p) => i + p,
            None => break,
        };
        let size_str = std::str::from_utf8(&bytes[i..line_end])
            .unwrap_or("")
            .trim();
        // Chunk size may carry extensions after ';'; take the hex part only.
        let hex = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(hex, 16).unwrap_or(0);
        if size == 0 {
            break; // final chunk
        }
        let data_start = line_end + 2; // skip CRLF after the size line
        let data_end = (data_start + size).min(bytes.len());
        out.extend_from_slice(&bytes[data_start..data_end]);
        // Advance past the chunk data and its trailing CRLF.
        i = data_end + 2;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Index of the first CRLF in `bytes`, if present.
fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|w| w == b"\r\n")
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
