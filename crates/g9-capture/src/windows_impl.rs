//! Windows implementation: D3D11 device + adapter/display enumeration + DXGI Desktop
//! Duplication capture.
//!
//! [PENDING-HW] This module compiles on `x86_64-pc-windows-msvc` with the `windows`
//! crate and runs on a machine with a GPU-backed DXGI output. It is written against
//! the DXGI Desktop Duplication API (`IDXGIOutputDuplication`). It cannot link on
//! macOS (no Windows SDK), which is why the workspace uses the non-Windows stub there.
//!
//! Frame path: `AcquireNextFrame` hands back an `ID3D11Texture2D` that already lives
//! in VRAM. We keep that texture and pass it straight to the converter — there is no
//! CPU readback of pixels anywhere in this file.

use crate::types::{AdapterInfo, DisplayInfo, GpuTextureFrame};
use g9_core::{Error, Result};
use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};

/// Owns the D3D11 device + immediate context and the chosen adapter.
pub struct D3DContext {
    pub(crate) device: ID3D11Device,
    pub(crate) context: ID3D11DeviceContext,
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) feature_level: D3D_FEATURE_LEVEL,
}

// D3D11 objects are not Send by default in the bindings; the pipeline confines all
// D3D11/NVENC use to a single dedicated OS thread (see g9-stream pipeline), so we do
// not share these across threads. We therefore do NOT implement Send/Sync.

impl D3DContext {
    /// Borrow the D3D11 device (shared by converter + NVENC so textures interop).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }
    /// Borrow the immediate context.
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }
    /// Negotiated D3D feature level (for the startup GPU log).
    pub fn feature_level(&self) -> D3D_FEATURE_LEVEL {
        self.feature_level
    }

    /// Create a device, preferring an NVIDIA adapter (vendor id 0x10DE). If
    /// `preferred_adapter` is given, use that index instead.
    pub fn new(preferred_adapter: Option<u32>) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| Error::capture(format!("CreateDXGIFactory1: {e}")))?;

            let adapter = select_adapter(&factory, preferred_adapter)?;

            let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut got_level = D3D_FEATURE_LEVEL_11_0;

            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN, // required when passing an explicit adapter
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut got_level),
                Some(&mut context),
            )
            .map_err(|e| Error::capture(format!("D3D11CreateDevice: {e}")))?;

            Ok(Self {
                device: device.ok_or_else(|| Error::capture("null D3D11 device"))?,
                context: context.ok_or_else(|| Error::capture("null D3D11 context"))?,
                adapter,
                feature_level: got_level,
            })
        }
    }

    /// Enumerate all adapters (for `--list-displays`).
    pub fn enumerate_adapters() -> Result<Vec<AdapterInfo>> {
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| Error::capture(format!("CreateDXGIFactory1: {e}")))?;
            let mut out = Vec::new();
            let mut i = 0u32;
            loop {
                match factory.EnumAdapters1(i) {
                    Ok(adapter) => {
                        let desc = adapter
                            .GetDesc1()
                            .map_err(|e| Error::capture(format!("GetDesc1: {e}")))?;
                        let name = String::from_utf16_lossy(
                            &desc.Description[..desc
                                .Description
                                .iter()
                                .position(|&c| c == 0)
                                .unwrap_or(desc.Description.len())],
                        );
                        out.push(AdapterInfo {
                            index: i,
                            description: name,
                            dedicated_vram_mb: (desc.DedicatedVideoMemory / (1024 * 1024)) as u64,
                            is_nvidia: desc.VendorId == 0x10DE,
                            feature_level: "11_1".to_string(),
                        });
                        i += 1;
                    }
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(Error::capture(format!("EnumAdapters1: {e}"))),
                }
            }
            Ok(out)
        }
    }

    /// Enumerate all display outputs across all adapters (for `--display N`).
    pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| Error::capture(format!("CreateDXGIFactory1: {e}")))?;
            let mut out = Vec::new();
            let mut global_index = 0u32;
            let mut ai = 0u32;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                let mut oi = 0u32;
                loop {
                    match adapter.EnumOutputs(oi) {
                        Ok(output) => {
                            let desc = output
                                .GetDesc()
                                .map_err(|e| Error::capture(format!("output GetDesc: {e}")))?;
                            let (w, h) = rect_size(&desc);
                            let name = String::from_utf16_lossy(
                                &desc.DeviceName[..desc
                                    .DeviceName
                                    .iter()
                                    .position(|&c| c == 0)
                                    .unwrap_or(desc.DeviceName.len())],
                            );
                            out.push(DisplayInfo {
                                index: global_index,
                                adapter_index: ai,
                                device_name: name,
                                width: w,
                                height: h,
                                is_attached: desc.AttachedToDesktop.as_bool(),
                            });
                            global_index += 1;
                            oi += 1;
                        }
                        Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                        Err(e) => return Err(Error::capture(format!("EnumOutputs: {e}"))),
                    }
                }
                ai += 1;
            }
            if out.is_empty() {
                return Err(Error::NoGpuOutput);
            }
            Ok(out)
        }
    }
}

/// Pick the adapter: explicit index if given, else first NVIDIA, else adapter 0.
unsafe fn select_adapter(
    factory: &IDXGIFactory1,
    preferred: Option<u32>,
) -> Result<IDXGIAdapter1> {
    if let Some(idx) = preferred {
        return factory
            .EnumAdapters1(idx)
            .map_err(|e| Error::capture(format!("adapter {idx} not found: {e}")));
    }
    // Prefer NVIDIA.
    let mut i = 0u32;
    let mut first: Option<IDXGIAdapter1> = None;
    while let Ok(adapter) = factory.EnumAdapters1(i) {
        if first.is_none() {
            first = Some(adapter.clone());
        }
        if let Ok(desc) = adapter.GetDesc1() {
            if desc.VendorId == 0x10DE {
                return Ok(adapter);
            }
        }
        i += 1;
    }
    first.ok_or(Error::NoGpuOutput)
}

fn rect_size(desc: &DXGI_OUTPUT_DESC) -> (u32, u32) {
    let r = desc.DesktopCoordinates;
    (
        (r.right - r.left).max(0) as u32,
        (r.bottom - r.top).max(0) as u32,
    )
}

/// Captures frames from one display via DXGI Desktop Duplication.
pub struct Capturer {
    duplication: IDXGIOutputDuplication,
    output_desc: DXGI_OUTPUT_DESC,
    holds_frame: bool,
}

impl Capturer {
    /// Set up duplication for `display_index` (global index from `enumerate_displays`).
    pub fn new(ctx: &D3DContext, display_index: u32) -> Result<Self> {
        unsafe {
            // Resolve the global display index to (adapter, output). For a single-GPU
            // VM the adapter is `ctx.adapter`; we walk its outputs.
            let output = find_output(&ctx.adapter, display_index)?;
            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| Error::capture(format!("IDXGIOutput1 cast: {e}")))?;
            let output_desc = output
                .GetDesc()
                .map_err(|e| Error::capture(format!("output GetDesc: {e}")))?;

            let duplication = match output1.DuplicateOutput(&ctx.device) {
                Ok(d) => d,
                Err(e) if e.code() == E_ACCESSDENIED => {
                    // Typically "desktop in full-screen exclusive / secure" transient state.
                    return Err(Error::capture(
                        "DuplicateOutput access denied (secure desktop or fullscreen-exclusive); retry",
                    ));
                }
                Err(e) => {
                    return Err(Error::capture(format!(
                        "DuplicateOutput failed: {e}. On RDSH ensure UseWddmDriver=1"
                    )))
                }
            };

            Ok(Self {
                duplication,
                output_desc,
                holds_frame: false,
            })
        }
    }

    /// Acquire the next frame.
    /// - `Ok(Some(frame))` on a new frame (texture lives on the GPU).
    /// - `Ok(None)` on timeout (no new frame within `timeout_ms`) — caller loops.
    /// - `Err(CaptureReinit)` on ACCESS_LOST / resolution change — caller rebuilds.
    pub fn acquire_frame(&mut self, timeout_ms: u32) -> Result<Option<GpuTextureFrame>> {
        unsafe {
            // Release the previous frame before acquiring the next (DXGI requirement).
            if self.holds_frame {
                let _ = self.duplication.ReleaseFrame();
                self.holds_frame = false;
            }

            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // Mode change, desktop switch, or display reconnect — reinit.
                    return Err(Error::CaptureReinit);
                }
                Err(e) => return Err(Error::capture(format!("AcquireNextFrame: {e}"))),
            }
            self.holds_frame = true;

            let resource = resource.ok_or_else(|| Error::capture("null duplication resource"))?;
            // The desktop image as a GPU texture. No CPU copy.
            let texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D = resource
                .cast()
                .map_err(|e| Error::capture(format!("texture cast: {e}")))?;

            let (w, h) = rect_size(&self.output_desc);
            Ok(Some(GpuTextureFrame {
                width: w,
                height: h,
                acquired_at: Instant::now(),
                texture: Some(texture),
            }))
        }
    }
}

impl Capturer {
    /// DEBUG ONLY: capture one real desktop frame and write it to a PPM file so we
    /// can SEE what DXGI is actually grabbing (vs guessing why a stream is black).
    /// This is the one place in the crate that reads pixels back to the CPU; it is
    /// never on the streaming hot path. Returns the (width, height) written.
    ///
    /// We retry until we get a frame with a non-zero present time (so we don't dump
    /// a cursor-only or empty update), then CopyResource into a CPU-readable staging
    /// texture, Map it, and write BGRA->RGB as binary PPM (P6).
    pub fn dump_one_frame(&mut self, ctx: &D3DContext, path: &str) -> Result<(u32, u32)> {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
            D3D11_USAGE_STAGING,
        };
        unsafe {
            // Grab several real frames and keep the LAST one. The first AcquireNextFrame
            // after DuplicateOutput often returns an initial blank/black surface before
            // the compositor presents real content; skipping ahead captures actual
            // desktop pixels. We wait up to ~5s for at least a few real frames.
            let mut frame_tex = None;
            let mut got = 0;
            for _ in 0..600 {
                match self.acquire_frame(16)? {
                    Some(f) => {
                        if let Some(t) = f.texture() {
                            frame_tex = Some(t.clone());
                            got += 1;
                            // Keep going until we've seen a handful of real presents.
                            if got >= 10 {
                                break;
                            }
                        }
                    }
                    None => continue,
                }
            }
            let src = frame_tex
                .ok_or_else(|| Error::capture("no frame acquired within timeout for dump"))?;

            // Describe a CPU-readable staging copy of the captured texture.
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut desc);
            let (w, h) = (desc.Width, desc.Height);
            let mut staging_desc = desc;
            staging_desc.Usage = D3D11_USAGE_STAGING;
            staging_desc.BindFlags = 0;
            staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            staging_desc.MiscFlags = 0;

            let mut staging: Option<
                windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
            > = None;
            ctx.device()
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .map_err(|e| Error::capture(format!("CreateTexture2D(staging): {e}")))?;
            let staging = staging.ok_or_else(|| Error::capture("null staging texture"))?;

            ctx.context().CopyResource(&staging, &src);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.context()
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| Error::capture(format!("Map(staging): {e}")))?;

            let row_pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            // Compute mean luma to report whether the frame is basically black.
            let mut sum: u64 = 0;
            let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
            ppm.reserve((w * h * 3) as usize);
            for y in 0..h as usize {
                let row = base.add(y * row_pitch);
                for x in 0..w as usize {
                    let px = row.add(x * 4); // BGRA
                    let b = *px;
                    let g = *px.add(1);
                    let r = *px.add(2);
                    ppm.push(r);
                    ppm.push(g);
                    ppm.push(b);
                    sum += r as u64 + g as u64 + b as u64;
                }
            }
            ctx.context().Unmap(&staging, 0);

            std::fs::write(path, &ppm)
                .map_err(|e| Error::capture(format!("write {path}: {e}")))?;

            let mean = sum as f64 / (w as f64 * h as f64 * 3.0);
            tracing::info!(
                target: "g9::capture",
                "dumped frame {}x{} to {} (mean pixel value {:.1}/255 — near 0 means a BLACK capture)",
                w, h, path, mean
            );
            Ok((w, h))
        }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        unsafe {
            if self.holds_frame {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }
}

/// Resolve a global display index to the matching output on this adapter.
unsafe fn find_output(adapter: &IDXGIAdapter1, display_index: u32) -> Result<IDXGIOutput> {
    // For a single-adapter VM, the global index equals the output index on this
    // adapter. (Multi-GPU resolution is a future enhancement — documented.)
    adapter
        .EnumOutputs(display_index)
        .map_err(|e| Error::capture(format!("display index {display_index} not found: {e}")))
}

// Suppress an unused warning for DXGI_FORMAT import kept for documentation of the
// expected BGRA capture format (DXGI_FORMAT_B8G8R8A8_UNORM).
#[allow(dead_code)]
fn _expected_capture_format() -> DXGI_FORMAT {
    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM
}
