//! Windows broadcast compositor: adds the Glitch9 wordmark to every broadcast and,
//! when configured, decodes the player's camera and blends it into the game frame.
//!
//! [PENDING-HW] Compiles on `x86_64-pc-windows-msvc`; runs on a GPU. Decode uses the
//! Media Foundation H.264 decoder MFT producing NV12 samples; the composite uses the
//! same D3D11 Video Processor family as the NV12 converter so the camera (NV12) is
//! blended into the BGRA game texture entirely on the GPU (no CPU pixel readback).
//!
//! Facecam placement: bottom-right corner, ~22% of the game width, 16:9. The game
//! texture is a render target (DXGI duplication textures are not), so we blend into
//! a transient BGRA render-target copy only when a camera frame is present; when no
//! camera frame has arrived yet the game frame passes through untouched.

use crate::types::GpuTextureFrame;
use crate::D3DContext;
use g9_core::{Error, Result};

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::facecam_decode::H264Decoder;
use crate::facecam_vp8::Vp8Decoder;

/// Which video codec the facecam WHEP track negotiated. Browsers that cannot send
/// WebRTC H.264 (Brave, Firefox without OpenH264) publish VP8; the compositor
/// decodes each with the matching decoder into the same NV12 texture contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacecamCodec {
    H264,
    Vp8,
}

/// Fraction of the game width the facecam overlay occupies.
const OVERLAY_WIDTH_FRACTION: f32 = 0.22;
/// Margin from the edges, as a fraction of the game width.
const OVERLAY_MARGIN_FRACTION: f32 = 0.02;
/// Width of the unobtrusive stream wordmark relative to the output frame.
const WORDMARK_WIDTH_FRACTION: f32 = 0.105;
const WORDMARK_WIDTH: u32 = 306;
const WORDMARK_HEIGHT: u32 = 96;
const WORDMARK_BGRA: &[u8] = include_bytes!("../assets/glitch9-wordmark.bgra");

pub struct FacecamCompositor {
    device: ID3D11Device,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    width: u32,
    height: u32,
    /// The engine device, used to lazily build the matching decoder on first frame.
    cam_device: ID3D11Device,
    /// H.264 decoder (Media Foundation), built on first H.264 frame.
    h264: Option<H264Decoder>,
    /// VP8 decoder (libvpx), built on first VP8 frame.
    vp8: Option<Vp8Decoder>,
    /// Latest decoded camera frame (NV12 texture), if any has arrived.
    cam_nv12: Option<ID3D11Texture2D>,
    cam_w: u32,
    cam_h: u32,
    /// Reusable BGRA render target containing game + facecam. Desktop duplication
    /// textures are input-only and must never be used as a VP output surface.
    composite_texture: ID3D11Texture2D,
    wordmark_texture: ID3D11Texture2D,
    wordmark_w: u32,
    wordmark_h: u32,
    avoid_top_right: bool,
    position: FacecamPosition,
    shape: FacecamShape,
}

#[derive(Debug, Clone, Copy)]
enum FacecamPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl FacecamPosition {
    fn parse(value: &str) -> Self {
        match value {
            "top-left" => Self::TopLeft,
            "top-right" => Self::TopRight,
            "bottom-left" => Self::BottomLeft,
            _ => Self::BottomRight,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FacecamShape {
    Landscape,
    Square,
    Portrait,
}

impl FacecamShape {
    fn parse(value: &str) -> Self {
        match value {
            "square" => Self::Square,
            "portrait" => Self::Portrait,
            _ => Self::Landscape,
        }
    }
}

impl FacecamCompositor {
    /// Build a compositor targeting a `width`x`height` game frame on `ctx`'s device.
    pub fn new(
        ctx: &D3DContext,
        width: u32,
        height: u32,
        position: &str,
        shape: &str,
        has_facecam: bool,
    ) -> Result<Self> {
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
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 30,
                    Denominator: 1,
                },
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 30,
                    Denominator: 1,
                },
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

            let output_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut composite_texture = None;
            device
                .CreateTexture2D(&output_desc, None, Some(&mut composite_texture))
                .map_err(|e| Error::capture(format!("facecam composite texture: {e}")))?;
            let composite_texture = composite_texture
                .ok_or_else(|| Error::capture("null facecam composite texture"))?;

            let wordmark_desc = D3D11_TEXTURE2D_DESC {
                Width: WORDMARK_WIDTH,
                Height: WORDMARK_HEIGHT,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: 0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut wordmark_texture = None;
            device
                .CreateTexture2D(&wordmark_desc, None, Some(&mut wordmark_texture))
                .map_err(|e| Error::capture(format!("wordmark texture: {e}")))?;
            let wordmark_texture =
                wordmark_texture.ok_or_else(|| Error::capture("null wordmark texture"))?;
            context.UpdateSubresource(
                &wordmark_texture,
                0,
                None,
                WORDMARK_BGRA.as_ptr().cast(),
                WORDMARK_WIDTH * 4,
                0,
            );

            Ok(Self {
                device: device.clone(),
                video_device,
                video_context,
                enumerator,
                processor,
                width,
                height,
                cam_device: device,
                h264: None,
                vp8: None,
                cam_nv12: None,
                cam_w: 0,
                cam_h: 0,
                composite_texture,
                wordmark_texture,
                wordmark_w: WORDMARK_WIDTH,
                wordmark_h: WORDMARK_HEIGHT,
                // Keep the brand clear of a top-right camera; otherwise use the
                // conventional top-right broadcast-watermark position.
                avoid_top_right: has_facecam
                    && matches!(FacecamPosition::parse(position), FacecamPosition::TopRight),
                position: FacecamPosition::parse(position),
                shape: FacecamShape::parse(shape),
            })
        }
    }

    /// Feed one camera access unit for the given codec, decoding with the matching
    /// decoder (H.264 via Media Foundation, VP8 via libvpx). Updates the latest
    /// decoded NV12 camera texture when the decoder produces an output frame.
    ///
    /// The decoder is built lazily on first use so a session only pays for the
    /// codec its browser actually publishes.
    pub fn update_camera(&mut self, codec: FacecamCodec, data: &[u8]) -> Result<()> {
        let decoded = match codec {
            FacecamCodec::H264 => {
                if self.h264.is_none() {
                    self.h264 = Some(H264Decoder::new(&self.cam_device)?);
                }
                self.h264
                    .as_mut()
                    .expect("h264 decoder present")
                    .decode(data)?
            }
            FacecamCodec::Vp8 => {
                if self.vp8.is_none() {
                    self.vp8 = Some(Vp8Decoder::new(&self.cam_device)?);
                }
                self.vp8
                    .as_mut()
                    .expect("vp8 decoder present")
                    .decode(data)?
            }
        };
        if let Some((tex, w, h)) = decoded {
            if self.cam_nv12.is_none() {
                tracing::info!(
                    "facecam: first camera frame decoded ({codec:?}) {w}x{h}; compositing"
                );
            }
            self.cam_nv12 = Some(tex);
            self.cam_w = w;
            self.cam_h = h;
        }
        Ok(())
    }

    /// Remove the old publication's decoded frame and codec state. This prevents
    /// a frozen facecam when the user switches the camera off and lets a later
    /// VP8/H.264 publication initialize cleanly from a new keyframe.
    pub fn clear_camera(&mut self) {
        self.cam_nv12 = None;
        self.cam_w = 0;
        self.cam_h = 0;
        self.h264 = None;
        self.vp8 = None;
    }

    /// Blend game + latest camera into a separate render-target texture. DXGI
    /// desktop-duplication textures cannot be VP output surfaces, so the caller
    /// must use the returned frame for conversion/encoding.
    pub fn composite(&mut self, game: &GpuTextureFrame) -> Result<Option<GpuTextureFrame>> {
        let cam = self.cam_nv12.clone();
        let camera_visible = cam.is_some();
        let game_tex = game
            .texture()
            .ok_or_else(|| Error::capture("game frame has no texture"))?;

        unsafe {
            // Output view over our render target, never over the captured desktop.
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            self.video_device
                .CreateVideoProcessorOutputView(
                    &self.composite_texture,
                    &self.enumerator,
                    &out_desc,
                    Some(&mut output_view),
                )
                .map_err(|e| Error::capture(format!("VP output view: {e}")))?;
            let output_view = output_view.ok_or_else(|| Error::capture("null VP output view"))?;

            // Input view 0: full-size BGRA game frame.
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut game_view = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    game_tex,
                    &self.enumerator,
                    &in_desc,
                    Some(&mut game_view),
                )
                .map_err(|e| Error::capture(format!("VP game input view: {e}")))?;
            let game_view = game_view.ok_or_else(|| Error::capture("null VP game input view"))?;

            // Destination rectangle in the configured corner and aspect shape.
            let margin = (self.width as f32 * OVERLAY_MARGIN_FRACTION) as i32;
            let (width_fraction, aspect) = match self.shape {
                FacecamShape::Landscape => (OVERLAY_WIDTH_FRACTION, 16.0 / 9.0),
                FacecamShape::Square => (0.18, 1.0),
                FacecamShape::Portrait => (0.13, 9.0 / 16.0),
            };
            let ow = (self.width as f32 * width_fraction) as i32;
            let oh = (ow as f32 / aspect) as i32;
            let (left, top) = match self.position {
                FacecamPosition::TopLeft => (margin, margin),
                FacecamPosition::TopRight => (self.width as i32 - margin - ow, margin),
                FacecamPosition::BottomLeft => (margin, self.height as i32 - margin - oh),
                FacecamPosition::BottomRight => (
                    self.width as i32 - margin - ow,
                    self.height as i32 - margin - oh,
                ),
            };
            let dest = RECT {
                left,
                top,
                right: left + ow,
                bottom: top + oh,
            };

            // Center-crop the source so square/portrait modes do not stretch faces.
            let cam_aspect = self.cam_w as f32 / self.cam_h.max(1) as f32;
            let (source_w, source_h) = if cam_aspect > aspect {
                ((self.cam_h as f32 * aspect) as i32, self.cam_h as i32)
            } else {
                (self.cam_w as i32, (self.cam_w as f32 / aspect) as i32)
            };
            let source_left = (self.cam_w as i32 - source_w) / 2;
            let source_top = (self.cam_h as i32 - source_h) / 2;
            let source = RECT {
                left: source_left,
                top: source_top,
                right: source_left + source_w,
                bottom: source_top + source_h,
            };

            let full = RECT {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            };
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&full),
            );
            let mut streams = vec![D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(game_view)),
                ..Default::default()
            }];

            if let Some(cam) = cam {
                let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
                self.video_device
                    .CreateVideoProcessorInputView(
                        &cam,
                        &self.enumerator,
                        &in_desc,
                        Some(&mut input_view),
                    )
                    .map_err(|e| Error::capture(format!("VP camera input view: {e}")))?;
                let input_view =
                    input_view.ok_or_else(|| Error::capture("null VP camera input view"))?;
                let stream_index = streams.len() as u32;
                self.video_context.VideoProcessorSetStreamDestRect(
                    &self.processor,
                    stream_index,
                    true,
                    Some(&dest),
                );
                self.video_context.VideoProcessorSetStreamSourceRect(
                    &self.processor,
                    stream_index,
                    true,
                    Some(&source),
                );
                self.video_context.VideoProcessorSetStreamAlpha(
                    &self.processor,
                    stream_index,
                    true,
                    1.0,
                );
                streams.push(D3D11_VIDEO_PROCESSOR_STREAM {
                    Enable: true.into(),
                    pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
                    ..Default::default()
                });
            }

            let mut wordmark_view = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    &self.wordmark_texture,
                    &self.enumerator,
                    &in_desc,
                    Some(&mut wordmark_view),
                )
                .map_err(|e| Error::capture(format!("VP wordmark input view: {e}")))?;
            let wordmark_view =
                wordmark_view.ok_or_else(|| Error::capture("null VP wordmark input view"))?;
            let logo_index = streams.len() as u32;
            let logo_w = (self.width as f32 * WORDMARK_WIDTH_FRACTION) as i32;
            let logo_h = (logo_w as f32 * self.wordmark_h as f32 / self.wordmark_w as f32) as i32;
            let logo_left = if self.avoid_top_right && camera_visible {
                margin
            } else {
                self.width as i32 - margin - logo_w
            };
            let logo_dest = RECT {
                left: logo_left,
                top: margin,
                right: logo_left + logo_w,
                bottom: margin + logo_h,
            };
            let logo_source = RECT {
                left: 0,
                top: 0,
                right: self.wordmark_w as i32,
                bottom: self.wordmark_h as i32,
            };
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                logo_index,
                true,
                Some(&logo_dest),
            );
            self.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                logo_index,
                true,
                Some(&logo_source),
            );
            self.video_context.VideoProcessorSetStreamAlpha(
                &self.processor,
                logo_index,
                true,
                0.92,
            );
            streams.push(D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(wordmark_view)),
                ..Default::default()
            });
            self.video_context
                .VideoProcessorBlt(&self.processor, &output_view, 0, &streams)
                .map_err(|e| Error::capture(format!("VideoProcessorBlt: {e}")))?;
        }
        Ok(Some(GpuTextureFrame::from_texture(
            self.composite_texture.clone(),
            self.width,
            self.height,
        )))
    }
}
