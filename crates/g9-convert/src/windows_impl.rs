//! Windows implementation: GPU BGRA → NV12 color conversion via the D3D11 Video
//! Processor (`ID3D11VideoDevice` / `ID3D11VideoContext` / `ID3D11VideoProcessor`).
//!
//! [PENDING-HW] Compiles on `x86_64-pc-windows-msvc`; runs on a GPU. Everything here
//! stays on the GPU: the input is the captured BGRA `ID3D11Texture2D`, the output is
//! an NV12 `ID3D11Texture2D` reused across frames. There is **no CPU readback** — we
//! never call `Map()` on the pixels. NVENC consumes the NV12 texture directly.
//!
//! Why the Video Processor instead of a compute shader: it is the hardware fixed-
//! function colour-space converter, handles BT.601/709 and full/limited range, and
//! is the lowest-overhead BGRA→NV12 path on NVIDIA under D3D11. (A compute-shader
//! fallback is noted in docs for GPUs without VP support; NVIDIA always has it.)

use g9_capture::{D3DContext, GpuTextureFrame};
use g9_core::{Error, Result};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL,
};

pub struct Nv12Converter {
    device: ID3D11Device,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// Reused NV12 output texture (allocated once).
    nv12_out: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    width: u32,
    height: u32,
}

impl Nv12Converter {
    /// Build a converter bound to the capture device so textures interoperate.
    pub fn new_with_ctx(ctx: &D3DContext, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let device = ctx.device().clone();
            let video_device: ID3D11VideoDevice = device
                .cast()
                .map_err(|e| Error::convert(format!("ID3D11VideoDevice: {e}")))?;
            let video_context: ID3D11VideoContext = ctx
                .context()
                .cast()
                .map_err(|e| Error::convert(format!("ID3D11VideoContext: {e}")))?;

            // Describe the conversion: progressive, input size == output size, 60fps.
            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };

            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&content_desc)
                .map_err(|e| Error::convert(format!("CreateVideoProcessorEnumerator: {e}")))?;

            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| Error::convert(format!("CreateVideoProcessor: {e}")))?;

            // Allocate the NV12 output texture once; reused every frame.
            let nv12_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0, // no CPU access — proves there is no readback
                MiscFlags: 0,
            };
            let mut nv12_out: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&nv12_desc, None, Some(&mut nv12_out))
                .map_err(|e| Error::convert(format!("CreateTexture2D(NV12): {e}")))?;
            let nv12_out = nv12_out.ok_or_else(|| Error::convert("null NV12 texture"))?;

            // Output view onto the NV12 texture.
            let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPOV {
                        MipSlice: 0,
                    },
                },
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            video_device
                .CreateVideoProcessorOutputView(&nv12_out, &enumerator, &ov_desc, Some(&mut output_view))
                .map_err(|e| Error::convert(format!("CreateVideoProcessorOutputView: {e}")))?;
            let output_view = output_view.ok_or_else(|| Error::convert("null output view"))?;

            Ok(Self {
                device,
                video_device,
                video_context,
                enumerator,
                processor,
                nv12_out,
                output_view,
                width,
                height,
            })
        }
    }

    /// Compatibility constructor (used by the stub signature). Prefer `new_with_ctx`.
    pub fn new(_width: u32, _height: u32) -> Result<Self> {
        Err(Error::convert(
            "use Nv12Converter::new_with_ctx(ctx, w, h) on Windows",
        ))
    }

    /// Convert a BGRA capture texture to NV12 on the GPU. Returns a `GpuTextureFrame`
    /// wrapping the reused NV12 texture. No CPU copy of pixels occurs.
    pub fn convert(&mut self, bgra: &GpuTextureFrame) -> Result<GpuTextureFrame> {
        unsafe {
            let input_tex = bgra
                .texture()
                .ok_or_else(|| Error::convert("input frame has no GPU texture"))?;

            // Create an input view onto the BGRA texture for this frame.
            let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0, // use the texture's own format (BGRA)
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device
                .CreateVideoProcessorInputView(input_tex, &self.enumerator, &iv_desc, Some(&mut input_view))
                .map_err(|e| Error::convert(format!("CreateVideoProcessorInputView: {e}")))?;
            let input_view = input_view.ok_or_else(|| Error::convert("null input view"))?;

            // One input stream, enabled, pointing at the BGRA input view.
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view.clone())),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };

            // GPU does the colour conversion straight into the NV12 output texture.
            self.video_context
                .VideoProcessorBlt(&self.processor, &self.output_view, 0, &[stream])
                .map_err(|e| Error::convert(format!("VideoProcessorBlt: {e}")))?;

            // Hand back the NV12 texture (shared handle). NVENC registers this directly.
            Ok(GpuTextureFrame::from_texture(
                self.nv12_out.clone(),
                self.width,
                self.height,
            ))
        }
    }
}

// Keep imports referenced for clarity / future use.
#[allow(dead_code)]
fn _formats() -> (
    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) {
    (DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12)
}
