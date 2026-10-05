//! Windows+NVIDIA NVENC H.264 encoder.
//!
//! [PENDING-HW] Compiles on `x86_64-pc-windows-msvc`; runs on an NVIDIA GPU with the
//! driver-provided `nvEncodeAPI64.dll`. The input is the NV12 `ID3D11Texture2D`
//! produced by `g9-convert`; NVENC registers that texture directly, so there is **no
//! GPU→CPU copy** of pixels on the input side. Output is an Annex-B H.264 access unit
//! (SPS/PPS emitted on keyframes) copied out of the locked bitstream buffer.
//!
//! Rate control is CBR; tuning/preset/GOP/B-frames come from the `EncoderProfile`
//! (WebRTC = P4/low_latency/0 B-frames; YouTube = P5/high_quality/2s GOP). There is
//! no x264/x265 fallback — if NVENC init fails we return an error (rule #9/#33/#34).

use crate::nvenc_ffi::*;
use g9_capture::{D3DContext, GpuTextureFrame};
use g9_core::frame::{EncodedFrame, FrameKind, VideoCodec};
use g9_core::h264::{contains_idr, extract_parameter_sets};
use g9_core::{EncoderProfile, Error, PtsClock, RateControl, Result};
use std::ffi::c_void;
use std::time::Duration;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

pub struct NvencEncoder {
    _dll: HMODULE,
    api: Box<NV_ENCODE_API_FUNCTION_LIST>,
    encoder: *mut c_void,
    #[allow(dead_code)] // reserved for register-once optimization (see POC report)
    registered_resource: *mut c_void,
    bitstream: *mut c_void,
    #[allow(dead_code)] // kept for reconfigure/debug
    profile: EncoderProfile,
    clock: PtsClock,
    frame_index: u32,
    force_idr: bool,
    width: u32,
    height: u32,
    cached_params: Option<g9_core::frame::ParameterSets>,
}

impl NvencEncoder {
    pub fn new(_profile: EncoderProfile, _clock: PtsClock) -> Result<Self> {
        Err(Error::encode("use NvencEncoder::new_with_ctx on Windows"))
    }

    /// Create an NVENC H.264 session bound to the capture D3D11 device.
    pub fn new_with_ctx(ctx: &D3DContext, profile: EncoderProfile, clock: PtsClock) -> Result<Self> {
        unsafe {
            // 1) Load nvEncodeAPI64.dll (ships with the NVIDIA driver).
            let dll = LoadLibraryA(PCSTR(b"nvEncodeAPI64.dll\0".as_ptr()))
                .map_err(|e| Error::encode(format!("LoadLibrary nvEncodeAPI64.dll: {e}")))?;
            let create = GetProcAddress(dll, PCSTR(b"NvEncodeAPICreateInstance\0".as_ptr()))
                .ok_or_else(|| Error::encode("NvEncodeAPICreateInstance not found"))?;
            let create: PFN_NvEncodeAPICreateInstance = std::mem::transmute(create);

            // Diagnostic: struct sizes + computed versions, to compare against the
            // SDK header's expected values when debugging INVALID_VERSION.
            tracing::info!(
                target: "g9::nvenc",
                "ffi sizes: FUNCTION_LIST={} LOCK_BITSTREAM={} PIC_PARAMS={} CONFIG={} INIT={} ; \
                 ver: lock={:#x} pic={:#x} api={:#x}",
                std::mem::size_of::<NV_ENCODE_API_FUNCTION_LIST>(),
                std::mem::size_of::<NV_ENC_LOCK_BITSTREAM>(),
                std::mem::size_of::<NV_ENC_PIC_PARAMS>(),
                std::mem::size_of::<NV_ENC_CONFIG>(),
                std::mem::size_of::<NV_ENC_INITIALIZE_PARAMS>(),
                struct_version_rt(2) | (1 << 31),
                struct_version_rt(6) | (1 << 31),
                api_version(),
            );

            // 2) Fill the function list.
            let mut api: Box<NV_ENCODE_API_FUNCTION_LIST> = Box::new(std::mem::zeroed());
            api.version = struct_version_rt(2);
            let st = create(api.as_mut() as *mut _);
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "NvEncodeAPICreateInstance failed: status {st}"
                )));
            }

            // 3) Open an encode session over the D3D11 device.
            let mut session: *mut c_void = std::ptr::null_mut();
            let mut open = std::mem::zeroed::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>();
            open.version = struct_version_rt(1);
            open.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = ctx.device().as_raw();
            open.apiVersion = api_version();
            let st = (api.nvEncOpenEncodeSessionEx.unwrap())(&mut open, &mut session);
            if st != NV_ENC_SUCCESS || session.is_null() {
                return Err(Error::encode(format!(
                    "failed to open encode session. NvencStatus: {st}"
                )));
            }

            // 4) Query the preset config for H.264 + chosen preset + tuning.
            let preset_guid = match profile.preset.as_str() {
                "p5" => NV_ENC_PRESET_P5_GUID,
                _ => NV_ENC_PRESET_P4_GUID,
            };
            let tuning = match profile.tuning.as_str() {
                "ultra_low_latency" => NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                "high_quality" => NV_ENC_TUNING_INFO_HIGH_QUALITY,
                _ => NV_ENC_TUNING_INFO_LOW_LATENCY,
            };
            let mut preset_cfg = std::mem::zeroed::<NV_ENC_PRESET_CONFIG>();
            preset_cfg.version = struct_version_rt(4) | (1 << 31);
            preset_cfg.presetCfg.version = struct_version_rt(7) | (1 << 31);
            let st = (api.nvEncGetEncodePresetConfigEx.unwrap())(
                session,
                NV_ENC_CODEC_H264_GUID,
                preset_guid,
                tuning,
                &mut preset_cfg,
            );
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "GetEncodePresetConfigEx failed: status {st}"
                )));
            }

            // 5) Override rate control (CBR), GOP and B-frames from the profile.
            let mut config = preset_cfg.presetCfg;
            config.version = struct_version_rt(7) | (1 << 31);
            config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
            config.gopLength = profile.gop_frames;
            config.frameIntervalP = (profile.b_frames as i32) + 1; // P-frame interval
            config.rcParams.version = struct_version_rt(1);
            config.rcParams.rateControlMode = match profile.rate_control {
                RateControl::Cbr => NV_ENC_PARAMS_RC_CBR,
                RateControl::VbrCapped => NV_ENC_PARAMS_RC_VBR,
            };
            config.rcParams.averageBitRate = profile.bitrate_bps;
            config.rcParams.maxBitRate = profile.bitrate_bps;
            // VBV = one frame for low latency (prevents large rate bursts).
            config.rcParams.vbvBufferSize = profile.bitrate_bps / profile.fps.max(1);
            config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
            config.rcParams.lookaheadDepth = profile.lookahead as u16;

            // 6) Initialize the encoder.
            let mut init = std::mem::zeroed::<NV_ENC_INITIALIZE_PARAMS>();
            init.version = struct_version_rt(5);
            init.encodeGUID = NV_ENC_CODEC_H264_GUID;
            init.presetGUID = preset_guid;
            init.encodeWidth = profile.width;
            init.encodeHeight = profile.height;
            init.darWidth = profile.width;
            init.darHeight = profile.height;
            init.frameRateNum = profile.fps;
            init.frameRateDen = 1;
            init.enablePTD = 1; // picture-type decision by NVENC
            init.tuningInfo = tuning;
            init.encodeConfig = &mut config;
            let st = (api.nvEncInitializeEncoder.unwrap())(session, &mut init);
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "nvEncInitializeEncoder failed: status {st}"
                )));
            }

            // 7) Create an output bitstream buffer.
            let mut bb = std::mem::zeroed::<NV_ENC_CREATE_BITSTREAM_BUFFER>();
            bb.version = struct_version_rt(1);
            let st = (api.nvEncCreateBitstreamBuffer.unwrap())(session, &mut bb);
            if st != NV_ENC_SUCCESS || bb.bitstreamBuffer.is_null() {
                return Err(Error::encode(format!(
                    "nvEncCreateBitstreamBuffer failed: status {st}"
                )));
            }

            Ok(Self {
                _dll: dll,
                api,
                encoder: session,
                registered_resource: std::ptr::null_mut(),
                bitstream: bb.bitstreamBuffer,
                profile,
                clock,
                frame_index: 0,
                force_idr: true, // first frame is a keyframe
                width: init.encodeWidth,
                height: init.encodeHeight,
                cached_params: None,
            })
        }
    }

    pub fn force_idr(&mut self) {
        self.force_idr = true;
    }

    /// Encode one NV12 GPU texture. Registers+maps the texture (zero-copy), submits
    /// it, and locks the bitstream to copy out the encoded access unit.
    pub fn encode(&mut self, nv12: &GpuTextureFrame) -> Result<Option<EncodedFrame>> {
        unsafe {
            let tex = nv12
                .texture()
                .ok_or_else(|| Error::encode("NV12 frame has no GPU texture"))?;

            // Register the D3D11 texture as an NVENC input resource (once; re-registered
            // each call here for clarity — the converter reuses one texture, so a real
            // optimization is to register once and cache. Kept simple + correct.)
            let mut reg = std::mem::zeroed::<NV_ENC_REGISTER_RESOURCE>();
            reg.version = struct_version_rt(3);
            reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            reg.width = self.width;
            reg.height = self.height;
            reg.resourceToRegister = tex.as_raw();
            reg.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
            let st = (self.api.nvEncRegisterResource.unwrap())(self.encoder, &mut reg);
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!("nvEncRegisterResource: status {st}")));
            }

            // Map it to get an input buffer handle.
            let mut map = std::mem::zeroed::<NV_ENC_MAP_INPUT_RESOURCE>();
            map.version = struct_version_rt(4);
            map.registeredResource = reg.registeredResource;
            let st = (self.api.nvEncMapInputResource.unwrap())(self.encoder, &mut map);
            if st != NV_ENC_SUCCESS {
                let _ = (self.api.nvEncUnregisterResource.unwrap())(self.encoder, reg.registeredResource);
                return Err(Error::encode(format!("nvEncMapInputResource: status {st}")));
            }

            let pts = self.clock.now();

            // Submit the frame.
            let mut pic = std::mem::zeroed::<NV_ENC_PIC_PARAMS>();
            pic.version = struct_version_rt(6) | (1 << 31);
            pic.inputWidth = self.width;
            pic.inputHeight = self.height;
            pic.inputBuffer = map.mappedResource;
            pic.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
            pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
            pic.outputBitstream = self.bitstream;
            pic.inputTimeStamp = pts.as_millis() as u64;
            pic.frameIdx = self.frame_index;
            if self.force_idr {
                pic.encodePicFlags |= NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
                self.force_idr = false;
            }

            let st = (self.api.nvEncEncodePicture.unwrap())(self.encoder, &mut pic);
            // Unmap+unregister regardless of encode result.
            let _ = (self.api.nvEncUnmapInputResource.unwrap())(self.encoder, map.mappedResource);
            let _ = (self.api.nvEncUnregisterResource.unwrap())(self.encoder, reg.registeredResource);

            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!("nvEncEncodePicture: status {st}")));
            }
            self.frame_index += 1;

            // Lock the bitstream and copy out the encoded access unit.
            let mut lock = std::mem::zeroed::<NV_ENC_LOCK_BITSTREAM>();
            lock.version = struct_version_rt(2) | (1 << 31);
            lock.outputBitstream = self.bitstream;
            let st = (self.api.nvEncLockBitstream.unwrap())(self.encoder, &mut lock);
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!("nvEncLockBitstream: status {st}")));
            }

            let data = std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            );
            let annexb = bytes::Bytes::copy_from_slice(data);
            let _ = (self.api.nvEncUnlockBitstream.unwrap())(self.encoder, self.bitstream);

            // Classify + extract SPS/PPS on keyframes.
            let is_key = contains_idr(&annexb);
            if is_key {
                if let Some(ps) = extract_parameter_sets(&annexb) {
                    self.cached_params = Some(ps);
                }
            }

            Ok(Some(EncodedFrame {
                codec: VideoCodec::H264,
                kind: if is_key { FrameKind::Key } else { FrameKind::Delta },
                data: annexb,
                annex_b: true,
                pts,
                dts: pts, // no B-frames → dts == pts
                parameter_sets: if is_key { self.cached_params.clone() } else { None },
            }))
        }
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.bitstream.is_null() {
                if let Some(f) = self.api.nvEncDestroyBitstreamBuffer {
                    let _ = f(self.encoder, self.bitstream);
                }
            }
            if !self.encoder.is_null() {
                if let Some(f) = self.api.nvEncDestroyEncoder {
                    let _ = f(self.encoder);
                }
            }
            let _ = FreeLibrary(self._dll);
        }
    }
}

// NVENC session is confined to the single video thread; we do not share it.
// (No Send/Sync impls — the pipeline keeps all GPU work on one OS thread.)

// Keep the Duration import used.
#[allow(dead_code)]
fn _d() -> Duration {
    Duration::from_millis(0)
}
