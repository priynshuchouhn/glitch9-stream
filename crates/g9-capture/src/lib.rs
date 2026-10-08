//! `g9-capture` — D3D11 device/adapter/display enumeration + DXGI Desktop Duplication.
//! Windows-only; implemented in `windows_impl`. See that module for the real code.

mod types;
pub use types::{AdapterInfo, DisplayInfo, GpuTextureFrame};

#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::{Capturer, D3DContext, GpuFrameCache};

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::{Capturer, D3DContext, GpuFrameCache};

// Facecam compositor: decodes the player's camera H.264 and blends it over the
// game texture before NV12 conversion. Windows-only real impl; a stub elsewhere.
#[cfg(windows)]
mod facecam;
#[cfg(windows)]
mod facecam_decode;
#[cfg(windows)]
mod facecam_vp8;
#[cfg(windows)]
pub use facecam::{FacecamCodec, FacecamCompositor};

#[cfg(not(windows))]
mod facecam_stub;
#[cfg(not(windows))]
pub use facecam_stub::{FacecamCodec, FacecamCompositor};
