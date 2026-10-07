//! Opus decoder for the facecam microphone. The browser publishes the player's
//! voice as Opus (48 kHz); the engine decodes it here to interleaved f32 PCM so the
//! audio pipeline can mix it into the system/game audio before re-encoding to the
//! broadcast outputs. Cross-platform (libopus via `audiopus`).

use g9_core::{Error, Result};

/// Max PCM samples a single Opus packet can decode to: 120 ms at 48 kHz, times
/// channels. Opus frames are <= 120 ms, so this buffer is always large enough.
const MAX_FRAME_SAMPLES_PER_CH: usize = 48_000 / 1000 * 120;

pub struct OpusDecoder {
    dec: audiopus::coder::Decoder,
    channels: usize,
    scratch: Vec<f32>,
}

impl OpusDecoder {
    /// Create a decoder for `sample_rate` (expected 48 kHz) and channel count.
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        use audiopus::{Channels, SampleRate};
        let rate = match sample_rate {
            48000 => SampleRate::Hz48000,
            24000 => SampleRate::Hz24000,
            16000 => SampleRate::Hz16000,
            12000 => SampleRate::Hz12000,
            8000 => SampleRate::Hz8000,
            n => return Err(Error::audio(format!("unsupported opus rate {n}"))),
        };
        let ch = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => return Err(Error::audio(format!("unsupported channel count {n}"))),
        };
        let dec = audiopus::coder::Decoder::new(rate, ch)
            .map_err(|e| Error::audio(format!("opus decoder init: {e}")))?;
        Ok(Self {
            dec,
            channels: channels as usize,
            scratch: vec![0.0; MAX_FRAME_SAMPLES_PER_CH * channels as usize],
        })
    }

    /// Decode one Opus packet to interleaved f32 PCM.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>> {
        use audiopus::packet::Packet;
        use audiopus::MutSignals;
        let input = Packet::try_from(packet)
            .map_err(|e| Error::audio(format!("opus packet: {e}")))?;
        let output = MutSignals::try_from(&mut self.scratch[..])
            .map_err(|e| Error::audio(format!("opus output signals: {e}")))?;
        let frames = self
            .dec
            .decode_float(Some(input), output, false)
            .map_err(|e| Error::audio(format!("opus decode: {e}")))?;
        let samples = frames * self.channels;
        Ok(self.scratch[..samples].to_vec())
    }
}
