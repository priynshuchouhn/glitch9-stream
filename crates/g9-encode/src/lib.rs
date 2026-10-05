//! `g9-encode` — NVENC H.264 encoder taking a registered D3D11 NV12 texture as input
//! (zero-copy). Windows+NVIDIA only.

// H.264 bitstream helpers live in g9-core (shared with the transports).
pub use g9_core::h264::{self, extract_parameter_sets};

#[cfg(windows)]
mod nvenc_ffi;
#[cfg(windows)]
mod nvenc;
#[cfg(windows)]
pub use nvenc::NvencEncoder;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::NvencEncoder;
