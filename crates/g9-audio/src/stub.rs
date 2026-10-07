//! Non-Windows stubs for WASAPI capture and the AAC encoder.

use crate::PcmChunk;
use g9_core::{transport::AudioPacket, Error, Result};

pub struct WasapiCapture;

impl WasapiCapture {
    pub fn new(_sample_rate: u32, _channels: u8) -> Result<Self> {
        Err(Error::Unsupported(
            "WASAPI capture is only available on Windows".into(),
        ))
    }
    pub fn read(&mut self) -> Result<Option<PcmChunk>> {
        Err(Error::Unsupported("WASAPI is only available on Windows".into()))
    }
    pub fn sample_rate(&self) -> u32 {
        48000
    }
    pub fn channels(&self) -> u8 {
        2
    }
}

pub struct AacEncoder;

impl AacEncoder {
    pub fn new(_sample_rate: u32, _channels: u8, _bitrate_bps: u32) -> Result<Self> {
        Err(Error::Unsupported(
            "AAC via Media Foundation is only available on Windows".into(),
        ))
    }
    pub fn encode(&mut self, _pcm: &PcmChunk) -> Result<Vec<AudioPacket>> {
        Err(Error::Unsupported("AAC encode is only available on Windows".into()))
    }
}

/// Opus stub for non-Windows type-checking. The real libopus-backed encoder in
/// `opus_enc.rs` is compiled on the Windows target.
pub struct OpusEncoder;

impl OpusEncoder {
    pub fn new(_sample_rate: u32, _channels: u8, _bitrate_bps: u32) -> Result<Self> {
        Err(Error::Unsupported(
            "Opus encoder is compiled on the Windows target".into(),
        ))
    }
    pub fn encode(&mut self, _pcm: &PcmChunk) -> Result<Vec<AudioPacket>> {
        Err(Error::Unsupported("Opus encode available on the Windows target".into()))
    }
}

/// Opus decoder stub for non-Windows type-checking. The real libopus-backed
/// decoder in `opus_dec.rs` is compiled on the Windows target.
pub struct OpusDecoder;

impl OpusDecoder {
    pub fn new(_sample_rate: u32, _channels: u8) -> Result<Self> {
        Err(Error::Unsupported(
            "Opus decoder is compiled on the Windows target".into(),
        ))
    }
    pub fn decode(&mut self, _packet: &[u8]) -> Result<Vec<f32>> {
        Err(Error::Unsupported(
            "Opus decode available on the Windows target".into(),
        ))
    }
}
