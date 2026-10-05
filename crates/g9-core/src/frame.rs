//! Frame types that flow through the pipeline.
//!
//! `EncodedFrame` is the unit of fan-out: NVENC produces one, and it is shared
//! (via `Arc`) to every enabled transport. Transports must not mutate it.

use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;

/// Video codec of an encoded frame. H.264 is the only codec this POC produces,
/// but the enum keeps the door open for HEVC/AV1 later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VideoCodec {
    H264,
    // Hevc, Av1  // future
}

/// Pixel format of a GPU/CPU surface. Capture gives us BGRA; NVENC consumes NV12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFormat {
    /// 8-bit BGRA (what DXGI Desktop Duplication typically hands back).
    Bgra8,
    /// 8-bit NV12 (what we feed NVENC).
    Nv12,
}

/// Whether an encoded frame is a keyframe (IDR) or a delta frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// IDR / keyframe. Carries SPS/PPS when `parameter_sets` is set.
    Key,
    /// Non-IDR delta frame.
    Delta,
}

/// A single encoded video frame, ready for packetization/muxing.
///
/// `data` is an Annex-B or AVCC H.264 bitstream depending on `annex_b`:
/// - WebRTC RTP packetization wants Annex-B NAL units (start codes).
/// - FLV/RTMP wants AVCC (length-prefixed) NALs + an out-of-band AVCDecoderConfigurationRecord.
///
/// NVENC can emit Annex-B; the RTMP muxer converts to AVCC and extracts SPS/PPS.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub codec: VideoCodec,
    pub kind: FrameKind,
    /// H.264 bitstream for this access unit.
    pub data: Bytes,
    /// True if `data` uses Annex-B start codes (`00 00 00 01`).
    pub annex_b: bool,
    /// Presentation timestamp from the unified `PtsClock` (monotonic from stream start).
    pub pts: Duration,
    /// Decode timestamp. Equals `pts` when B-frames are disabled (our WebRTC default).
    pub dts: Duration,
    /// SPS/PPS parameter sets for this stream, present at least on the first keyframe.
    /// Transports cache these so a late viewer / a reconnect gets a valid config.
    pub parameter_sets: Option<ParameterSets>,
}

impl EncodedFrame {
    pub fn is_key(&self) -> bool {
        matches!(self.kind, FrameKind::Key)
    }
}

/// H.264 SPS/PPS, extracted once and reused for RTP out-of-band config and the
/// FLV AVCDecoderConfigurationRecord.
#[derive(Debug, Clone)]
pub struct ParameterSets {
    pub sps: Bytes,
    pub pps: Bytes,
    /// Decoded profile/level for building the AVCC record and RTP `profile-level-id`.
    pub profile_idc: u8,
    pub profile_compat: u8,
    pub level_idc: u8,
}

/// Convenience alias for the shared, fanned-out encoded frame.
pub type SharedEncodedFrame = Arc<EncodedFrame>;
