//! `g9-convert` — GPU BGRA→NV12 color conversion, GPU-resident (no CPU readback).
//! Windows-only real impl in `windows_impl`.

#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::Nv12Converter;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::Nv12Converter;
