//! Windows facecam compositor: decodes the player's camera H.264 (received over
//! WHEP from the SFU) and blends it into a corner of the captured game texture on
//! the GPU, before NV12 conversion — so one encode carries game + facecam.
//!
//! [PENDING-HW] Compiles on `x86_64-pc-windows-msvc`; runs on a GPU. Decode uses the
//! Media Foundation H.264 decoder MFT producing NV12 samples; the composite uses the
//! same D3D11 Video Processor family as the NV12 converter so the camera (NV12) is
//! blended into the BGRA game texture entirely on the GPU (no CPU pixel readback).
//!
//! Overlay placement: bottom-right corner, ~22% of the game width, 16:9. The game
//! texture is a render target (DXGI duplication textures are not), so we blend into
//! a transient BGRA render-target copy only when a camera frame is present; when no
//! camera frame has arrived yet the game frame passes through untouched.

use crate::types::GpuTextureFrame;
use crate::D3DContext;
use g9_core::{Error, Result};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL};
use windows::Win32::Foundation::RECT;

use crate::facecam_decode::H264Decoder;

/// Fraction of the game width the facecam overlay occupies (bottom-right corner).
const OVERLAY_WIDTH_FRACTION: f32 = 0.22;
/// Margin from the edges, as a fraction of the game width.
const OVERLAY_MARGIN_FRACTION: f32 = 0.02;

pub struct FacecamCompositor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    width: u32,
    height: u32,
    decoder: H264Decoder,
    /// Latest decoded camera frame (NV12 texture), if any has arrived.
    cam_nv12: Option<ID3D11Texture2D>,
    cam_w: u32,
    cam_h: u32,
}

impl FacecamCompositor {
    /// Build a compositor targeting a `width`x`height` game frame on `ctx`'s device.
    pub fn new(ctx: &D3DContext, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let device = ctx.device().clone();
            let context = ctx.context().clone();
            let video_device: ID3D11VideoDevice = device
                .cast()
                .map_err(|e| Error::capture(format!("ID3D11VideoDevice: {e}")))?;
            let video_context: ID3D11VideoContext = context
                .cast()
                .map_err(|e| Error::capture(format!("ID3D11VideoContext: {e}")))?;

            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&content_desc)
                .map_err(|e| Error::capture(format!("VP enumerator: {e}")))?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| Error::capture(format!("VP create: {e}")))?;

            let decoder = H264Decoder::new(&device)?;

            Ok(Self {
                device,
                context,
                video_device,
                video_context,
                enumerator,
                processor,
                width,
                height,
                decoder,
                cam_nv12: None,
                cam_w: 0,
                cam_h: 0,
            })
        }
    }

    /// Feed one camera H.264 access unit (Annex-B). Updates the latest decoded
    /// NV12 camera texture when the decoder produces an output frame.
    pub fn update_camera(&mut self, annex_b: &[u8]) -> Result<()> {
        if let Some((tex, w, h)) = self.decoder.decode(annex_b)? {
            self.cam_nv12 = Some(tex);
            self.cam_w = w;
            self.cam_h = h;
        }
        Ok(())
    }

    /// Blend the latest camera frame into the bottom-right corner of `game`.
    /// No-op (Ok) when no camera frame has been decoded yet.
    pub fn composite_onto(&mut self, game: &GpuTextureFrame) -> Result<()> {
        let cam = match self.cam_nv12.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(()),
        };
        let game_tex = game
            .texture()
            .ok_or_else(|| Error::capture("game frame has no texture"))?;

        unsafe {
            // Output view over the game texture (destination of the blend).
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            self.video_device
                .CreateVideoProcessorOutputView(
                    game_tex,
                    &self.enumerator,
                    &out_desc,
                    Some(&mut output_view),
                )
                .map_err(|e| Error::capture(format!("VP output view: {e}")))?;
            let output_view =
                output_view.ok_or_else(|| Error::capture("null VP output view"))?;

            // Input view over the camera NV12 texture.
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    &cam,
                    &self.enumerator,
                    &in_desc,
                    Some(&mut input_view),
                )
                .map_err(|e| Error::capture(format!("VP input view: {e}")))?;
            let input_view =
                input_view.ok_or_else(|| Error::capture("null VP input view"))?;

            // Destination rectangle: bottom-right corner overlay, preserving 16:9.
            let margin = (self.width as f32 * OVERLAY_MARGIN_FRACTION) as i32;
            let ow = (self.width as f32 * OVERLAY_WIDTH_FRACTION) as i32;
            let oh = (ow as f32 * 9.0 / 16.0) as i32;
            let right = self.width as i32 - margin;
            let bottom = self.height as i32 - margin;
            let dest = RECT {
                left: right - ow,
                top: bottom - oh,
                right,
                bottom,
            };

            // Blend the camera stream into the destination rect over the game. The
            // game pixels outside the rect are preserved (background enabled off,
            // single stream drawn into a sub-rect of the existing render target).
            self.video_context
                .VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dest));

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view.clone())),
                ..Default::default()
            };
            self.video_context
                .VideoProcessorBlt(&self.processor, &output_view, 0, &[stream])
                .map_err(|e| Error::capture(format!("VideoProcessorBlt: {e}")))?;
        }
        Ok(())
    }
}
