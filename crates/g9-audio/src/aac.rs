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
use g9_core::{transport::AudioPacket, Result};
use std::time::Duration;

pub struct AacEncoder {
    sample_rate: u32,
    channels: u8,
    #[allow(dead_code)] // read in the MFT; referenced via the ctor on Windows
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
    use windows::Win32::Media::MediaFoundation::{
        IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
        MFCreateSample, MFStartup, MFTEnumEx, MFAudioFormat_AAC, MFAudioFormat_PCM,
        MFMediaType_Audio, MFSTARTUP_LITE, MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_SORTANDFILTER,
        MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
        MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
        MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO, MFT_REGISTER_TYPE_INFO,
        MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
        MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
        MF_VERSION,
    };

    /// Bits per sample MF's AAC encoder requires on its PCM input.
    const PCM_BITS_PER_SAMPLE: u32 = 16;
    /// MFT ProcessOutput "needs more input" HRESULT (0xC00D6D72).
    const MF_E_TRANSFORM_NEED_MORE_INPUT: i32 = 0xC00D_6D72u32 as i32;

    /// Owns the AAC encoder MFT. Setup follows the standard MF encoder MFT flow:
    /// MFStartup → MFTEnumEx(MFT_CATEGORY_AUDIO_ENCODER, MFAudioFormat_AAC) → set
    /// output type (AAC + target byte rate) then input type (PCM) → feed samples via
    /// ProcessInput and drain ProcessOutput. The MF AAC MFT emits raw AAC (no ADTS),
    /// which is exactly what FLV/RTMP needs.
    pub struct MfAacMft {
        transform: IMFTransform,
        sample_rate: u32,
        channels: u8,
        bitrate_bps: u32,
        /// Running input timestamp in 100-ns units, as MF expects on each sample.
        time_100ns: i64,
        started: bool,
    }

    impl MfAacMft {
        pub fn new(sample_rate: u32, channels: u8, bitrate_bps: u32) -> Result<Self> {
            if sample_rate != 44100 && sample_rate != 48000 {
                // MF AAC encoder supports 44.1k/48k; resample upstream if needed.
                return Err(Error::audio(format!(
                    "MF AAC requires 44100 or 48000 Hz input (got {sample_rate}); resample first"
                )));
            }
            unsafe {
                MFStartup(MF_VERSION, MFSTARTUP_LITE)
                    .map_err(|e| Error::audio(format!("MFStartup: {e}")))?;
                let transform = create_aac_encoder_mft()?;
                // Output type (AAC) must be set before the input type (PCM).
                configure_output_type(&transform, sample_rate, channels, bitrate_bps)?;
                configure_input_type(&transform, sample_rate, channels)?;
                Ok(Self {
                    transform,
                    sample_rate,
                    channels,
                    bitrate_bps,
                    time_100ns: 0,
                    started: false,
                })
            }
        }

        /// Feed f32 interleaved samples; return any complete raw-AAC frames.
        pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>> {
            if samples.is_empty() {
                return Ok(Vec::new());
            }
            unsafe {
                if !self.started {
                    let _ = self
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                    self.started = true;
                }

                // f32 [-1,1] interleaved → s16 LE interleaved (what MF PCM expects).
                let mut pcm = Vec::with_capacity(samples.len() * 2);
                for &s in samples {
                    let clamped = s.clamp(-1.0, 1.0);
                    let v = (clamped * i16::MAX as f32) as i16;
                    pcm.extend_from_slice(&v.to_le_bytes());
                }

                // Duration of this chunk in 100-ns units, for sample timestamps.
                let frames = (samples.len() / self.channels.max(1) as usize) as i64;
                let duration_100ns = frames * 10_000_000 / self.sample_rate as i64;

                let sample = wrap_pcm_sample(&pcm, self.time_100ns, duration_100ns)?;
                self.time_100ns += duration_100ns;

                // Push input; ProcessInput can reject with "not accepting" if output
                // must be drained first, so drain, then retry once.
                let mut out = Vec::new();
                if self.transform.ProcessInput(0, &sample, 0).is_err() {
                    self.drain_output(&mut out)?;
                    self.transform
                        .ProcessInput(0, &sample, 0)
                        .map_err(|e| Error::audio(format!("AAC ProcessInput: {e}")))?;
                }
                self.drain_output(&mut out)?;
                Ok(out)
            }
        }

        /// Drain all currently available raw-AAC frames from the MFT output.
        unsafe fn drain_output(&mut self, out: &mut Vec<Vec<u8>>) -> Result<()> {
            // The AAC encoder provides its own output samples when the stream info
            // flag is set; otherwise we allocate one sized to the stream info.
            let mut info = MFT_OUTPUT_STREAM_INFO::default();
            self.transform
                .GetOutputStreamInfo(0, &mut info)
                .map_err(|e| Error::audio(format!("GetOutputStreamInfo: {e}")))?;
            let provides_samples = (info.dwFlags
                & (windows::Win32::Media::MediaFoundation::MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                    as u32))
                != 0;

            loop {
                let mut status: u32 = 0;
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
                if !provides_samples {
                    let sample = MFCreateSample()
                        .map_err(|e| Error::audio(format!("MFCreateSample(out): {e}")))?;
                    let buffer = MFCreateMemoryBuffer(info.cbSize.max(1))
                        .map_err(|e| Error::audio(format!("MFCreateMemoryBuffer(out): {e}")))?;
                    sample
                        .AddBuffer(&buffer)
                        .map_err(|e| Error::audio(format!("AddBuffer(out): {e}")))?;
                    buffers[0].pSample = std::mem::ManuallyDrop::new(Some(sample));
                }

                match self.transform.ProcessOutput(0, &mut buffers, &mut status) {
                    Ok(()) => {}
                    Err(e) if e.code().0 == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                        break;
                    }
                    Err(e) => {
                        let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                        return Err(Error::audio(format!("AAC ProcessOutput: {e}")));
                    }
                }

                let produced = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                if let Some(sample) = produced {
                    if let Some(frame) = copy_sample_bytes(&sample)? {
                        if !frame.is_empty() {
                            out.push(frame);
                        }
                    }
                }
            }
            Ok(())
        }
    }

    impl Drop for MfAacMft {
        fn drop(&mut self) {
            unsafe {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            }
            let _ = (self.bitrate_bps, self.channels, self.sample_rate);
        }
    }

    /// Create the system AAC encoder MFT via MFTEnumEx.
    unsafe fn create_aac_encoder_mft() -> Result<IMFTransform> {
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_AAC,
        };
        let mut activate = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output),
            &mut activate,
            &mut count,
        )
        .map_err(|e| Error::audio(format!("MFTEnumEx(AAC): {e}")))?;
        if count == 0 || activate.is_null() {
            return Err(Error::audio("no AAC encoder MFT available"));
        }
        let activates = std::slice::from_raw_parts(activate, count as usize);
        let first = activates[0]
            .as_ref()
            .ok_or_else(|| Error::audio("null AAC MFT activate"))?;
        first
            .ActivateObject::<IMFTransform>()
            .map_err(|e| Error::audio(format!("ActivateObject(AAC): {e}")))
    }

    /// Configure the encoder's AAC output type (set before the PCM input type).
    unsafe fn configure_output_type(
        transform: &IMFTransform,
        sample_rate: u32,
        channels: u8,
        bitrate_bps: u32,
    ) -> Result<()> {
        let t: IMFMediaType =
            MFCreateMediaType().map_err(|e| Error::audio(format!("MFCreateMediaType(out): {e}")))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .and_then(|_| t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC))
            .and_then(|_| t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, PCM_BITS_PER_SAMPLE))
            .and_then(|_| t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate))
            .and_then(|_| t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32))
            // MF wants the AAC average BYTES per second, not bits.
            .and_then(|_| {
                t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate_bps / 8)
            })
            // Payload type 0 = raw AAC (no ADTS/LATM), matching FLV/RTMP.
            .and_then(|_| t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0))
            .map_err(|e| Error::audio(format!("set AAC output type: {e}")))?;
        transform
            .SetOutputType(0, &t, 0)
            .map_err(|e| Error::audio(format!("SetOutputType(AAC): {e}")))
    }

    /// Configure the encoder's PCM input type.
    unsafe fn configure_input_type(
        transform: &IMFTransform,
        sample_rate: u32,
        channels: u8,
    ) -> Result<()> {
        let t: IMFMediaType =
            MFCreateMediaType().map_err(|e| Error::audio(format!("MFCreateMediaType(in): {e}")))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .and_then(|_| t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM))
            .and_then(|_| t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, PCM_BITS_PER_SAMPLE))
            .and_then(|_| t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate))
            .and_then(|_| t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32))
            .map_err(|e| Error::audio(format!("set PCM input type: {e}")))?;
        transform
            .SetInputType(0, &t, 0)
            .map_err(|e| Error::audio(format!("SetInputType(PCM): {e}")))
    }

    /// Wrap interleaved s16 PCM bytes in an `IMFSample` with a timestamp + duration.
    unsafe fn wrap_pcm_sample(pcm: &[u8], time_100ns: i64, duration_100ns: i64) -> Result<IMFSample> {
        let buffer = MFCreateMemoryBuffer(pcm.len() as u32)
            .map_err(|e| Error::audio(format!("MFCreateMemoryBuffer(in): {e}")))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        buffer
            .Lock(&mut ptr, Some(&mut max_len), None)
            .map_err(|e| Error::audio(format!("buffer Lock(in): {e}")))?;
        std::ptr::copy_nonoverlapping(pcm.as_ptr(), ptr, pcm.len());
        let _ = buffer.SetCurrentLength(pcm.len() as u32);
        let _ = buffer.Unlock();

        let sample =
            MFCreateSample().map_err(|e| Error::audio(format!("MFCreateSample(in): {e}")))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| Error::audio(format!("AddBuffer(in): {e}")))?;
        let _ = sample.SetSampleTime(time_100ns);
        let _ = sample.SetSampleDuration(duration_100ns);
        Ok(sample)
    }

    /// Copy the full byte contents of a sample's first buffer into a Vec.
    unsafe fn copy_sample_bytes(sample: &IMFSample) -> Result<Option<Vec<u8>>> {
        let buffer = match sample.GetBufferByIndex(0) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len: u32 = 0;
        buffer
            .Lock(&mut ptr, None, Some(&mut cur_len))
            .map_err(|e| Error::audio(format!("buffer Lock(out): {e}")))?;
        let bytes = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
        let _ = buffer.Unlock();
        Ok(Some(bytes))
    }
}


