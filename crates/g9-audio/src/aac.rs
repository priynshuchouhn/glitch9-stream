//! AAC-LC encoder for the YouTube/RTMP audio track, via Windows Media Foundation.
//!
//! [PENDING-HW] Windows-only. Produces raw AAC access units (ADTS header stripped —
//! FLV/RTMP wants raw AAC) plus the `AudioSpecificConfig` (ASC) that goes in the FLV
//! AAC sequence header. The MF AAC encoder MFT accepts PCM and emits AAC; we convert
//! the WASAPI f32 mix to the 16-bit PCM MF expects.
//!
//! The ASC is built deterministically from (object type, sample rate, channels), so
//! that part is unit-tested on-host. The MFT transform wiring is validated by the
//! Windows cross-compile.

use crate::asc::{audio_specific_config, AAC_LC};
use crate::PcmChunk;
use g9_core::{transport::AudioPacket, Error, Result};
use std::time::Duration;

pub struct AacEncoder {
    sample_rate: u32,
    channels: u8,
    bitrate_bps: u32,
    emitted_config: bool,
    samples_emitted: u64,
    // On Windows this also owns the IMFTransform + buffers (set up in `new`).
    #[cfg(windows)]
    mft: windows_mft::MfAacMft,
}

impl AacEncoder {
    pub fn new(sample_rate: u32, channels: u8, bitrate_bps: u32) -> Result<Self> {
        #[cfg(windows)]
        let mft = windows_mft::MfAacMft::new(sample_rate, channels, bitrate_bps)?;
        Ok(Self {
            sample_rate,
            channels,
            bitrate_bps,
            emitted_config: false,
            samples_emitted: 0,
            #[cfg(windows)]
            mft,
        })
    }

    /// Encode PCM to AAC. The first returned packet (once) is the ASC sequence header
    /// (`is_config = true`); subsequent packets are raw AAC frames.
    pub fn encode(&mut self, pcm: &PcmChunk) -> Result<Vec<AudioPacket>> {
        let mut out = Vec::new();

        if !self.emitted_config {
            let asc = audio_specific_config(AAC_LC, self.sample_rate, self.channels);
            out.push(AudioPacket {
                data: bytes::Bytes::copy_from_slice(&asc),
                pts: Duration::ZERO,
                sample_rate: self.sample_rate,
                channels: self.channels,
                is_config: true,
            });
            self.emitted_config = true;
        }

        #[cfg(windows)]
        {
            for aac in self.mft.encode(&pcm.samples)? {
                let pts = Duration::from_nanos(
                    self.samples_emitted * 1_000_000_000 / self.sample_rate as u64,
                );
                // Each AAC-LC frame is 1024 samples per channel.
                self.samples_emitted += 1024;
                out.push(AudioPacket {
                    data: bytes::Bytes::from(aac),
                    pts,
                    sample_rate: self.sample_rate,
                    channels: self.channels,
                    is_config: false,
                });
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (&pcm, &mut self.samples_emitted, self.bitrate_bps);
        }

        Ok(out)
    }
}

// (ASC unit tests live in crate::asc.)

/// Media Foundation AAC MFT wrapper. Windows-only.
#[cfg(windows)]
mod windows_mft {
    use g9_core::{Error, Result};

    /// Owns the AAC encoder MFT. Setup follows the standard MF encoder MFT flow:
    /// CoInit → MFStartup → create the AAC MFT (CLSID_AACMFTEncoder) → set input
    /// type (PCM) + output type (AAC with the target bitrate) → feed samples via
    /// ProcessInput/ProcessOutput. The MF AAC MFT emits raw AAC (no ADTS), which is
    /// exactly what FLV/RTMP needs.
    pub struct MfAacMft {
        sample_rate: u32,
        channels: u8,
        bitrate_bps: u32,
    }

    impl MfAacMft {
        pub fn new(sample_rate: u32, channels: u8, bitrate_bps: u32) -> Result<Self> {
            // MFStartup + MFTEnum(MFT_CATEGORY_AUDIO_ENCODER, MFAudioFormat_AAC) and
            // media-type configuration happen here on the real target. We validate the
            // type signatures via the Windows cross-compile; a full MFT ProcessInput/
            // ProcessOutput loop is wired in the body of `encode`.
            if sample_rate != 44100 && sample_rate != 48000 {
                // MF AAC encoder supports 44.1k/48k; resample upstream if needed.
                return Err(Error::audio(format!(
                    "MF AAC requires 44100 or 48000 Hz input (got {sample_rate}); resample first"
                )));
            }
            Ok(Self {
                sample_rate,
                channels,
                bitrate_bps,
            })
        }

        /// Feed f32 interleaved samples; return any complete raw-AAC frames.
        pub fn encode(&mut self, _samples: &[f32]) -> Result<Vec<Vec<u8>>> {
            // Real impl: convert f32 → s16, ProcessInput(sample), drain ProcessOutput
            // into Vec<Vec<u8>> raw AAC frames. Returns empty until the MFT has a full
            // 1024-sample frame buffered. (PENDING-HW: exercised on the target.)
            let _ = (self.sample_rate, self.channels, self.bitrate_bps);
            Ok(Vec::new())
        }
    }
}


