//! Windows+NVIDIA NVENC H.264 encoder, using bindgen-generated FFI (byte-exact
//! struct layouts from the vendored `nvEncodeAPI.h`).
//!
//! [PENDING-HW] Compiles on `x86_64-pc-windows-msvc`; runs on an NVIDIA GPU with the
//! driver-provided `nvEncodeAPI64.dll`. Input is the NV12 `ID3D11Texture2D` from
//! `g9-convert`, registered directly with NVENC (no GPU→CPU pixel copy). Output is an
//! Annex-B H.264 access unit (SPS/PPS on keyframes). No x264/x265 fallback.

use crate::nvenc_ffi::*;
use g9_capture::{D3DContext, GpuTextureFrame};
use g9_core::frame::{EncodedFrame, FrameKind, VideoCodec};
use g9_core::h264::{contains_idr, extract_parameter_sets};
use g9_core::{EncoderProfile, Error, PtsClock, RateControl, Result};
use std::ffi::c_void;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

const NV_ENC_SUCCESS: i32 = 0;

// Struct version numbers come straight from the header macros:
//   OPEN_SESSION_EX=1, PRESET_CONFIG=5|hi, CONFIG=9|hi, RC_PARAMS=1, INIT=7|hi,
//   CREATE_BITSTREAM_BUFFER=1, REGISTER_RESOURCE=5, MAP_INPUT_RESOURCE=4,
//   PIC_PARAMS=7|hi, LOCK_BITSTREAM=2|hi, FUNCTION_LIST=2.   (hi = 1<<31)
const HI: u32 = 1 << 31;
fn ver_func_list() -> u32 { struct_version(2) }
fn ver_open() -> u32 { struct_version(1) }
fn ver_preset_cfg() -> u32 { struct_version(5) | HI }
fn ver_config() -> u32 { struct_version(9) | HI }
fn ver_rc() -> u32 { struct_version(1) }
fn ver_init() -> u32 { struct_version(7) | HI }
fn ver_bitstream_buf() -> u32 { struct_version(1) }
fn ver_register() -> u32 { struct_version(5) }
fn ver_map() -> u32 { struct_version(4) }
fn ver_pic() -> u32 { struct_version(7) | HI }
fn ver_lock() -> u32 { struct_version(2) | HI }

pub struct NvencEncoder {
    _dll: HMODULE,
    api: Box<NV_ENCODE_API_FUNCTION_LIST>,
    encoder: *mut c_void,
    bitstream: *mut c_void,
    #[allow(dead_code)]
    profile: EncoderProfile,
    clock: PtsClock,
    frame_index: u32,
    force_idr: bool,
    width: u32,
    height: u32,
    cached_params: Option<g9_core::frame::ParameterSets>,
    logged_sps: bool,
}

impl NvencEncoder {
    pub fn new(_profile: EncoderProfile, _clock: PtsClock) -> Result<Self> {
        Err(Error::encode("use NvencEncoder::new_with_ctx on Windows"))
    }

    pub fn new_with_ctx(ctx: &D3DContext, profile: EncoderProfile, clock: PtsClock) -> Result<Self> {
        unsafe {
            // 1) Load the driver's NVENC entry point.
            let dll = LoadLibraryA(PCSTR(b"nvEncodeAPI64.dll\0".as_ptr()))
                .map_err(|e| Error::encode(format!("LoadLibrary nvEncodeAPI64.dll: {e}")))?;
            let proc = GetProcAddress(dll, PCSTR(b"NvEncodeAPICreateInstance\0".as_ptr()))
                .ok_or_else(|| Error::encode("NvEncodeAPICreateInstance not found"))?;
            type CreateInstanceFn =
                unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> i32;
            let create: CreateInstanceFn = std::mem::transmute(proc);

            // 2) Function list.
            let mut api: Box<NV_ENCODE_API_FUNCTION_LIST> = Box::new(std::mem::zeroed());
            api.version = ver_func_list();
            let st = create(api.as_mut() as *mut _);
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "NvEncodeAPICreateInstance: {} ({st})",
                    status_name(st)
                )));
            }

            tracing::info!(
                target: "g9::nvenc",
                "nvenc init: api={:#x} CONFIG_VER={:#x} INIT_VER={:#x} PIC_VER={:#x} LOCK_VER={:#x}",
                api_version(), ver_config(), ver_init(), ver_pic(), ver_lock()
            );

            // 3) Open session over the D3D11 device.
            let mut session: *mut c_void = std::ptr::null_mut();
            let mut open = std::mem::zeroed::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>();
            open.version = ver_open();
            open.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = ctx.device().as_raw();
            open.apiVersion = api_version();
            let st = (api.nvEncOpenEncodeSessionEx.unwrap())(&mut open, &mut session);
            if st != NV_ENC_SUCCESS || session.is_null() {
                return Err(Error::encode(format!(
                    "OpenEncodeSessionEx: {} ({st})",
                    status_name(st)
                )));
            }

            // 4) Preset config for H.264 + preset + tuning.
            let preset_guid = match profile.preset.as_str() {
                "p5" => G9_NV_ENC_PRESET_P5_GUID,
                _ => G9_NV_ENC_PRESET_P4_GUID,
            };
            let tuning = match profile.tuning.as_str() {
                "ultra_low_latency" => NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                "high_quality" => NV_ENC_TUNING_INFO_HIGH_QUALITY,
                _ => NV_ENC_TUNING_INFO_LOW_LATENCY,
            };
            let mut preset_cfg = std::mem::zeroed::<NV_ENC_PRESET_CONFIG>();
            preset_cfg.version = ver_preset_cfg();
            preset_cfg.presetCfg.version = ver_config();
            let st = (api.nvEncGetEncodePresetConfigEx.unwrap())(
                session,
                G9_NV_ENC_CODEC_H264_GUID,
                preset_guid,
                tuning,
                &mut preset_cfg,
            );
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "GetEncodePresetConfigEx: {} ({st})",
                    status_name(st)
                )));
            }

            // 5) Override RC (CBR), GOP, B-frames.
            let mut config = preset_cfg.presetCfg;
            config.version = ver_config();
            config.profileGUID = G9_NV_ENC_H264_PROFILE_HIGH_GUID;
            config.gopLength = profile.gop_frames;
            config.frameIntervalP = (profile.b_frames as i32) + 1;
            config.rcParams.version = ver_rc();
            config.rcParams.rateControlMode = match profile.rate_control {
                RateControl::Cbr => NV_ENC_PARAMS_RC_CBR,
                RateControl::VbrCapped => NV_ENC_PARAMS_RC_VBR,
            };
            config.rcParams.averageBitRate = profile.bitrate_bps;
            config.rcParams.maxBitRate = profile.bitrate_bps;
            config.rcParams.vbvBufferSize = profile.bitrate_bps / profile.fps.max(1);
            config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
            config.rcParams.lookaheadDepth = profile.lookahead as u16;

            // H.264-specific config: force SPS/PPS on EVERY IDR. This is the real
            // fix for the browser black screen — without it, periodic GOP keyframes
            // are bare IDRs with no parameter sets, so the decoder can never
            // initialize (bytes arrive, framesDecoded stays 0). repeatSPSPPS=1 makes
            // every keyframe self-contained and decodable by a viewer that joins at
            // any time. idrPeriod = gopLength keeps IDR cadence aligned with the GOP.
            {
                let h264 = &mut config.encodeCodecConfig.h264Config;
                h264.set_repeatSPSPPS(1);
                h264.set_disableSPSPPS(0);
                h264.idrPeriod = profile.gop_frames;
            }

            // 6) Initialize encoder.
            let mut init = std::mem::zeroed::<NV_ENC_INITIALIZE_PARAMS>();
            init.version = ver_init();
            init.encodeGUID = G9_NV_ENC_CODEC_H264_GUID;
            init.presetGUID = preset_guid;
            init.encodeWidth = profile.width;
            init.encodeHeight = profile.height;
            init.darWidth = profile.width;
            init.darHeight = profile.height;
            init.frameRateNum = profile.fps;
            init.frameRateDen = 1;
            init.enablePTD = 1;
            init.tuningInfo = tuning;
            init.encodeConfig = &mut config;
            let st = (api.nvEncInitializeEncoder.unwrap())(session, &mut init);
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "InitializeEncoder: {} ({st})",
                    status_name(st)
                )));
            }

            // 7) Output bitstream buffer.
            let mut bb = std::mem::zeroed::<NV_ENC_CREATE_BITSTREAM_BUFFER>();
            bb.version = ver_bitstream_buf();
            let st = (api.nvEncCreateBitstreamBuffer.unwrap())(session, &mut bb);
            if st != NV_ENC_SUCCESS || bb.bitstreamBuffer.is_null() {
                return Err(Error::encode(format!(
                    "CreateBitstreamBuffer: {} ({st})",
                    status_name(st)
                )));
            }

            Ok(Self {
                _dll: dll,
                api,
                encoder: session,
                bitstream: bb.bitstreamBuffer,
                profile,
                clock,
                frame_index: 0,
                force_idr: true,
                width: init.encodeWidth,
                height: init.encodeHeight,
                cached_params: None,
                logged_sps: false,
            })
        }
    }

    pub fn force_idr(&mut self) {
        self.force_idr = true;
    }

    pub fn encode(&mut self, nv12: &GpuTextureFrame) -> Result<Option<EncodedFrame>> {
        unsafe {
            let tex = nv12
                .texture()
                .ok_or_else(|| Error::encode("NV12 frame has no GPU texture"))?;

            // Register the D3D11 NV12 texture (zero-copy input).
            let mut reg = std::mem::zeroed::<NV_ENC_REGISTER_RESOURCE>();
            reg.version = ver_register();
            reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            reg.width = self.width;
            reg.height = self.height;
            reg.resourceToRegister = tex.as_raw();
            reg.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
            let st = (self.api.nvEncRegisterResource.unwrap())(self.encoder, &mut reg);
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "RegisterResource: {} ({st})",
                    status_name(st)
                )));
            }

            let mut map = std::mem::zeroed::<NV_ENC_MAP_INPUT_RESOURCE>();
            map.version = ver_map();
            map.registeredResource = reg.registeredResource;
            let st = (self.api.nvEncMapInputResource.unwrap())(self.encoder, &mut map);
            if st != NV_ENC_SUCCESS {
                let _ =
                    (self.api.nvEncUnregisterResource.unwrap())(self.encoder, reg.registeredResource);
                return Err(Error::encode(format!(
                    "MapInputResource: {} ({st})",
                    status_name(st)
                )));
            }

            let pts = self.clock.now();

            let mut pic = std::mem::zeroed::<NV_ENC_PIC_PARAMS>();
            pic.version = ver_pic();
            pic.inputWidth = self.width;
            pic.inputHeight = self.height;
            pic.inputBuffer = map.mappedResource;
            pic.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
            pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
            pic.outputBitstream = self.bitstream;
            pic.inputTimeStamp = pts.as_millis() as u64;
            pic.frameIdx = self.frame_index;
            let requested_idr = self.force_idr;
            if requested_idr {
                pic.encodePicFlags |=
                    (NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS) as u32;
                // Don't clear force_idr yet: if this encode returns NEED_MORE_INPUT
                // (frame buffered, nothing emitted), the IDR request would be lost.
                // Clear it only once we know a bitstream was produced (below).
            }

            let st = (self.api.nvEncEncodePicture.unwrap())(self.encoder, &mut pic);
            let _ = (self.api.nvEncUnmapInputResource.unwrap())(self.encoder, map.mappedResource);
            let _ =
                (self.api.nvEncUnregisterResource.unwrap())(self.encoder, reg.registeredResource);
            self.frame_index += 1;

            if st == NV_ENC_ERR_NEED_MORE_INPUT_I {
                // Frame buffered, nothing emitted — keep force_idr pending so the
                // next emitted frame still carries the forced IDR + SPS/PPS.
                return Ok(None);
            }
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "EncodePicture: {} ({st})",
                    status_name(st)
                )));
            }
            // A bitstream was produced for this submission; the IDR request (if any)
            // has now been satisfied.
            if requested_idr {
                self.force_idr = false;
            }

            let mut lock = std::mem::zeroed::<NV_ENC_LOCK_BITSTREAM>();
            lock.version = ver_lock();
            lock.outputBitstream = self.bitstream;
            let st = (self.api.nvEncLockBitstream.unwrap())(self.encoder, &mut lock);
            if st != NV_ENC_SUCCESS {
                return Err(Error::encode(format!(
                    "LockBitstream: {} ({st})",
                    status_name(st)
                )));
            }

            let data = std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            );
            let annexb = bytes::Bytes::copy_from_slice(data);
            let _ = (self.api.nvEncUnlockBitstream.unwrap())(self.encoder, self.bitstream);

            let is_key = contains_idr(&annexb);
            if is_key {
                if let Some(ps) = extract_parameter_sets(&annexb) {
                    // One-time: log the actual SPS profile_idc / level so we can
                    // confirm the bitstream matches the SDP profile-level-id the
                    // browser negotiated (must share profile_idc + constraint byte,
                    // i.e. the first two SPS bytes, or the browser decodes nothing).
                    if !self.logged_sps {
                        tracing::info!(
                            target: "g9::nvenc",
                            "SPS: profile_idc={} constraint=0x{:02x} level_idc={} -> profile-level-id={:02x}{:02x}{:02x} (SDP must share the first two bytes)",
                            ps.profile_idc, ps.profile_compat, ps.level_idc,
                            ps.profile_idc, ps.profile_compat, ps.level_idc
                        );
                        self.logged_sps = true;
                    }
                    self.cached_params = Some(ps);
                }
            }

            Ok(Some(EncodedFrame {
                codec: VideoCodec::H264,
                kind: if is_key { FrameKind::Key } else { FrameKind::Delta },
                data: annexb,
                annex_b: true,
                pts,
                dts: pts,
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
