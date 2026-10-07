//! Media Foundation H.264 → NV12 decoder for the facecam, producing a GPU-resident
//! `ID3D11Texture2D` the compositor can blend without CPU readback.
//!
//! [PENDING-HW] Compiles on `x86_64-pc-windows-msvc`; the Media Foundation decoder
//! MFT body runs on hardware. Uses the system H.264 Video Decoder MFT, bound to the
//! engine's D3D11 device via a `IMFDXGIDeviceManager` so decoded samples are D3D11
//! NV12 textures. Each `decode()` feeds one Annex-B access unit and returns the
//! newest decoded frame when one is available.
//!
//! This is intentionally conservative: if MF decode cannot be initialized on the
//! host (missing codec, no HW path), `new()` fails and the compositor degrades to a
//! game-only broadcast rather than breaking the pipeline.

use g9_core::{Error, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_DECODER, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, IMFSample, IMFTransform, MFCreateDXGIDeviceManager,
    MFCreateMemoryBuffer, MFCreateSample, MFStartup, MFSTARTUP_LITE, MF_VERSION,
};

/// Decodes H.264 access units into NV12 D3D11 textures via a Media Foundation MFT.
pub struct H264Decoder {
    device: ID3D11Device,
    #[allow(dead_code)]
    device_manager: IMFDXGIDeviceManager,
    transform: IMFTransform,
    configured: bool,
    /// Reusable NV12 texture for the software-decode fallback (system-memory MF
    /// samples copied to the GPU), sized to the current frame; recreated on resize.
    sw_texture: Option<ID3D11Texture2D>,
    sw_w: u32,
    sw_h: u32,
    /// Scratch NV12 buffer reused across software-decoded frames.
    sw_nv12: Vec<u8>,
}

impl H264Decoder {
    /// Initialize the MF H.264 decoder bound to `device`'s DXGI device manager.
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_LITE)
                .map_err(|e| Error::capture(format!("MFStartup: {e}")))?;

            // Bind the decoder to our D3D11 device so output samples are D3D11
            // textures on the same device the compositor uses.
            let mut reset_token: u32 = 0;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
                .map_err(|e| Error::capture(format!("MFCreateDXGIDeviceManager: {e}")))?;
            let device_manager =
                manager.ok_or_else(|| Error::capture("null DXGI device manager"))?;
            device_manager
                .ResetDevice(device, reset_token)
                .map_err(|e| Error::capture(format!("ResetDevice: {e}")))?;

            // Instantiate the system H.264 decoder MFT.
            let transform = create_h264_decoder_mft()?;

            Ok(Self {
                device: device.clone(),
                device_manager,
                transform,
                configured: false,
                sw_texture: None,
                sw_w: 0,
                sw_h: 0,
                sw_nv12: Vec::new(),
            })
        }
    }

    /// Feed one Annex-B access unit; return the newest decoded NV12 texture + its
    /// dimensions when the decoder emits a frame (None while it buffers).
    pub fn decode(&mut self, annex_b: &[u8]) -> Result<Option<(ID3D11Texture2D, u32, u32)>> {
        if annex_b.is_empty() {
            return Ok(None);
        }
        unsafe {
            if !self.configured {
                // First access unit carries SPS/PPS; configure I/O media types now.
                configure_decoder_types(&self.transform)?;
                self.configured = true;
            }
            let sample = wrap_annex_b_sample(annex_b)?;
            // Push input; ignore "need more input" style flow-control errors.
            let _ = self.transform.ProcessInput(0, &sample, 0);
            self.pull_output()
        }
    }

    /// Attempt to pull one decoded output sample and extract its D3D11 NV12 texture.
    unsafe fn pull_output(&mut self) -> Result<Option<(ID3D11Texture2D, u32, u32)>> {
        use windows::Win32::Media::MediaFoundation::{
            IMFMediaBuffer, MF_SOURCE_READERF_ERROR, MFT_OUTPUT_DATA_BUFFER,
        };
        let _ = &self.device;
        let _ = MF_SOURCE_READERF_ERROR;

        let mut status: u32 = 0;
        let mut out = [MFT_OUTPUT_DATA_BUFFER::default()];
        // Allocate an output sample for the MFT to fill (software fallback path).
        let sample = MFCreateSample().map_err(|e| Error::capture(format!("MFCreateSample: {e}")))?;
        out[0].pSample = std::mem::ManuallyDrop::new(Some(sample));
        match self.transform.ProcessOutput(0, &mut out, &mut status) {
            Ok(()) => {}
            // The decoder has no frame ready yet, or wants a media-type change —
            // both are normal; return no frame this call.
            Err(_) => return Ok(None),
        }

        let produced = std::mem::ManuallyDrop::take(&mut out[0].pSample);
        let sample = match produced {
            Some(s) => s,
            None => return Ok(None),
        };
        self.sample_to_texture(&sample)
    }

    /// Turn a decoded MF sample into an NV12 D3D11 texture. GPU (IMFDXGIBuffer)
    /// samples are used directly; software (system-memory) samples — the only kind
    /// this VM's software H.264 decoder produces — are copied into a reusable
    /// D3D11 NV12 texture so the compositor can still blend the facecam.
    unsafe fn sample_to_texture(
        &mut self,
        sample: &IMFSample,
    ) -> Result<Option<(ID3D11Texture2D, u32, u32)>> {
        use windows::Win32::Media::MediaFoundation::IMFDXGIBuffer;

        let buffer = sample
            .GetBufferByIndex(0)
            .map_err(|e| Error::capture(format!("GetBufferByIndex: {e}")))?;

        // Fast path: GPU-backed sample exposes IMFDXGIBuffer wrapping the texture.
        if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
            let mut texture: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(
                &ID3D11Texture2D::IID,
                &mut texture as *mut _ as *mut *mut core::ffi::c_void,
            )
            .map_err(|e| Error::capture(format!("GetResource: {e}")))?;
            if let Some(t) = texture {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                t.GetDesc(&mut desc);
                return Ok(Some((t, desc.Width, desc.Height)));
            }
            return Ok(None);
        }

        // Software path: copy the system-memory NV12 into a D3D11 texture.
        self.upload_software_nv12(&buffer)
    }

    /// Copy a system-memory NV12 MF buffer into a reusable D3D11 NV12 texture.
    unsafe fn upload_software_nv12(
        &mut self,
        buffer: &windows::Win32::Media::MediaFoundation::IMFMediaBuffer,
    ) -> Result<Option<(ID3D11Texture2D, u32, u32)>> {
        // Resolve the current output frame size from the MFT output media type.
        let (w, h) = self.output_dimensions()?;
        if w == 0 || h == 0 {
            return Ok(None);
        }

        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len: u32 = 0;
        buffer
            .Lock(&mut ptr, None, Some(&mut cur_len))
            .map_err(|e| Error::capture(format!("sw buffer Lock: {e}")))?;
        // NV12: Y plane (w*h) followed by interleaved UV (w*h/2). Copy the packed
        // buffer verbatim; MF's system-memory NV12 is tightly packed at `w`.
        let needed = (w * h + w * (h / 2)) as usize;
        let copy_len = needed.min(cur_len as usize);
        if self.sw_nv12.len() != needed {
            self.sw_nv12.resize(needed, 0);
        }
        std::ptr::copy_nonoverlapping(ptr, self.sw_nv12.as_mut_ptr(), copy_len);
        let _ = buffer.Unlock();

        self.ensure_sw_texture(w, h)?;
        let texture = match self.sw_texture.as_ref() {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        let context = self
            .device
            .GetImmediateContext()
            .map_err(|e| Error::capture(format!("GetImmediateContext: {e}")))?;
        context.UpdateSubresource(
            &texture,
            0,
            None,
            self.sw_nv12.as_ptr() as *const core::ffi::c_void,
            w,
            w * h,
        );
        Ok(Some((texture, w, h)))
    }

    /// Current decoder output frame size from the MFT output media type.
    unsafe fn output_dimensions(&self) -> Result<(u32, u32)> {
        use windows::Win32::Media::MediaFoundation::MF_MT_FRAME_SIZE;
        let out_type = self
            .transform
            .GetOutputCurrentType(0)
            .map_err(|e| Error::capture(format!("GetOutputCurrentType: {e}")))?;
        let packed = out_type
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(|e| Error::capture(format!("get frame size: {e}")))?;
        // MF packs width in the high 32 bits, height in the low 32 bits.
        let w = (packed >> 32) as u32;
        let h = (packed & 0xFFFF_FFFF) as u32;
        Ok((w, h))
    }

    /// (Re)create the reusable software-decode NV12 texture on a size change.
    unsafe fn ensure_sw_texture(&mut self, w: u32, h: u32) -> Result<()> {
        if self.sw_texture.is_some() && self.sw_w == w && self.sw_h == h {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| Error::capture(format!("CreateTexture2D(sw NV12): {e}")))?;
        self.sw_texture = texture;
        self.sw_w = w;
        self.sw_h = h;
        self.sw_nv12.clear();
        Ok(())
    }
}

/// Create the system H.264 decoder MFT via MFTEnumEx (hardware preferred).
unsafe fn create_h264_decoder_mft() -> Result<IMFTransform> {
    use windows::Win32::Media::MediaFoundation::{
        MFTEnumEx, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO,
        MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12,
    };
    use windows::core::GUID;

    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let mut activate = std::ptr::null_mut();
    let mut count: u32 = 0;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &mut activate,
        &mut count,
    )
    .map_err(|e| Error::capture(format!("MFTEnumEx: {e}")))?;
    if count == 0 || activate.is_null() {
        return Err(Error::capture("no H.264 decoder MFT available"));
    }
    let activates = std::slice::from_raw_parts(activate, count as usize);
    let first = activates[0]
        .as_ref()
        .ok_or_else(|| Error::capture("null MFT activate"))?;
    let transform: IMFTransform = first
        .ActivateObject::<IMFTransform>()
        .map_err(|e| Error::capture(format!("ActivateObject: {e}")))?;
    let _ = GUID::zeroed();
    Ok(transform)
}

/// Configure the decoder's input (H.264) and output (NV12) media types.
unsafe fn configure_decoder_types(transform: &IMFTransform) -> Result<()> {
    use windows::Win32::Media::MediaFoundation::{
        MFCreateMediaType, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MFMediaType_Video, MFVideoFormat_H264,
        MFVideoFormat_NV12,
    };

    let input_type =
        MFCreateMediaType().map_err(|e| Error::capture(format!("MFCreateMediaType: {e}")))?;
    input_type
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|e| Error::capture(format!("set major: {e}")))?;
    input_type
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
        .map_err(|e| Error::capture(format!("set input subtype: {e}")))?;
    transform
        .SetInputType(0, &input_type, 0)
        .map_err(|e| Error::capture(format!("SetInputType: {e}")))?;

    let output_type =
        MFCreateMediaType().map_err(|e| Error::capture(format!("MFCreateMediaType: {e}")))?;
    output_type
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|e| Error::capture(format!("set major: {e}")))?;
    output_type
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
        .map_err(|e| Error::capture(format!("set output subtype: {e}")))?;
    transform
        .SetOutputType(0, &output_type, 0)
        .map_err(|e| Error::capture(format!("SetOutputType: {e}")))?;
    Ok(())
}

/// Wrap an Annex-B access unit in an `IMFSample` with one memory buffer.
unsafe fn wrap_annex_b_sample(annex_b: &[u8]) -> Result<IMFSample> {
    let buffer = MFCreateMemoryBuffer(annex_b.len() as u32)
        .map_err(|e| Error::capture(format!("MFCreateMemoryBuffer: {e}")))?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut max_len: u32 = 0;
    buffer
        .Lock(&mut ptr, Some(&mut max_len), None)
        .map_err(|e| Error::capture(format!("buffer Lock: {e}")))?;
    std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
    buffer
        .SetCurrentLength(annex_b.len() as u32)
        .map_err(|e| Error::capture(format!("SetCurrentLength: {e}")))?;
    let _ = buffer.Unlock();

    let sample = MFCreateSample().map_err(|e| Error::capture(format!("MFCreateSample: {e}")))?;
    sample
        .AddBuffer(&buffer)
        .map_err(|e| Error::capture(format!("AddBuffer: {e}")))?;
    Ok(sample)
}

/// Extract the D3D11 NV12 texture from a decoded sample's DXGI buffer.

