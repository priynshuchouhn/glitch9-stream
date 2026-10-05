//! WASAPI loopback capture → interleaved f32 PCM.
//!
//! [PENDING-HW] Windows-only. Captures the system render endpoint in **loopback**
//! mode (what the game is playing), which is the right source for a game stream.
//! One capture feeds both the Opus (WebRTC) and AAC (YouTube) encoders — audio is
//! captured once (spec §18/§24). Produces `PcmChunk`s of interleaved f32 at the
//! device mix rate; the encoders resample/handle format as needed.

use crate::PcmChunk;
use g9_core::{Error, Result};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

pub struct WasapiCapture {
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    sample_rate: u32,
    channels: u8,
    start: Instant,
    started: bool,
}

impl WasapiCapture {
    /// Open the default render endpoint in loopback mode. `sample_rate`/`channels`
    /// are the desired values but WASAPI shared mode uses the device mix format, so
    /// the actual values are read back and reported in each `PcmChunk`.
    pub fn new(_desired_sr: u32, _desired_ch: u8) -> Result<Self> {
        unsafe {
            // COM init (multithreaded; harmless if already initialized on this thread).
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| Error::audio(format!("MMDeviceEnumerator: {e}")))?;
            // Render endpoint (we capture its loopback).
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| Error::audio(format!("GetDefaultAudioEndpoint: {e}")))?;

            let audio_client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| Error::audio(format!("Activate IAudioClient: {e}")))?;

            // Device mix format (shared mode must use it).
            let mix_ptr = audio_client
                .GetMixFormat()
                .map_err(|e| Error::audio(format!("GetMixFormat: {e}")))?;
            let mix = &*mix_ptr;
            let sample_rate = mix.nSamplesPerSec;
            let channels = mix.nChannels as u8;

            // 100ms buffer (REFERENCE_TIME is 100ns units).
            let buffer_duration: i64 = 1_000_000; // 100 ms
            audio_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    buffer_duration,
                    0,
                    mix_ptr as *const WAVEFORMATEX,
                    None,
                )
                .map_err(|e| Error::audio(format!("IAudioClient.Initialize(loopback): {e}")))?;

            let capture_client: IAudioCaptureClient = audio_client
                .GetService()
                .map_err(|e| Error::audio(format!("GetService IAudioCaptureClient: {e}")))?;

            Ok(Self {
                audio_client,
                capture_client,
                sample_rate,
                channels,
                start: Instant::now(),
                started: false,
            })
        }
    }

    fn ensure_started(&mut self) -> Result<()> {
        if !self.started {
            unsafe {
                self.audio_client
                    .Start()
                    .map_err(|e| Error::audio(format!("IAudioClient.Start: {e}")))?;
            }
            self.start = Instant::now();
            self.started = true;
        }
        Ok(())
    }

    /// Read any available captured audio as interleaved f32. Returns `Ok(None)` when
    /// no packet is ready. The caller polls this on the audio thread.
    pub fn read(&mut self) -> Result<Option<PcmChunk>> {
        self.ensure_started()?;
        unsafe {
            let mut packet_len = self
                .capture_client
                .GetNextPacketSize()
                .map_err(|e| Error::audio(format!("GetNextPacketSize: {e}")))?;
            if packet_len == 0 {
                return Ok(None);
            }

            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;
            self.capture_client
                .GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)
                .map_err(|e| Error::audio(format!("GetBuffer: {e}")))?;

            let ch = self.channels as usize;
            let total = num_frames as usize * ch;
            // WASAPI shared mix format is 32-bit float.
            let samples: Vec<f32> = if data_ptr.is_null() || total == 0 {
                vec![0.0; total] // AUDCLNT_BUFFERFLAGS_SILENT → emit silence
            } else {
                let slice = std::slice::from_raw_parts(data_ptr as *const f32, total);
                slice.to_vec()
            };

            self.capture_client
                .ReleaseBuffer(num_frames)
                .map_err(|e| Error::audio(format!("ReleaseBuffer: {e}")))?;

            // keep packet_len referenced for clarity
            let _ = &mut packet_len;

            Ok(Some(PcmChunk {
                samples,
                sample_rate: self.sample_rate,
                channels: self.channels,
                pts: self.start.elapsed(),
            }))
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> u8 {
        self.channels
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                let _ = self.audio_client.Stop();
            }
        }
    }
}

// Keep Duration import used.
#[allow(dead_code)]
fn _d() -> Duration {
    Duration::from_millis(0)
}
