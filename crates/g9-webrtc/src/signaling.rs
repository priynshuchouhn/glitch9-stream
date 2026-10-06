//! Development-only local signaling + viewer server.
//!
//! One HTTP endpoint serves the viewer page (`/`), and a WebSocket endpoint
//! (`/ws`) carries SDP offer/answer + trickle ICE. Media never flows here. Binds
//! 127.0.0.1 by default. No auth — local POC only (not for deployment).
//!
//! Each WS connection = one viewer: we build an `RTCPeerConnection`, add the shared
//! video/audio tracks (sendonly), set the browser's offer, create our answer, and
//! relay ICE candidates both directions.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// The browser viewer, embedded at compile time (served over plain HTTP GET).
const VIEWER_HTML: &str = include_str!("../../../web/index.html");

/// Serve the viewer page over HTTP/1.1. We must DRAIN the request first: on Windows,
/// closing a socket that still has unread inbound data triggers a TCP RST, which
/// truncates our response and leaves the browser with a blank page. So we read the
/// request headers, write the response, flush, then shut down the write half cleanly.
async fn serve_viewer_page(mut stream: tokio::net::TcpStream) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Disable Nagle so the full response goes out promptly.
    let _ = stream.set_nodelay(true);

    // Drain the request headers (read until the blank line, bounded).
    let mut buf = [0u8; 2048];
    let mut total = 0usize;
    loop {
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            break;
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") || total == buf.len() {
            break;
        }
    }

    let body = VIEWER_HTML.as_bytes();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

    // Avoid the Windows "RST on close with unread data" that truncates the response:
    // half-close our write side (sends FIN), then read until the client closes its
    // side (EOF). Letting the client close first means a clean FIN both ways, no RST.
    let _ = stream.shutdown().await;
    let mut drain = [0u8; 1024];
    loop {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read(&mut drain),
        )
        .await
        {
            Ok(Ok(0)) | Err(_) => break, // client closed, or timed out
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break,
        }
    }
    Ok(())
}
use webrtc::api::API;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::track::track_local::TrackLocal;

/// Signaling messages (JSON over WS).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SignalMessage {
    Offer { sdp: String },
    Answer { sdp: String },
    Candidate {
        candidate: String,
        #[serde(rename = "sdpMid")]
        sdp_mid: Option<String>,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: Option<u16>,
    },
}

pub struct SignalingServer {
    pub bind_addr: String,
    pub port: u16,
    pub api: Arc<API>,
    pub rtc_config: RTCConfiguration,
    pub video_track: Arc<dyn TrackLocal + Send + Sync>,
    pub audio_track: Arc<dyn TrackLocal + Send + Sync>,
    pub viewers: Arc<AtomicU32>,
    pub state: Arc<AtomicU8>,
    pub on_viewer_join: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>,
    /// Adaptive bitrate: updated from RTCP Receiver Reports; read by the pipeline.
    pub target_bitrate: Arc<AtomicU32>,
    pub min_bitrate: u32,
    pub max_bitrate: u32,
}

impl SignalingServer {
    pub async fn run(self) -> anyhow::Result<()> {
        let addr = format!("{}:{}", self.bind_addr, self.port);
        let listener = TcpListener::bind(&addr).await?;
        self.state.store(2 /*Connected=listening*/, Ordering::Relaxed);
        let srv = Arc::new(self);

        loop {
            let (stream, peer) = listener.accept().await?;
            let srv = srv.clone();
            tokio::spawn(async move {
                // Peek the request head to route. `peek` can return before the full
                // header has arrived, so retry until we see the end of headers
                // (\r\n\r\n) or the Upgrade line, with a short bound.
                let mut peek = [0u8; 2048];
                let mut is_ws = false;
                for _ in 0..50 {
                    match stream.peek(&mut peek).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let head = String::from_utf8_lossy(&peek[..n]).to_ascii_lowercase();
                            is_ws = head.contains("upgrade: websocket");
                            if is_ws || head.contains("\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                tracing::debug!(target: "g9::webrtc", "conn {peer}: is_ws={is_ws}");

                if is_ws {
                    match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => {
                            if let Err(e) = srv.handle_viewer(ws).await {
                                tracing::debug!(target: "g9::webrtc", "viewer {peer} ended: {e}");
                            }
                        }
                        Err(e) => tracing::debug!(target: "g9::webrtc", "ws upgrade failed: {e}"),
                    }
                } else {
                    // Serve the embedded viewer page for any plain HTTP GET.
                    if let Err(e) = serve_viewer_page(stream).await {
                        tracing::debug!(target: "g9::webrtc", "serve viewer page: {e}");
                    }
                }
            });
        }
    }

    /// Handle one browser viewer over its WebSocket.
    async fn handle_viewer(
        &self,
        ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> anyhow::Result<()> {
        tracing::info!(target: "g9::webrtc", "viewer connected (WS up); creating PeerConnection");
        let pc = Arc::new(self.api.new_peer_connection(self.rtc_config.clone()).await?);

        // Add the shared tracks (view-only: we only send).
        let video_sender = pc.add_track(self.video_track.clone()).await?;
        pc.add_track(self.audio_track.clone()).await?;

        // Read RTCP from the video sender. The browser sends PLI/FIR when it needs a
        // keyframe (e.g. it just joined and has only P-frames, or it lost the IDR).
        // On either, force an IDR so the viewer gets a decodable frame promptly —
        // this is what un-blacks the screen. Without it the viewer waits for the next
        // periodic GOP keyframe (~4s) or stays black forever if timing is unlucky.
        {
            let force_kf = self.on_viewer_join.clone();
            let target_bitrate = self.target_bitrate.clone();
            let min_bitrate = self.min_bitrate;
            let max_bitrate = self.max_bitrate;
            tokio::spawn(async move {
                use webrtc::rtcp::payload_feedbacks::{
                    full_intra_request::FullIntraRequest,
                    picture_loss_indication::PictureLossIndication,
                };
                use webrtc::rtcp::receiver_report::ReceiverReport;

                // Loss-based adaptive bitrate controller (a simplified GCC loss
                // signal). Each Receiver Report carries fraction_lost (0-255 = 0-100%
                // of packets lost since the last report). Receiver Reports can arrive
                // several times a second, so reacting to every one makes the rate
                // oscillate wildly. Instead we smooth loss with an EWMA and apply at
                // most one adjustment per second:
                //   - smoothed loss > 10% -> multiplicative decrease (x0.85)
                //   - smoothed loss <  2% -> additive increase (+5% of ceiling)
                //   - 2-10%               -> hold
                // Clamped to [min_bitrate, max_bitrate]. Keeps the stream within what
                // the path can carry instead of a fixed CBR that floods a weak link.
                let mut ewma_loss: f64 = 0.0;
                let mut last_adjust = std::time::Instant::now();
                while let Ok((pkts, _attrs)) = video_sender.read_rtcp().await {
                    for p in &pkts {
                        if p.as_any().downcast_ref::<PictureLossIndication>().is_some()
                            || p.as_any().downcast_ref::<FullIntraRequest>().is_some()
                        {
                            tracing::info!(target: "g9::webrtc", "RTCP keyframe request (PLI/FIR) -> force IDR");
                            if let Some(cb) = force_kf.lock().as_ref() {
                                cb();
                            }
                        }
                        if let Some(rr) = p.as_any().downcast_ref::<ReceiverReport>() {
                            let frac = rr
                                .reports
                                .iter()
                                .map(|r| r.fraction_lost)
                                .max()
                                .unwrap_or(0);
                            let loss = frac as f64 / 256.0;
                            // EWMA (alpha=0.3) so a single spike doesn't whipsaw the rate.
                            ewma_loss = 0.3 * loss + 0.7 * ewma_loss;
                        }
                    }
                    // Apply at most once per second based on the smoothed loss.
                    if last_adjust.elapsed() >= std::time::Duration::from_secs(1) {
                        last_adjust = std::time::Instant::now();
                        let cur = target_bitrate.load(Ordering::Relaxed).max(min_bitrate);
                        let next = if ewma_loss > 0.10 {
                            ((cur as f64) * 0.85) as u32
                        } else if ewma_loss < 0.02 {
                            cur + (max_bitrate / 20)
                        } else {
                            cur
                        }
                        .clamp(min_bitrate, max_bitrate);
                        if next != cur {
                            tracing::info!(
                                target: "g9::webrtc",
                                "ABR: loss={:.1}% {} -> {} kbps",
                                ewma_loss * 100.0, cur / 1000, next / 1000
                            );
                            target_bitrate.store(next, Ordering::Relaxed);
                        }
                    }
                }
            });
        }

        // Log connection-state transitions so we can see where it stalls. When the
        // connection reaches `connected` (DTLS/SRTP ready), force a keyframe: the
        // join-time IDR fired at WS-connect would be encoded before SRTP is up and
        // therefore lost, leaving the viewer black until the next PLI/GOP. Forcing it
        // here guarantees the first decodable frame lands right after media can flow.
        let force_kf_on_connect = self.on_viewer_join.clone();
        pc.on_peer_connection_state_change(Box::new(move |s| {
            tracing::info!(target: "g9::webrtc", "peer connection state: {s}");
            let force_kf = force_kf_on_connect.clone();
            Box::pin(async move {
                if s == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected {
                    if let Some(cb) = force_kf.lock().as_ref() {
                        tracing::info!(target: "g9::webrtc", "peer connected -> force initial IDR");
                        cb();
                    }
                }
            })
        }));
        pc.on_ice_connection_state_change(Box::new(|s| {
            tracing::info!(target: "g9::webrtc", "ICE connection state: {s}");
            Box::pin(async {})
        }));
        pc.on_ice_gathering_state_change(Box::new(|s| {
            tracing::info!(target: "g9::webrtc", "ICE gathering state: {s}");
            Box::pin(async {})
        }));

        self.viewers.fetch_add(1, Ordering::Relaxed);
        // NOTE: the keyframe is forced on the `connected` state transition (above)
        // and on PLI/FIR, not here — forcing at WS-connect would encode the IDR
        // before SRTP is ready, so the viewer would never receive it.

        let (mut ws_tx, mut ws_rx) = ws.split();

        // Relay our locally-gathered ICE candidates to the browser.
        let (cand_tx, mut cand_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        pc.on_ice_candidate(Box::new(move |c| {
            let cand_tx = cand_tx.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    if let Ok(j) = c.to_json() {
                        tracing::info!(target: "g9::webrtc", "local ICE candidate: {}", j.candidate);
                        if let Ok(txt) = serde_json::to_string(&SignalMessage::Candidate {
                            candidate: j.candidate,
                            sdp_mid: j.sdp_mid,
                            sdp_mline_index: j.sdp_mline_index,
                        }) {
                            let _ = cand_tx.send(txt);
                        }
                    }
                }
            })
        }));

        // Task: forward queued ICE candidates to the websocket.
        let viewers = self.viewers.clone();
        loop {
            tokio::select! {
                maybe_cand = cand_rx.recv() => {
                    if let Some(txt) = maybe_cand {
                        ws_tx.send(Message::Text(txt)).await.ok();
                    }
                }
                maybe_msg = ws_rx.next() => {
                    let msg = match maybe_msg {
                        Some(Ok(Message::Text(t))) => t,
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(_)) => continue,
                        Some(Err(_)) => break,
                    };
                    let signal: SignalMessage = match serde_json::from_str(&msg) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    match signal {
                        SignalMessage::Offer { sdp } => {
                            tracing::info!(target: "g9::webrtc", "received SDP offer ({} bytes)", sdp.len());
                            let offer = match RTCSessionDescription::offer(sdp) {
                                Ok(o) => o,
                                Err(e) => { tracing::error!(target: "g9::webrtc", "bad offer: {e}"); continue; }
                            };
                            if let Err(e) = pc.set_remote_description(offer).await {
                                tracing::error!(target: "g9::webrtc", "set_remote_description: {e}"); continue;
                            }
                            let answer = match pc.create_answer(None).await {
                                Ok(a) => a,
                                Err(e) => { tracing::error!(target: "g9::webrtc", "create_answer: {e}"); continue; }
                            };
                            // Non-trickle: gather ALL candidates, then send the answer
                            // with them embedded in the SDP. This avoids trickle
                            // timing/relay races (the browser gets our 127.0.0.1 host
                            // candidate inline, guaranteed, before it starts checking).
                            let mut gather_done = pc.gathering_complete_promise().await;
                            if let Err(e) = pc.set_local_description(answer).await {
                                tracing::error!(target: "g9::webrtc", "set_local_description: {e}"); continue;
                            }
                            let _ = gather_done.recv().await;
                            let final_sdp = match pc.local_description().await {
                                Some(d) => d.sdp,
                                None => { tracing::error!(target: "g9::webrtc", "no local description after gather"); continue; }
                            };
                            let txt = serde_json::to_string(&SignalMessage::Answer { sdp: final_sdp })
                                .unwrap_or_default();
                            if ws_tx.send(Message::Text(txt)).await.is_ok() {
                                tracing::info!(target: "g9::webrtc", "sent SDP answer (non-trickle, candidates embedded)");
                            }
                        }
                        SignalMessage::Candidate { candidate, sdp_mid, sdp_mline_index } => {
                            tracing::info!(target: "g9::webrtc", "remote ICE candidate: {candidate}");
                            use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
                            let init = RTCIceCandidateInit {
                                candidate,
                                sdp_mid,
                                sdp_mline_index,
                                username_fragment: None,
                            };
                            if let Err(e) = pc.add_ice_candidate(init).await {
                                tracing::warn!(target: "g9::webrtc", "add_ice_candidate: {e}");
                            }
                        }
                        SignalMessage::Answer { .. } => { /* engine is the answerer */ }
                    }
                }
            }
        }

        viewers.fetch_sub(1, Ordering::Relaxed);
        pc.close().await.ok();
        Ok(())
    }
}
