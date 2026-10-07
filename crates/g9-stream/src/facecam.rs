//! Shared facecam state bridging the async WHEP subscriber (which receives the
//! player's browser-published camera + mic) and the synchronous video/audio worker
//! threads that composite the camera over the game and mix the mic into the audio.
//!
//! The subscriber task pushes the latest H.264 access unit and queues Opus audio
//! packets here; the video thread pulls the newest camera frame to composite, and
//! the audio thread drains queued mic packets to mix. Lock-light and lossy by
//! design: a cam frame or two dropped under load never stalls the game pipeline.

use bytes::Bytes;
use g9_capture::FacecamCodec;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// Bound on the queued mic packets so a stalled consumer can't grow memory without
/// limit; old packets are dropped (audio favors freshness over completeness).
const MAX_AUDIO_QUEUE: usize = 64;

/// Thread-safe handle shared between the subscriber task and the worker threads.
#[derive(Clone)]
pub struct FacecamState {
    inner: Arc<Inner>,
}

struct Inner {
    /// Most recent camera video frame + its codec (H.264 Annex-B or VP8), if any.
    /// Replaced each time a newer one arrives — the compositor only needs the
    /// latest frame — and tagged so the compositor selects the right decoder.
    latest_video: Mutex<Option<(FacecamCodec, Bytes)>>,
    /// Queued Opus mic packets awaiting mix into the broadcast audio.
    audio_queue: Mutex<VecDeque<Bytes>>,
}

impl FacecamState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                latest_video: Mutex::new(None),
                audio_queue: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Store the newest camera frame + codec (replacing any un-consumed one).
    pub fn push_video(&self, codec: FacecamCodec, au: Bytes) {
        *self.inner.latest_video.lock() = Some((codec, au));
    }

    /// Take the latest camera frame + codec, if a new one arrived since last call.
    pub fn take_video(&self) -> Option<(FacecamCodec, Bytes)> {
        self.inner.latest_video.lock().take()
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
