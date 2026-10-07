//! Non-Windows stub. Lets the workspace type-check on macOS/Linux. Every entry point
//! returns `Error::Unsupported` at runtime — this is NOT a software fallback, it just
//! makes the crate compile off-Windows so the rest of the engine can be developed.

use crate::types::{AdapterInfo, DisplayInfo, GpuTextureFrame};
use g9_core::{Error, Result};

pub struct D3DContext;

impl D3DContext {
    pub fn new(_preferred_adapter: Option<u32>) -> Result<Self> {
        Err(Error::Unsupported(
            "D3D11 is only available on Windows".into(),
        ))
    }

    pub fn enumerate_adapters() -> Result<Vec<AdapterInfo>> {
        Err(Error::Unsupported("DXGI is only available on Windows".into()))
    }

    pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
        Err(Error::Unsupported("DXGI is only available on Windows".into()))
    }
}

pub struct Capturer;

impl Capturer {
    pub fn new(_ctx: &D3DContext, _display_index: u32) -> Result<Self> {
        Err(Error::Unsupported(
            "DXGI Desktop Duplication is only available on Windows".into(),
        ))
    }

    pub fn acquire_frame(&mut self, _timeout_ms: u32) -> Result<Option<GpuTextureFrame>> {
        Err(Error::Unsupported("capture is only available on Windows".into()))
    }

    pub fn dump_one_frame(&mut self, _ctx: &D3DContext, _path: &str) -> Result<(u32, u32)> {
        Err(Error::Unsupported("capture is only available on Windows".into()))
    }
}

pub struct GpuFrameCache;

impl GpuFrameCache {
    pub fn new() -> Self {
        Self
    }

    pub fn update(&mut self, _ctx: &D3DContext, _frame: &GpuTextureFrame) -> Result<()> {
        Err(Error::Unsupported(
            "GPU frame caching is only available on Windows".into(),
        ))
    }

    pub fn latest(&self) -> Option<GpuTextureFrame> {
        None
    }
}
