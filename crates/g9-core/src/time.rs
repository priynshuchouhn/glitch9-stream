//! Unified presentation-timestamp clock.
//!
//! A single `PtsClock` is created at stream start and shared by video + audio so
//! every output (WebRTC RTP timestamps, FLV/RTMP timestamps) derives from one
//! coherent timeline. We never use sleep-based sync.

use std::time::{Duration, Instant};

/// Monotonic clock measuring elapsed time since the stream started.
#[derive(Debug, Clone)]
pub struct PtsClock {
    start: Instant,
}

impl PtsClock {
    pub fn start_now() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// Current presentation time since stream start.
    pub fn now(&self) -> Duration {
        self.start.elapsed()
    }

    /// 90 kHz RTP timestamp units (H.264 video clock) for a given PTS.
    pub fn to_rtp_90k(pts: Duration) -> u32 {
        // Wraps at 2^32 as RTP requires.
        ((pts.as_nanos() as u128 * 90_000 / 1_000_000_000) & 0xFFFF_FFFF) as u32
    }

    /// Millisecond timestamp for FLV/RTMP.
    pub fn to_millis(pts: Duration) -> u32 {
        (pts.as_millis() & 0xFFFF_FFFF) as u32
    }
}

/// Tracks audio/video drift for reporting (not for correcting via sleeps).
#[derive(Debug, Default)]
pub struct DriftTracker {
    pub last_video_pts: Duration,
    pub last_audio_pts: Duration,
}

impl DriftTracker {
    pub fn on_video(&mut self, pts: Duration) {
        self.last_video_pts = pts;
    }
    pub fn on_audio(&mut self, pts: Duration) {
        self.last_audio_pts = pts;
    }
    /// Positive = audio ahead of video.
    pub fn drift(&self) -> i64 {
        self.last_audio_pts.as_millis() as i64 - self.last_video_pts.as_millis() as i64
    }
}
