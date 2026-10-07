//! `g9-audio` — WASAPI loopback capture → PCM, then Opus (WebRTC) and AAC (YouTube).
//! Capture happens once; the PCM is fed to both encoders when both outputs are on.

// AAC AudioSpecificConfig helper is pure byte logic — always compiled + tested.
pub mod asc;

#[cfg(windows)]
mod opus_enc;
#[cfg(windows)]
pub use opus_enc::OpusEncoder;
#[cfg(windows)]
mod opus_dec;
#[cfg(windows)]
pub use opus_dec::OpusDecoder;
#[cfg(windows)]
mod wasapi;
#[cfg(windows)]
pub use wasapi::WasapiCapture;
#[cfg(windows)]
mod aac;
#[cfg(windows)]
pub use aac::AacEncoder;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::{AacEncoder, OpusDecoder, OpusEncoder, WasapiCapture};

/// A chunk of interleaved f32 PCM captured from WASAPI.
#[derive(Debug, Clone)]
pub struct PcmChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u8,
    pub pts: std::time::Duration,
}
