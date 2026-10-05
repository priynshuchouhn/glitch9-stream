//! Encoder profiles and the shared-vs-dual-encoder decision.
//!
//! WebRTC and YouTube want different things from H.264. These profiles capture
//! those differences explicitly (no hidden hardcoding) and let the pipeline decide
//! whether one NVENC session can serve both (Mode A) or two are needed (Mode B).

use crate::frame::VideoCodec;

/// Which output a profile is tuned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    WebRtc,
    Youtube,
}

/// NVENC rate-control mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Constant bitrate — used for both WebRTC (predictable pacing) and YouTube (stable ingest).
    Cbr,
    /// Variable bitrate with a cap.
    VbrCapped,
}

/// A concrete encoder configuration. Fields map directly onto NVENC settings.
#[derive(Debug, Clone)]
pub struct EncoderProfile {
    pub output: OutputKind,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub rate_control: RateControl,
    /// NVENC preset, e.g. "p4". Kept as a string so SDK preset GUIDs stay in g9-encode.
    pub preset: String,
    /// NVENC tuning info: low_latency / ultra_low_latency / high_quality.
    pub tuning: String,
    /// Keyframe (IDR) interval in frames. WebRTC: large (keyframes on demand).
    /// YouTube: 2 * fps (2-second GOP) per YouTube's requirement.
    pub gop_frames: u32,
    /// Number of B-frames. 0 for low-latency WebRTC.
    pub b_frames: u32,
    /// Rate-control lookahead frames (0 = off).
    pub lookahead: u32,
}

impl EncoderProfile {
    /// Low-latency WebRTC default: P4, low_latency tuning, 0 B-frames, on-demand keyframes.
    pub fn webrtc(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Self {
        Self {
            output: OutputKind::WebRtc,
            codec: VideoCodec::H264,
            width,
            height,
            fps,
            bitrate_bps,
            rate_control: RateControl::Cbr,
            preset: "p4".to_string(),
            tuning: "low_latency".to_string(),
            // Effectively on-demand: we request IDR via force-IDR when a viewer joins
            // or a PLI arrives, rather than a tight periodic GOP.
            gop_frames: fps * 4,
            b_frames: 0,
            lookahead: 0,
        }
    }

    /// YouTube broadcast default: P5 (a touch more quality), CBR, 2-second GOP.
    /// YouTube requires keyframe interval <= ~4s; 2s is the recommended value.
    /// Documented so it can be tuned if YouTube's guidance changes.
    pub fn youtube(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Self {
        Self {
            output: OutputKind::Youtube,
            codec: VideoCodec::H264,
            width,
            height,
            fps,
            bitrate_bps,
            rate_control: RateControl::Cbr,
            preset: "p5".to_string(),
            tuning: "high_quality".to_string(),
            gop_frames: fps * 2, // 2-second keyframe interval
            b_frames: 0,         // keep 0 for POC; YouTube accepts B-frames, revisit later
            lookahead: 0,
        }
    }

    /// Can a single NVENC session serve both of these profiles?
    ///
    /// They must share geometry/fps/bitrate band and GOP/B-frame behavior. When
    /// true, the pipeline runs Mode A (one encoder). Otherwise Mode B (two
    /// encoders, shared NV12 input — capture + convert still happen once).
    pub fn compatible_with(&self, other: &EncoderProfile) -> bool {
        self.codec == other.codec
            && self.width == other.width
            && self.height == other.height
            && self.fps == other.fps
            && self.gop_frames == other.gop_frames
            && self.b_frames == other.b_frames
            // bitrates within 10% of each other
            && (self.bitrate_bps as i64 - other.bitrate_bps as i64).abs()
                <= (self.bitrate_bps as i64 / 10)
    }
}
