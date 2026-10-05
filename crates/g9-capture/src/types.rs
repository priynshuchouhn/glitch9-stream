//! Platform-independent capture types (so `g9-stream` can talk about adapters/displays
//! without pulling in Windows headers).

/// A GPU adapter discovered via DXGI.
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub index: u32,
    pub description: String,
    pub dedicated_vram_mb: u64,
    pub is_nvidia: bool,
    /// D3D feature level as a human string, e.g. "11_1".
    pub feature_level: String,
}

/// A display output attached to an adapter.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub index: u32,
    pub adapter_index: u32,
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub is_attached: bool,
}

/// A captured frame that lives on the GPU as a D3D11 texture.
///
/// On Windows this wraps an `ID3D11Texture2D` (BGRA). The texture stays on the GPU;
/// the conversion stage consumes it directly — there is no CPU copy of the pixels.
/// The opaque handle is only meaningful inside the Windows implementation.
pub struct GpuTextureFrame {
    pub width: u32,
    pub height: u32,
    /// Monotonic capture time (QPC-derived on Windows).
    pub acquired_at: std::time::Instant,
    /// The underlying `ID3D11Texture2D` (Windows only). Non-Windows builds never
    /// populate this. The convert/encode crates read it via `texture()`.
    #[cfg(windows)]
    pub(crate) texture: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
}

#[cfg(windows)]
impl GpuTextureFrame {
    /// Construct a frame from an existing GPU texture (used by the converter to
    /// hand its NV12 output back into the pipeline as a `GpuTextureFrame`).
    pub fn from_texture(
        texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            width,
            height,
            acquired_at: std::time::Instant::now(),
            texture: Some(texture),
        }
    }

    /// Borrow the underlying D3D11 texture. The pixels stay on the GPU.
    pub fn texture(&self) -> Option<&windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
        self.texture.as_ref()
    }
}
