//! Shared facecam state bridging the async WHEP subscriber (which receives the
//! player's browser-published camera + mic) and the synchronous video/audio worker
//! threads that composite the camera over the game and mix the mic into the audio.
//!
//! The subscriber task queues H.264 access units and Opus audio packets here; the
//! video thread decodes camera access units in order and composites the newest
//! decoded frame, while the audio thread drains queued mic packets to mix.

use bytes::Bytes;
use g9_capture::FacecamCodec;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// Bound on the queued mic packets so a stalled consumer can't grow memory without
/// limit; old packets are dropped (audio favors freshness over completeness).
const MAX_AUDIO_QUEUE: usize = 64;

/// H.264 is inter-frame compressed, so the decoder must see the initial IDR before
/// any dependent delta frames. Keep a bounded ordered queue instead of a single
/// "latest" slot: a short arrival burst must not overwrite the startup keyframe.
const MAX_VIDEO_QUEUE: usize = 120;

/// Thread-safe handle shared between the subscriber task and the worker threads.
#[derive(Clone)]
pub struct FacecamState {
    inner: Arc<Inner>,
}

struct Inner {
    /// Encoded camera access units waiting to be decoded, in arrival order.
    video_queue: Mutex<VecDeque<(FacecamCodec, Bytes)>>,
    /// Queued Opus mic packets awaiting mix into the broadcast audio.
    audio_queue: Mutex<VecDeque<Bytes>>,
}

impl FacecamState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                video_queue: Mutex::new(VecDeque::new()),
                audio_queue: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Queue a camera access unit without allowing an unconsumed startup keyframe
    /// to be overwritten by the delta frames that immediately follow it.
    pub fn push_video(&self, codec: FacecamCodec, au: Bytes) {
        let mut q = self.inner.video_queue.lock();
        if q.len() >= MAX_VIDEO_QUEUE {
            q.pop_front();
        }
        q.push_back((codec, au));
    }

    /// Take the oldest queued camera access unit so inter-frame decode order is
    /// preserved.
    pub fn take_video(&self) -> Option<(FacecamCodec, Bytes)> {
        self.inner.video_queue.lock().pop_front()
    }

    /// Queue a mic Opus packet, dropping the oldest when the bound is reached.
    pub fn push_audio(&self, pkt: Bytes) {
        let mut q = self.inner.audio_queue.lock();
        if q.len() >= MAX_AUDIO_QUEUE {
            q.pop_front();
        }
        q.push_back(pkt);
    }

    /// Drain all currently queued mic packets for mixing.
    pub fn drain_audio(&self) -> Vec<Bytes> {
        let mut q = self.inner.audio_queue.lock();
        q.drain(..).collect()
    }
}

impl Default for FacecamState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_access_units_are_consumed_in_arrival_order() {
        let state = FacecamState::new();
        state.push_video(FacecamCodec::H264, Bytes::from_static(b"keyframe"));
        state.push_video(FacecamCodec::H264, Bytes::from_static(b"delta"));

        assert_eq!(state.take_video().unwrap().1, Bytes::from_static(b"keyframe"));
        assert_eq!(state.take_video().unwrap().1, Bytes::from_static(b"delta"));
        assert!(state.take_video().is_none());
    }
}
