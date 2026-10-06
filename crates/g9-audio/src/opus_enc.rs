//! Opus encoder for the WebRTC audio track. Cross-platform (libopus via `audiopus`).
//!
//! WebRTC audio is Opus at 48 kHz. We buffer PCM into 20 ms frames (960 samples per
//! channel at 48 kHz) and encode each frame.

use crate::PcmChunk;
use g9_core::{transport::AudioPacket, Error, Result};
use std::time::Duration;

pub struct OpusEncoder {
    enc: audiopus::coder::Encoder,
    /// Opus always runs at 48 kHz for WebRTC.
    out_rate: u32,
    channels: u8,
    /// Input sample rate from WASAPI (e.g. 44100). Resampled to 48000 if different.
    in_rate: u32,
    /// 20 ms worth of (interleaved) samples per Opus frame at the OUTPUT rate.
    frame_samples: usize,
    buf: Vec<f32>,
    samples_emitted: u64,
    /// Fractional read position into the input stream, for linear resampling.
    resample_pos: f64,
    /// Carryover of input samples across chunks (interleaved), for the resampler.
    in_tail: Vec<f32>,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u8, bitrate_bps: u32) -> Result<Self> {
        use audiopus::{Application, Channels, SampleRate};
        // WebRTC Opus is 48 kHz. We resample any other input rate up/down to 48k.
        let out_rate = 48000u32;
        let ch = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => return Err(Error::audio(format!("unsupported channel count {n}"))),
        };
        let mut enc = audiopus::coder::Encoder::new(SampleRate::Hz48000, ch, Application::Audio)
            .map_err(|e| Error::audio(format!("opus encoder init: {e}")))?;
        enc.set_bitrate(audiopus::Bitrate::BitsPerSecond(bitrate_bps as i32))
            .map_err(|e| Error::audio(format!("opus set_bitrate: {e}")))?;
        let frame_samples = (out_rate as usize / 50) * channels as usize; // 20 ms @ 48k
        Ok(Self {
            enc,
            out_rate,
            channels,
            in_rate: sample_rate,
            frame_samples,
            buf: Vec::with_capacity(frame_samples * 2),
            samples_emitted: 0,
            resample_pos: 0.0,
            in_tail: Vec::new(),
        })
    }

    /// Linear-resample interleaved f32 PCM from `in_rate` to 48 kHz. Linear is light
    /// and dependency-free; adequate for a POC. (A production build would use a
    /// polyphase/sinc resampler for better quality.)
    fn resample_to_out(&mut self, input: &[f32]) -> Vec<f32> {
        let ch = self.channels as usize;
        if self.in_rate == self.out_rate || input.is_empty() {
            return input.to_vec();
        }
        // Prepend carryover tail so interpolation is continuous across chunks.
        let mut src = std::mem::take(&mut self.in_tail);
        src.extend_from_slice(input);
        let frames = src.len() / ch;
        if frames < 2 {
            self.in_tail = src;
            return Vec::new();
        }
        let ratio = self.in_rate as f64 / self.out_rate as f64;
        let mut out = Vec::new();
        let mut pos = self.resample_pos;
        while (pos as usize) + 1 < frames {
            let i = pos as usize;
            let frac = (pos - i as f64) as f32;
            for c in 0..ch {
                let a = src[i * ch + c];
                let b = src[(i + 1) * ch + c];
                out.push(a + (b - a) * frac);
            }
            pos += ratio;
        }
        // Keep the last whole input frame (and fractional offset) for next time.
        let consumed = pos as usize;
        self.resample_pos = pos - consumed as f64;
        let keep_from = consumed.min(frames.saturating_sub(1)) * ch;
        self.in_tail = src[keep_from..].to_vec();
        out
    }

    /// Append PCM, emit as many 20 ms Opus packets as are now complete.
    pub fn encode(&mut self, pcm: &PcmChunk) -> Result<Vec<AudioPacket>> {
        let resampled = self.resample_to_out(&pcm.samples);
        self.buf.extend_from_slice(&resampled);
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
                self.samples_emitted * 1_000_000_000 / self.out_rate as u64,
            );
            self.samples_emitted += per_channel as u64;
            out.push(AudioPacket {
                data: bytes::Bytes::copy_from_slice(&scratch[..n]),
                pts,
                sample_rate: self.out_rate,
                channels: self.channels,
                is_config: false,
            });
        }
        Ok(out)
    }
}
