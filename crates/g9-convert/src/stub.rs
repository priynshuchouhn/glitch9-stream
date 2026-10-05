//! Non-Windows stub for the NV12 converter.

use g9_capture::{D3DContext, GpuTextureFrame};
use g9_core::{Error, Result};

/// Converts a BGRA GPU texture to an NV12 GPU texture suitable for NVENC input.
pub struct Nv12Converter;

impl Nv12Converter {
    pub fn new(_width: u32, _height: u32) -> Result<Self> {
        Err(Error::Unsupported(
            "GPU color conversion is only available on Windows".into(),
        ))
    }

    /// Matches the Windows constructor signature so the pipeline compiles on all hosts.
    pub fn new_with_ctx(_ctx: &D3DContext, _width: u32, _height: u32) -> Result<Self> {
        Err(Error::Unsupported(
            "GPU color conversion is only available on Windows".into(),
        ))
    }

    /// Returns an NV12 GPU texture. On Windows this stays entirely on the GPU.
    pub fn convert(&mut self, _bgra: &GpuTextureFrame) -> Result<GpuTextureFrame> {
        Err(Error::Unsupported(
            "GPU color conversion is only available on Windows".into(),
        ))
    }
}
