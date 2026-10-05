//! In-process pipeline metrics. Transport-specific numbers come from `TransportStats`.
//!
//! Everything here is measured, never fabricated. Fields that aren't available on a
//! given run stay at their zero/`None` value and are reported as such.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Rolling counters updated by the pipeline stages.
#[derive(Debug, Default)]
pub struct PipelineCounters {
    pub frames_captured: AtomicU64,
    pub frames_converted: AtomicU64,
    pub frames_encoded: AtomicU64,
    pub dropped_capture: AtomicU64,
    pub dropped_encoder: AtomicU64,
    /// Count of full-frame CPU readbacks. MUST remain 0 for the GPU path.
    pub cpu_readbacks: AtomicU64,
}

impl PipelineCounters {
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// Latency samples (microseconds) per stage, kept as a simple moving average.
#[derive(Debug, Default)]
pub struct LatencyEwma {
    inner: Mutex<Ewma>,
}

#[derive(Debug, Default)]
struct Ewma {
    value_us: f64,
    initialized: bool,
}

impl LatencyEwma {
    pub fn observe(&self, d: Duration) {
        let mut g = self.inner.lock();
        let us = d.as_micros() as f64;
        if g.initialized {
            g.value_us = g.value_us * 0.9 + us * 0.1;
        } else {
            g.value_us = us;
            g.initialized = true;
        }
    }
    pub fn millis(&self) -> f64 {
        self.inner.lock().value_us / 1000.0
    }
}

/// Shared metrics handle passed to each stage.
#[derive(Clone, Default)]
pub struct Metrics {
    pub counters: Arc<PipelineCounters>,
    pub capture_latency: Arc<LatencyEwma>,
    pub convert_latency: Arc<LatencyEwma>,
    pub encode_latency: Arc<LatencyEwma>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }
}
