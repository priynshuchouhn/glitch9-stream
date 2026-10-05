//! The `MediaTransport` trait — the clean boundary between encoded media and any
//! output destination. WebRTC and RTMP both implement it. The pipeline fans an
//! `Arc<EncodedFrame>` out to every transport without caring what they are.

use crate::frame::SharedEncodedFrame;
use async_trait::async_trait;
use bytes::Bytes;
use std::time::Duration;

/// Encoded audio handed to a transport. The pipeline encodes PCM once per codec
/// (Opus for WebRTC, AAC for RTMP) and routes the right one to each transport.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub data: Bytes,
    pub pts: Duration,
    /// Sample rate used when encoding (e.g. 48000).
    pub sample_rate: u32,
    pub channels: u8,
    /// True once per stream when this packet carries codec config
    /// (e.g. the AAC AudioSpecificConfig for the FLV sequence header).
    pub is_config: bool,
}

/// Connection state of a transport, surfaced in metrics. A transport reports its
/// own state; a failure in one must never be interpreted as a failure of another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransportState {
    Idle = 0,
    Connecting = 1,
    Connected = 2,
    Reconnecting = 3,
    Failed = 4,
    Stopped = 5,
}

/// A media output destination.
///
/// Implementations own their networking and reconnect logic. The pipeline calls
/// `send_video`/`send_audio` with shared frames; implementations must enqueue into
/// a **bounded** internal queue and drop stale frames rather than block the caller
/// (so a slow output can never stall the encoder or another transport).
#[async_trait]
pub trait MediaTransport: Send + Sync {
    /// Human-readable name for logs/metrics ("webrtc", "youtube").
    fn name(&self) -> &str;

    /// Begin connecting. Returns quickly; actual connect happens in the background.
    async fn start(&self) -> crate::Result<()>;

    /// Offer a video frame. Must be non-blocking (bounded queue; drop-stale on full).
    fn send_video(&self, frame: SharedEncodedFrame);

    /// Offer an audio packet. Must be non-blocking.
    fn send_audio(&self, packet: AudioPacket);

    /// Current connection state (for metrics).
    fn state(&self) -> TransportState;

    /// Per-transport metrics snapshot.
    fn stats(&self) -> TransportStats;

    /// Stop and release resources. Idempotent.
    async fn stop(&self);
}

/// Metrics a transport reports every snapshot interval. Fields that cannot be
/// measured for a given transport are `None` — never fabricated.
#[derive(Debug, Clone, Default)]
pub struct TransportStats {
    pub state: Option<TransportState>,
    pub viewers: Option<u32>,
    pub bitrate_bps: Option<u32>,
    pub rtt_ms: Option<f64>,
    pub packet_loss: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub bytes_sent: Option<u64>,
    pub reconnects: Option<u32>,
    pub dropped_frames: Option<u64>,
}
