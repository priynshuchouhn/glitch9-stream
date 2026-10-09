//! Non-Windows stub for the facecam compositor so the workspace builds on macOS.
//! All methods are no-ops / unsupported; the real implementation is Windows-only.

use crate::types::GpuTextureFrame;
use crate::D3DContext;
use g9_core::{Error, Result};

/// Facecam video codec (mirrors the Windows enum so cross-platform callers compile).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacecamCodec {
    H264,
    Vp8,
}

/// Stub facecam compositor (non-Windows). Never constructs successfully.
pub struct FacecamCompositor;

impl FacecamCompositor {
    pub fn new(
        _ctx: &D3DContext,
        _w: u32,
        _h: u32,
        _position: &str,
        _shape: &str,
        _has_facecam: bool,
    ) -> Result<Self> {
        Err(Error::capture("facecam compositor is Windows-only"))
    }
    pub fn update_camera(&mut self, _codec: FacecamCodec, _data: &[u8]) -> Result<()> {
        Err(Error::capture("facecam compositor is Windows-only"))
    }
    pub fn clear_camera(&mut self) {}
    pub fn composite(&mut self, _game: &GpuTextureFrame) -> Result<Option<GpuTextureFrame>> {
        Err(Error::capture("facecam compositor is Windows-only"))
    }
}
