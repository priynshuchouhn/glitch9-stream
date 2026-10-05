//! `g9-capture` — D3D11 device/adapter/display enumeration + DXGI Desktop Duplication.
//! Windows-only; implemented in `windows_impl`. See that module for the real code.

mod types;
pub use types::{AdapterInfo, DisplayInfo, GpuTextureFrame};

#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::{Capturer, D3DContext};

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::{Capturer, D3DContext};
