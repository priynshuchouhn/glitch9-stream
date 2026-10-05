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
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// The browser viewer, embedded at compile time (served over plain HTTP GET).
const VIEWER_HTML: &str = include_str!("../../../web/index.html");

/// Write a minimal HTTP/1.1 response carrying the viewer page.
async fn serve_viewer_page(stream: &mut tokio::net::TcpStream) -> std::io::Result<()> {
    let body = VIEWER_HTML.as_bytes();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
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
}

impl SignalingServer {
    pub async fn run(self) -> anyhow::Result<()> {
        let addr = format!("{}:{}", self.bind_addr, self.port);
        let listener = TcpListener::bind(&addr).await?;
        self.state.store(2 /*Connected=listening*/, Ordering::Relaxed);
        let srv = Arc::new(self);

        loop {
            let (mut stream, peer) = listener.accept().await?;
            let srv = srv.clone();
            tokio::spawn(async move {
                // Peek the request head to route: an Upgrade: websocket request on /ws
                // becomes a viewer; a plain GET / returns the viewer HTML page.
                let mut peek = [0u8; 1024];
                let n = match stream.peek(&mut peek).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let head = String::from_utf8_lossy(&peek[..n]);
                let is_ws = head.to_ascii_lowercase().contains("upgrade: websocket");

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
                    let _ = serve_viewer_page(&mut stream).await;
                }
            });
        }
    }

    /// Handle one browser viewer over its WebSocket.
    async fn handle_viewer(
        &self,
        ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> anyhow::Result<()> {
        let pc = Arc::new(self.api.new_peer_connection(self.rtc_config.clone()).await?);

        // Add the shared tracks (view-only: we only send).
        pc.add_track(self.video_track.clone()).await?;
        pc.add_track(self.audio_track.clone()).await?;

        self.viewers.fetch_add(1, Ordering::Relaxed);
        // Ask the engine to force an IDR so this viewer decodes immediately.
        if let Some(cb) = self.on_viewer_join.lock().as_ref() {
            cb();
        }

        let (mut ws_tx, mut ws_rx) = ws.split();

        // Relay our locally-gathered ICE candidates to the browser.
        let (cand_tx, mut cand_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        pc.on_ice_candidate(Box::new(move |c| {
            let cand_tx = cand_tx.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    if let Ok(j) = c.to_json() {
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
                            let offer = RTCSessionDescription::offer(sdp)?;
                            pc.set_remote_description(offer).await?;
                            let answer = pc.create_answer(None).await?;
                            pc.set_local_description(answer.clone()).await?;
                            let txt = serde_json::to_string(&SignalMessage::Answer { sdp: answer.sdp })?;
                            ws_tx.send(Message::Text(txt)).await?;
                        }
                        SignalMessage::Candidate { candidate, sdp_mid, sdp_mline_index } => {
                            use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
                            let init = RTCIceCandidateInit {
                                candidate,
                                sdp_mid,
                                sdp_mline_index,
                                username_fragment: None,
                            };
                            pc.add_ice_candidate(init).await.ok();
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
