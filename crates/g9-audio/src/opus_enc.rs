//! Opus encoder for the WebRTC audio track. Cross-platform (libopus via `audiopus`).
//!
//! WebRTC audio is Opus at 48 kHz. We buffer PCM into 20 ms frames (960 samples per
//! channel at 48 kHz) and encode each frame.

use crate::PcmChunk;
use g9_core::{transport::AudioPacket, Error, Result};
use std::time::Duration;

pub struct OpusEncoder {
    enc: audiopus::coder::Encoder,
    sample_rate: u32,
    channels: u8,
    /// 20 ms worth of samples per channel.
    frame_samples: usize,
    buf: Vec<f32>,
    samples_emitted: u64,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u8, bitrate_bps: u32) -> Result<Self> {
        use audiopus::{Application, Channels, SampleRate};
        let sr = match sample_rate {
            8000 => SampleRate::Hz8000,
            12000 => SampleRate::Hz12000,
            16000 => SampleRate::Hz16000,
            24000 => SampleRate::Hz24000,
            48000 => SampleRate::Hz48000,
            other => {
                return Err(Error::audio(format!(
                    "unsupported Opus sample rate {other} (use 48000)"
                )))
            }
        };
        let ch = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => return Err(Error::audio(format!("unsupported channel count {n}"))),
        };
        let mut enc = audiopus::coder::Encoder::new(sr, ch, Application::Audio)
            .map_err(|e| Error::audio(format!("opus encoder init: {e}")))?;
        enc.set_bitrate(audiopus::Bitrate::BitsPerSecond(bitrate_bps as i32))
            .map_err(|e| Error::audio(format!("opus set_bitrate: {e}")))?;
        let frame_samples = (sample_rate as usize / 50) * channels as usize; // 20 ms
        Ok(Self {
            enc,
            sample_rate,
            channels,
            frame_samples,
            buf: Vec::with_capacity(frame_samples * 2),
            samples_emitted: 0,
        })
    }

    /// Append PCM, emit as many 20 ms Opus packets as are now complete.
    pub fn encode(&mut self, pcm: &PcmChunk) -> Result<Vec<AudioPacket>> {
        self.buf.extend_from_slice(&pcm.samples);
        let mut out = Vec::new();
        let mut scratch = vec![0u8; 4000];
        while self.buf.len() >= self.frame_samples {
            let frame: Vec<f32> = self.buf.drain(..self.frame_samples).collect();
            let n = self
                .enc
                .encode_float(&frame, &mut scratch)
                .map_err(|e| Error::audio(format!("opus encode: {e}")))?;
            let per_channel = self.frame_samples / self.channels as usize;
            let pts = Duration::from_nanos(
                self.samples_emitted * 1_000_000_000 / self.sample_rate as u64,
            );
            self.samples_emitted += per_channel as u64;
            out.push(AudioPacket {
                data: bytes::Bytes::copy_from_slice(&scratch[..n]),
                pts,
                sample_rate: self.sample_rate,
                channels: self.channels,
                is_config: false,
            });
        }
        Ok(out)
    }
}
