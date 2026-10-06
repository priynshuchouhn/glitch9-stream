//! Non-Windows stub for the NVENC encoder. NOT a software encoder — it only exists so
//! the workspace type-checks off-Windows. x264/x265 are deliberately absent.

use g9_capture::GpuTextureFrame;
use g9_core::{EncodedFrame, EncoderProfile, Error, PtsClock, Result};

pub struct NvencEncoder;

impl NvencEncoder {
    pub fn new(_profile: EncoderProfile, _clock: PtsClock) -> Result<Self> {
        Err(Error::Unsupported(
            "NVENC is only available on Windows with an NVIDIA GPU".into(),
        ))
    }

    /// Matches the Windows constructor so the pipeline compiles on all hosts.
    pub fn new_with_ctx(
        _ctx: &g9_capture::D3DContext,
        _profile: EncoderProfile,
        _clock: PtsClock,
    ) -> Result<Self> {
        Err(Error::Unsupported(
            "NVENC is only available on Windows with an NVIDIA GPU".into(),
        ))
    }

    /// Encode one NV12 GPU texture. Returns an encoded H.264 access unit.
    pub fn encode(&mut self, _nv12: &GpuTextureFrame) -> Result<Option<EncodedFrame>> {
        Err(Error::Unsupported("NVENC is only available on Windows".into()))
    }

    /// Request the next encoded frame be an IDR (e.g. new viewer / PLI / reconnect).
    pub fn force_idr(&mut self) {}

    pub fn current_bitrate_bps(&self) -> u32 {
        0
    }

    /// Adaptive bitrate (no-op off-Windows).
    pub fn set_bitrate(&mut self, _bitrate_bps: u32) -> Result<()> {
        Ok(())
    }
}
