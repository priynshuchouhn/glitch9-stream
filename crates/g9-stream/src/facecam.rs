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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Changes whenever a publication ends or restarts.
    video_generation: AtomicU64,
    /// Queued Opus mic packets awaiting mix into the broadcast audio.
    audio_queue: Mutex<VecDeque<Bytes>>,
}

impl FacecamState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                video_queue: Mutex::new(VecDeque::new()),
                video_generation: AtomicU64::new(0),
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

    /// Drain every pending camera access unit in arrival order. Camera publishers
    /// commonly run at 30 fps while the output loop can dip below that rate; only
    /// consuming one AU per output frame creates an ever-growing decode delay and
    /// eventually drops inter-frame dependencies when the queue reaches its cap.
    pub fn drain_video(&self) -> Vec<(FacecamCodec, Bytes)> {
        self.inner.video_queue.lock().drain(..).collect()
    }

    /// Clear camera video without touching microphone packets. The generation
    /// change tells the GPU compositor to remove its last decoded texture.
    pub fn clear_video(&self) {
        self.inner.video_queue.lock().clear();
        self.inner.video_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn video_generation(&self) -> u64 {
        self.inner.video_generation.load(Ordering::SeqCst)
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
    fn draining_video_preserves_decode_order() {
        let state = FacecamState::new();
        state.push_video(FacecamCodec::Vp8, Bytes::from_static(b"keyframe"));
        state.push_video(FacecamCodec::Vp8, Bytes::from_static(b"delta"));

        let drained = state.drain_video();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].1, Bytes::from_static(b"keyframe"));
        assert_eq!(drained[1].1, Bytes::from_static(b"delta"));
        assert!(state.drain_video().is_empty());
    }

    #[test]
    fn clearing_video_drops_frames_and_advances_generation() {
        let state = FacecamState::new();
        state.push_video(FacecamCodec::Vp8, Bytes::from_static(b"frame"));
        let generation = state.video_generation();

        state.clear_video();

        assert!(state.drain_video().is_empty());
        assert!(state.video_generation() > generation);
    }
}
