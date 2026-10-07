//! RTMP/RTMPS transport implementing `MediaTransport`.
//!
//! Responsibilities:
//! - Connect to `rtmp(s)://host/app` + publish `stream_key` (key never logged).
//! - RTMP handshake → connect → createStream → publish.
//! - Mux H.264 (AVCC) + AAC into FLV tags → RTMP media messages.
//! - Reconnect with bounded exponential backoff; on reconnect, resend the AVC/AAC
//!   sequence headers and wait for the next keyframe before sending delta frames.
//! - Bounded internal queue; drop stale video on overflow so a slow uplink never
//!   stalls the encoder or the WebRTC transport.
//!
//! The RTMP chunk/AMF layer is implemented in the full Milestone 7 code; this file
//! defines the transport contract, the secret handling, the reconnect state machine
//! and the bounded queue so the engine can be wired and the rest compiled/tested.

use async_trait::async_trait;
use g9_core::config::{RtmpConfig, Secret};
use g9_core::frame::SharedEncodedFrame;
use g9_core::transport::{AudioPacket, MediaTransport, TransportState, TransportStats};
use g9_core::Result;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const VIDEO_QUEUE_DEPTH: usize = 16;
const AUDIO_QUEUE_DEPTH: usize = 64;
const BACKOFF_START_MS: u64 = 1000;
const BACKOFF_MAX_MS: u64 = 16_000;

enum Msg {
    Video(SharedEncodedFrame),
    Audio(AudioPacket),
}

pub struct RtmpTransport {
    name: String,
    cfg: RtmpConfig,
    tx: mpsc::Sender<Msg>,
    inner_rx: Mutex<Option<mpsc::Receiver<Msg>>>,
    state: Arc<AtomicU8>,
    bytes_sent: Arc<AtomicU64>,
    reconnects: Arc<AtomicU32>,
    dropped: Arc<AtomicU64>,
}

impl RtmpTransport {
    pub fn new(name: impl Into<String>, cfg: RtmpConfig) -> Self {
        let (tx, rx) = mpsc::channel(VIDEO_QUEUE_DEPTH + AUDIO_QUEUE_DEPTH);
        Self {
            name: name.into(),
            cfg,
            tx,
            inner_rx: Mutex::new(Some(rx)),
            state: Arc::new(AtomicU8::new(TransportState::Idle as u8)),
            bytes_sent: Arc::new(AtomicU64::new(0)),
            reconnects: Arc::new(AtomicU32::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Compute the next backoff delay (bounded exponential).
    pub fn next_backoff(prev: Duration) -> Duration {
        let next = (prev.as_millis() as u64).max(BACKOFF_START_MS) * 2;
        Duration::from_millis(next.min(BACKOFF_MAX_MS))
    }

    /// The connect target, SAFE to log (host/app only — never the key).
    fn log_target(&self) -> String {
        self.cfg.redacted_url()
    }
}

#[async_trait]
impl MediaTransport for RtmpTransport {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self) -> Result<()> {
        if self.cfg.stream_key.is_empty() {
            return Err(g9_core::Error::config("missing RTMP stream key"));
        }
        self.state
            .store(TransportState::Connecting as u8, Ordering::Relaxed);
        // Never log the key:
        tracing::info!(target: "g9::rtmp", "{} connecting to {}", self.name, self.log_target());

        let rx = self
            .inner_rx
            .lock()
            .take()
            .ok_or_else(|| g9_core::Error::transport("rtmp already started"))?;
        let state = self.state.clone();
        let bytes_sent = self.bytes_sent.clone();
        let reconnects = self.reconnects.clone();
        let dropped = self.dropped.clone();
        let url = self.cfg.url.clone();
        let key = self.cfg.stream_key.clone();

        tokio::spawn(async move {
            run_publisher(url, key, rx, state, bytes_sent, reconnects, dropped).await;
        });
        Ok(())
    }

    fn send_video(&self, frame: SharedEncodedFrame) {
        match self.tx.try_send(Msg::Video(frame)) {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {}
        }
    }

    fn send_audio(&self, packet: AudioPacket) {
        let _ = self.tx.try_send(Msg::Audio(packet));
    }

    fn state(&self) -> TransportState {
        match self.state.load(Ordering::Relaxed) {
            x if x == TransportState::Connecting as u8 => TransportState::Connecting,
            x if x == TransportState::Connected as u8 => TransportState::Connected,
            x if x == TransportState::Reconnecting as u8 => TransportState::Reconnecting,
            x if x == TransportState::Failed as u8 => TransportState::Failed,
            x if x == TransportState::Stopped as u8 => TransportState::Stopped,
            _ => TransportState::Idle,
        }
    }

    fn stats(&self) -> TransportStats {
        TransportStats {
            state: Some(self.state()),
            bytes_sent: Some(self.bytes_sent.load(Ordering::Relaxed)),
            reconnects: Some(self.reconnects.load(Ordering::Relaxed)),
            dropped_frames: Some(self.dropped.load(Ordering::Relaxed)),
            ..Default::default()
        }
    }

    async fn stop(&self) {
        self.state
            .store(TransportState::Stopped as u8, Ordering::Relaxed);
    }
}

/// Publisher task: connect, publish, mux, and reconnect with backoff. Isolated from
/// the rest of the engine — if YouTube drops, only this task reconnects; capture,
/// NVENC and WebRTC keep running.
async fn run_publisher(
    url: String,
    key: Secret,
    mut rx: mpsc::Receiver<Msg>,
    state: Arc<AtomicU8>,
    bytes_sent: Arc<AtomicU64>,
    reconnects: Arc<AtomicU32>,
    _dropped: Arc<AtomicU64>,
) {
    use g9_core::h264::annexb_to_avcc;
    use g9_core::time::PtsClock;

    let parsed = match crate::client::RtmpUrl::parse(&url) {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(target: "g9::rtmp", "bad rtmp url: {e}");
            state.store(TransportState::Failed as u8, Ordering::Relaxed);
            return;
        }
    };

    let mut backoff = Duration::from_millis(BACKOFF_START_MS);
    let mut first_attempt = true;

    loop {
        if state.load(Ordering::Relaxed) == TransportState::Stopped as u8 {
            break;
        }
        if !first_attempt {
            state.store(TransportState::Reconnecting as u8, Ordering::Relaxed);
            reconnects.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "g9::rtmp", "RTMPS reconnecting to {} in {:?}", redact(&url), backoff);
            tokio::time::sleep(backoff).await;
            backoff = RtmpTransport::next_backoff(backoff);
        }
        first_attempt = false;
        state.store(TransportState::Connecting as u8, Ordering::Relaxed);

        // Connect + publish. Key is used as the publish name and never logged.
        let mut client = match crate::client::RtmpClient::connect_and_publish(&parsed, &key).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(target: "g9::rtmp", "connect to {} failed: {e}", redact(&url));
                continue;
            }
        };
        state.store(TransportState::Connected as u8, Ordering::Relaxed);
        tracing::info!(target: "g9::rtmp", "RTMPS connected to {}", redact(&url));
        backoff = Duration::from_millis(BACKOFF_START_MS);

        // After (re)connect we must send the AVC sequence header and wait for a
        // keyframe before sending delta frames so the server has decoder config.
        let mut need_keyframe = true;

        // Drain media until a write errors, then reconnect.
        //
        // We coalesce writes: block for one message, then greedily drain whatever
        // else is already buffered, writing each to the socket, and flush ONCE per
        // batch. Flushing after every tiny audio/video message issues a syscall
        // (and TLS record) per packet; under load that is slow enough to back the
        // bounded queue up, drop video frames, and leave YouTube seeing a choppy
        // ingest that flaps active/inactive and never finalizes past liveStarting.
        let conn_result: Result<()> = loop {
            let first = match rx.recv().await {
                Some(m) => m,
                None => break Ok(()), // channel closed → shutting down
            };

            // Process the blocking message plus any already-queued messages, then
            // flush once. `wrote` guards against an empty flush when every message
            // in the batch was skipped (e.g. pre-keyframe delta frames).
            let mut batch = Some(first);
            let mut wrote = false;
            let batch_result: Result<()> = loop {
                let msg = match batch.take() {
                    Some(m) => m,
                    None => match rx.try_recv() {
                        Ok(m) => m,
                        Err(_) => break Ok(()), // nothing more buffered → flush
                    },
                };
                match msg {
                    Msg::Video(f) => {
                        if need_keyframe {
                            if !f.is_key() {
                                continue;
                            }
                            // Send the AVC sequence header from this keyframe's SPS/PPS.
                            if let Some(ps) = &f.parameter_sets {
                                if let Err(e) = client.send_video_sequence_header(ps).await {
                                    break Err(e);
                                }
                            }
                            need_keyframe = false;
                        }
                        let avcc = annexb_to_avcc(&f.data);
                        let ts = PtsClock::to_millis(f.pts);
                        if let Err(e) = client.send_video(&avcc, f.is_key(), ts).await {
                            break Err(e);
                        }
                        wrote = true;
                        bytes_sent.fetch_add(avcc.len() as u64, Ordering::Relaxed);
                    }
                    Msg::Audio(p) => {
                        // The AAC AudioSpecificConfig arrives as the first `is_config` packet.
                        if p.is_config {
                            if let Err(e) = client.send_audio_sequence_header(&p.data).await {
                                break Err(e);
                            }
                            wrote = true;
                            continue;
                        }
                        if !client.audio_seq_sent() {
                            // Haven't seen the config yet; skip audio until we do.
                            continue;
                        }
                        let ts = PtsClock::to_millis(p.pts);
                        if let Err(e) = client.send_audio(&p.data, ts).await {
                            break Err(e);
                        }
                        wrote = true;
                        bytes_sent.fetch_add(p.data.len() as u64, Ordering::Relaxed);
                    }
                }
            };

            if let Err(e) = batch_result {
                break Err(e);
            }
            if wrote {
                if let Err(e) = client.flush().await {
                    break Err(e);
                }
            }
        };

        if let Err(e) = conn_result {
            tracing::warn!(target: "g9::rtmp", "RTMPS write failed: {e}");
            // loop → reconnect
        } else {
            // Channel closed cleanly.
            break;
        }
    }
    state.store(TransportState::Stopped as u8, Ordering::Relaxed);
}

/// Redact a stream key if one ever appears in a URL (defense in depth).
fn redact(url: &str) -> String {
    // Our URLs never contain the key, but if a user put it in `--rtmp-url`,
    // strip anything after the last '/'.
    match url.rfind('/') {
        Some(i) if i + 1 < url.len() => format!("{}/***", &url[..i]),
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded_and_exponential() {
        let mut d = Duration::from_millis(BACKOFF_START_MS);
        d = RtmpTransport::next_backoff(d);
        assert_eq!(d, Duration::from_millis(2000));
        d = RtmpTransport::next_backoff(d);
        assert_eq!(d, Duration::from_millis(4000));
        for _ in 0..10 {
            d = RtmpTransport::next_backoff(d);
        }
        assert_eq!(d, Duration::from_millis(BACKOFF_MAX_MS));
    }

    #[test]
    fn redacts_key_in_url() {
        assert_eq!(redact("rtmps://x.youtube.com/live2/SECRET"), "rtmps://x.youtube.com/live2/***");
    }
}
