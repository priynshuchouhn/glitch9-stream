//! Software VP8 decoder for the facecam, producing an NV12 `ID3D11Texture2D` the
//! existing `FacecamCompositor` can blend without changing its composite path.
//!
//! [PENDING-HW] Compiles on `x86_64-pc-windows-msvc`; runs on the engine host.
//! Browsers that cannot send WebRTC H.264 (notably Brave and Firefox without
//! OpenH264) publish the camera as **VP8**, which the Media Foundation H.264
//! decoder cannot handle. libvpx decodes VP8 on the CPU (webcam-sized, so the cost
//! is small — a few percent of one core) into I420; we convert I420 -> NV12 and
//! upload it to a reusable D3D11 NV12 texture on the engine's device, so the
//! D3D11 Video Processor composites it exactly like the MF-decoded H.264 path.
//!
//! libvpx is linked as a prebuilt static library via `env-libvpx-sys` (the build
//! sets `VPX_LIB_DIR` / `VPX_INCLUDE_DIR` / `VPX_VERSION` / `VPX_STATIC`), so the
//! engine build needs no libvpx source build toolchain (perl/nasm) on the host.

use g9_core::{Error, Result};
use std::ptr;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};

// The package is `env-libvpx-sys` but its library crate is named `vpx_sys`.
use vpx_sys::{
    vpx_codec_ctx_t, vpx_codec_dec_init_ver, vpx_codec_decode, vpx_codec_err_t,
    vpx_codec_get_frame, vpx_codec_iter_t, vpx_codec_vp8_dx, vpx_image_t, vpx_img_fmt,
    VPX_DECODER_ABI_VERSION,
};

/// libvpx realtime decode deadline (`VPX_DL_REALTIME` == 1): decode immediately
/// without extra post-processing, matching a low-latency live facecam.
const VPX_DL_REALTIME: std::os::raw::c_long = 1;
/// Success code from libvpx. `vpx_codec_err_t` is a `#[repr(u32)]` enum whose OK
/// variant is 0; comparing against this constant keeps the call sites readable.
const VPX_CODEC_OK: vpx_codec_err_t = vpx_codec_err_t::VPX_CODEC_OK;

/// Decodes VP8 frames into NV12 D3D11 textures via libvpx + a CPU I420->NV12 upload.
pub struct Vp8Decoder {
    device: ID3D11Device,
    ctx: vpx_codec_ctx_t,
    initialized: bool,
    /// Reusable NV12 texture sized to the current camera resolution; recreated
    /// only when the camera dimensions change.
    texture: Option<ID3D11Texture2D>,
    tex_w: u32,
    tex_h: u32,
    /// Scratch NV12 buffer (Y plane + interleaved UV) reused across frames.
    nv12: Vec<u8>,
}

impl Vp8Decoder {
    /// Initialize a VP8 decoder bound to `device` for NV12 texture uploads.
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            let mut ctx: vpx_codec_ctx_t = std::mem::zeroed();
            let iface = vpx_codec_vp8_dx();
            if iface.is_null() {
                return Err(Error::capture("libvpx: null VP8 decoder interface"));
            }
            // No custom config (let libvpx infer dimensions from the stream), no
            // special flags. ABI version must match the headers we bound against.
            let err = vpx_codec_dec_init_ver(
                &mut ctx,
                iface,
                ptr::null(),
                0,
                VPX_DECODER_ABI_VERSION as i32,
            );
            if err != VPX_CODEC_OK {
                return Err(Error::capture(format!(
                    "libvpx: vpx_codec_dec_init failed ({err:?})"
                )));
            }
            Ok(Self {
                device: device.clone(),
                ctx,
                initialized: true,
                texture: None,
                tex_w: 0,
                tex_h: 0,
                nv12: Vec::new(),
            })
        }
    }

    /// Feed one VP8 coded frame; return the newest decoded NV12 texture + its
    /// dimensions when a frame is produced (None while the decoder buffers).
    pub fn decode(&mut self, vp8: &[u8]) -> Result<Option<(ID3D11Texture2D, u32, u32)>> {
        if vp8.is_empty() {
            return Ok(None);
        }
        unsafe {
            let err = vpx_codec_decode(
                &mut self.ctx,
                vp8.as_ptr(),
                vp8.len() as std::os::raw::c_uint,
                ptr::null_mut(),
                VPX_DL_REALTIME,
            );
            if err != VPX_CODEC_OK {
                // A corrupt/partial frame is non-fatal; skip it and keep the stream.
                tracing::debug!("libvpx: vpx_codec_decode error {err:?}; skipping frame");
                return Ok(None);
            }

            // Pull the newest decoded image (drain the iterator, keep the last).
            let mut iter: vpx_codec_iter_t = ptr::null();
            let mut latest: *mut vpx_image_t = ptr::null_mut();
            loop {
                let img = vpx_codec_get_frame(&mut self.ctx, &mut iter);
                if img.is_null() {
                    break;
                }
                latest = img;
            }
            if latest.is_null() {
                return Ok(None);
            }
            self.upload_nv12(&*latest)
        }
    }

    /// Convert a decoded I420 (or NV12) libvpx image to an NV12 D3D11 texture.
    unsafe fn upload_nv12(
        &mut self,
        img: &vpx_image_t,
    ) -> Result<Option<(ID3D11Texture2D, u32, u32)>> {
        let w = img.d_w;
        let h = img.d_h;
        if w == 0 || h == 0 {
            return Ok(None);
        }

        // Build a tightly packed NV12 buffer (Y plane, then interleaved UV at half
        // resolution). libvpx gives planar I420 (Y, U, V) with per-plane strides.
        let y_size = (w * h) as usize;
        let uv_size = (w * (h / 2)) as usize; // interleaved U/V, half height, full width
        let needed = y_size + uv_size;
        if self.nv12.len() != needed {
            self.nv12.resize(needed, 0);
        }

        let y_plane = img.planes[0];
        let u_plane = img.planes[1];
        let v_plane = img.planes[2];
        let y_stride = img.stride[0] as usize;
        let u_stride = img.stride[1] as usize;
        let v_stride = img.stride[2] as usize;
        if y_plane.is_null() || u_plane.is_null() || v_plane.is_null() {
            return Ok(None);
        }

        // Copy Y plane row by row (dst is tightly packed at width `w`).
        for row in 0..h as usize {
            let src = y_plane.add(row * y_stride);
            let dst = self.nv12.as_mut_ptr().add(row * w as usize);
            ptr::copy_nonoverlapping(src, dst, w as usize);
        }

        // Interleave U and V into the NV12 chroma plane (half width, half height).
        let cw = (w / 2) as usize;
        let ch = (h / 2) as usize;
        let uv_base = y_size;
        for row in 0..ch {
            let u_row = u_plane.add(row * u_stride);
            let v_row = v_plane.add(row * v_stride);
            let dst_row = self.nv12.as_mut_ptr().add(uv_base + row * (cw * 2));
            for col in 0..cw {
                *dst_row.add(col * 2) = *u_row.add(col);
                *dst_row.add(col * 2 + 1) = *v_row.add(col);
            }
        }

        // Avoid an unused-fmt lint while documenting the expected input format.
        let _ = (img.fmt, vpx_img_fmt::VPX_IMG_FMT_I420);

        self.ensure_texture(w, h)?;
        let texture = match self.texture.as_ref() {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        // Upload the packed NV12 into the default-usage texture. Row pitch is the
        // tightly packed width; the chroma plane follows the luma plane in memory,
        // which matches NV12's single-allocation layout for UpdateSubresource.
        let context = self
            .device
            .GetImmediateContext()
            .map_err(|e| Error::capture(format!("GetImmediateContext: {e}")))?;
        context.UpdateSubresource(
            &texture,
            0,
            None,
            self.nv12.as_ptr() as *const core::ffi::c_void,
            w, // row pitch for the luma plane
            w * h, // depth pitch (start of chroma) — luma plane size
        );

        Ok(Some((texture, w, h)))
    }

    /// (Re)create the reusable NV12 texture when the camera dimensions change.
    fn ensure_texture(&mut self, w: u32, h: u32) -> Result<()> {
        if self.texture.is_some() && self.tex_w == w && self.tex_h == h {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|e| Error::capture(format!("CreateTexture2D(NV12 cam): {e}")))?;
        }
        self.texture = texture;
        self.tex_w = w;
        self.tex_h = h;
        // Buffer must be resized for the new dimensions on the next decode.
        self.nv12.clear();
        let _ = (D3D11_CPU_ACCESS_WRITE, D3D11_SUBRESOURCE_DATA::default());
        Ok(())
    }
}

impl Drop for Vp8Decoder {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                vpx_sys::vpx_codec_destroy(&mut self.ctx);
            }
        }
    }
}
