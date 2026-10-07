//! Non-Windows stub for the facecam compositor so the workspace builds on macOS.
//! All methods are no-ops / unsupported; the real implementation is Windows-only.

use crate::types::GpuTextureFrame;
use crate::D3DContext;
use g9_core::{Error, Result};

/// Stub facecam compositor (non-Windows). Never constructs successfully.
pub struct FacecamCompositor;

impl FacecamCompositor {
    pub fn new(_ctx: &D3DContext, _w: u32, _h: u32) -> Result<Self> {
        Err(Error::capture("facecam compositor is Windows-only"))
    }
    pub fn update_camera(&mut self, _annex_b: &[u8]) -> Result<()> {
        Err(Error::capture("facecam compositor is Windows-only"))
    }
    pub fn composite_onto(&mut self, _game: &GpuTextureFrame) -> Result<()> {
        Err(Error::capture("facecam compositor is Windows-only"))
    }
}
