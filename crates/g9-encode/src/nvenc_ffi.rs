//! Minimal, real FFI to the NVIDIA Video Codec SDK `nvEncodeAPI` (NVENC).
//!
//! [PENDING-HW] Windows+NVIDIA only. This binds the subset of `nvEncodeAPI.h` the
//! encoder uses: the function list struct, GUIDs, the key parameter structs and the
//! `NvEncodeAPICreateInstance` entry point exported by `nvEncodeAPI64.dll`.
//!
//! We deliberately bind by hand (rather than bindgen at build time) so the code is
//! reviewable and does not require the SDK headers to be present to *compile* the
//! workspace off-target. On the Windows build, `nvEncodeAPI64.dll` is loaded at
//! runtime (it ships with the NVIDIA driver), so there is no link-time SDK
//! dependency. Struct layouts follow SDK 12/13 (NVENCAPI_VERSION 12.x).
//!
//! Only the fields we set are modelled precisely; reserved regions are preserved as
//! padding arrays so the ABI matches. If NVIDIA bumps the struct version, update
//! `NVENCAPI_VERSION` and the `version` fields accordingly.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::os::raw::{c_int, c_void};

pub type NVENCSTATUS = c_int;
pub const NV_ENC_SUCCESS: NVENCSTATUS = 0;

/// NVENC GUID (matches the SDK's `GUID`).
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct GUID {
    pub Data1: u32,
    pub Data2: u16,
    pub Data3: u16,
    pub Data4: [u8; 8],
}

// --- Codec GUIDs ---
// NV_ENC_CODEC_H264_GUID = 6BC82762-4E63-4ca4-AA85-1E50F321F6BF
pub const NV_ENC_CODEC_H264_GUID: GUID = GUID {
    Data1: 0x6BC82762,
    Data2: 0x4E63,
    Data3: 0x4ca4,
    Data4: [0xAA, 0x85, 0x1E, 0x50, 0xF3, 0x21, 0xF6, 0xBF],
};
// NV_ENC_CODEC_HEVC_GUID = 790CDC88-4522-4d7b-9425-BDA9975F7603
pub const NV_ENC_CODEC_HEVC_GUID: GUID = GUID {
    Data1: 0x790CDC88,
    Data2: 0x4522,
    Data3: 0x4d7b,
    Data4: [0x94, 0x25, 0xBD, 0xA9, 0x97, 0x5F, 0x76, 0x03],
};

// --- Preset GUIDs (P1..P7). P4 = balanced, P5 = a bit more quality. ---
// NV_ENC_PRESET_P4_GUID = 90A7B826-DF06-4862-B9D2-CD6D73A08681
pub const NV_ENC_PRESET_P4_GUID: GUID = GUID {
    Data1: 0x90A7B826,
    Data2: 0xDF06,
    Data3: 0x4862,
    Data4: [0xB9, 0xD2, 0xCD, 0x6D, 0x73, 0xA0, 0x86, 0x81],
};
// NV_ENC_PRESET_P5_GUID = 21C6E6B4-297A-4CBA-998F-B6CBDE72ADE3
pub const NV_ENC_PRESET_P5_GUID: GUID = GUID {
    Data1: 0x21C6E6B4,
    Data2: 0x297A,
    Data3: 0x4CBA,
    Data4: [0x99, 0x8F, 0xB6, 0xCB, 0xDE, 0x72, 0xAD, 0xE3],
};

// --- H.264 profile GUID (high) = E7CBC309-4F7A-4b89-AF2A-D537C92BE310 ---
pub const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = GUID {
    Data1: 0xE7CBC309,
    Data2: 0x4F7A,
    Data3: 0x4b89,
    Data4: [0xAF, 0x2A, 0xD5, 0x37, 0xC9, 0x2B, 0xE3, 0x10],
};

// --- Tuning info ---
pub type NV_ENC_TUNING_INFO = c_int;
pub const NV_ENC_TUNING_INFO_UNDEFINED: NV_ENC_TUNING_INFO = 0;
pub const NV_ENC_TUNING_INFO_HIGH_QUALITY: NV_ENC_TUNING_INFO = 1;
pub const NV_ENC_TUNING_INFO_LOW_LATENCY: NV_ENC_TUNING_INFO = 2;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: NV_ENC_TUNING_INFO = 3;
pub const NV_ENC_TUNING_INFO_LOSSLESS: NV_ENC_TUNING_INFO = 4;

// --- Device type --- (per nvEncodeAPI.h: DIRECTX=0, CUDA=1, OPENGL=2)
pub type NV_ENC_DEVICE_TYPE = c_int;
pub const NV_ENC_DEVICE_TYPE_DIRECTX: NV_ENC_DEVICE_TYPE = 0;
pub const NV_ENC_DEVICE_TYPE_CUDA: NV_ENC_DEVICE_TYPE = 1;

// --- Buffer format ---
pub type NV_ENC_BUFFER_FORMAT = c_int;
pub const NV_ENC_BUFFER_FORMAT_NV12: NV_ENC_BUFFER_FORMAT = 0x01;
pub const NV_ENC_BUFFER_FORMAT_ARGB: NV_ENC_BUFFER_FORMAT = 0x1000000;

// --- Input resource type ---
pub type NV_ENC_INPUT_RESOURCE_TYPE = c_int;
pub const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: NV_ENC_INPUT_RESOURCE_TYPE = 0;

// --- Picture structure / flags ---
pub type NV_ENC_PIC_STRUCT = c_int;
pub const NV_ENC_PIC_STRUCT_FRAME: NV_ENC_PIC_STRUCT = 1;

pub type NV_ENC_PIC_FLAGS = u32;
pub const NV_ENC_PIC_FLAG_FORCEIDR: NV_ENC_PIC_FLAGS = 0x4;
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: NV_ENC_PIC_FLAGS = 0x8;

// --- Rate control mode ---
pub type NV_ENC_PARAMS_RC_MODE = c_int;
pub const NV_ENC_PARAMS_RC_CBR: NV_ENC_PARAMS_RC_MODE = 0x2;
pub const NV_ENC_PARAMS_RC_VBR: NV_ENC_PARAMS_RC_MODE = 0x4;

/// Version packing, matching the SDK header:
///   NVENCAPI_VERSION           = (MAJOR | (MINOR << 24))
///   NVENCAPI_STRUCT_VERSION(v) = NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
///
/// Blackwell drivers ship NVENC SDK 13.x and reject the older 12.x apiVersion with
/// NV_ENC_ERR_INVALID_VERSION (15) at OpenEncodeSessionEx. We therefore target 13.0.
/// The SDK major/minor can be overridden at runtime via the G9_NVENC_MAJOR /
/// G9_NVENC_MINOR env vars if a given driver needs a different value — see
/// `api_version()`.
pub const NVENCAPI_MAJOR: u32 = 13;
pub const NVENCAPI_MINOR: u32 = 0;

/// Compile-time default API version (13.0).
pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR | (NVENCAPI_MINOR << 24);

/// Runtime-resolved API version, allowing an env override without a rebuild. This is
/// invaluable for matching whatever SDK the installed driver expects.
pub fn api_version() -> u32 {
    let major = std::env::var("G9_NVENC_MAJOR")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(NVENCAPI_MAJOR);
    let minor = std::env::var("G9_NVENC_MINOR")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(NVENCAPI_MINOR);
    major | (minor << 24)
}

pub const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

/// Runtime struct-version using the env-overridable api_version().
pub fn struct_version_rt(ver: u32) -> u32 {
    api_version() | (ver << 16) | (0x7 << 28)
}

/// `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS`
#[repr(C)]
pub struct NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    pub version: u32,
    pub deviceType: NV_ENC_DEVICE_TYPE,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub apiVersion: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

/// A subset of `NV_ENC_INITIALIZE_PARAMS` with the fields we set; reserved padding
/// keeps the ABI. The real struct is large — padding sizes follow SDK 12.x.
#[repr(C)]
pub struct NV_ENC_INITIALIZE_PARAMS {
    pub version: u32,
    pub encodeGUID: GUID,
    pub presetGUID: GUID,
    pub encodeWidth: u32,
    pub encodeHeight: u32,
    pub darWidth: u32,
    pub darHeight: u32,
    pub frameRateNum: u32,
    pub frameRateDen: u32,
    pub enableEncodeAsync: u32,
    pub enablePTD: u32,
    pub reportSliceOffsets: u32,
    pub enableSubFrameWrite: u32,
    pub enableExternalMEHints: u32,
    pub enableMEOnlyMode: u32,
    pub enableWeightedPrediction: u32,
    pub enableOutputInVidmem: u32,
    pub reservedBitFields: u32,
    pub privDataSize: u32,
    pub privData: *mut c_void,
    pub encodeConfig: *mut NV_ENC_CONFIG,
    pub maxEncodeWidth: u32,
    pub maxEncodeHeight: u32,
    pub maxMEHintCountsPerBlock: [u32; 2],
    pub tuningInfo: NV_ENC_TUNING_INFO,
    pub bufferFormat: NV_ENC_BUFFER_FORMAT,
    pub reserved: [u32; 287],
    pub reserved2: [*mut c_void; 64],
}

/// Opaque-ish `NV_ENC_CONFIG`. We model the fields needed for CBR + GOP + rc; the
/// rest is reserved padding. For a hand FFI we fill rateControlParams via the
/// nested union representation. SDK layout: large; padding approximates it.
#[repr(C)]
pub struct NV_ENC_CONFIG {
    pub version: u32,
    pub profileGUID: GUID,
    pub gopLength: u32,
    pub frameIntervalP: i32,
    pub monoChromeEncoding: u32,
    pub frameFieldMode: c_int,
    pub mvPrecision: c_int,
    pub rcParams: NV_ENC_RC_PARAMS,
    // The codec-specific config union follows; we leave it zeroed (defaults from
    // the preset config query are used) and keep padding for ABI size.
    pub encodeCodecConfig_padding: [u8; 1024],
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NV_ENC_RC_PARAMS {
    pub version: u32,
    pub rateControlMode: NV_ENC_PARAMS_RC_MODE,
    pub constQP: [u32; 3],
    pub averageBitRate: u32,
    pub maxBitRate: u32,
    pub vbvBufferSize: u32,
    pub vbvInitialDelay: u32,
    pub flags: u32,
    pub enableMinQP_etc: [u32; 6],
    pub targetQuality: u8,
    pub targetQualityLSB: u8,
    pub lookaheadDepth: u16,
    pub reserved1: u32,
    pub reserved: [u32; 7],
    pub reserved2: [*mut c_void; 0],
}

/// `NV_ENC_PRESET_CONFIG` returned by GetEncodePresetConfigEx.
#[repr(C)]
pub struct NV_ENC_PRESET_CONFIG {
    pub version: u32,
    pub presetCfg: NV_ENC_CONFIG,
    pub reserved: [u32; 255],
    pub reserved2: [*mut c_void; 64],
}

/// `NV_ENC_REGISTER_RESOURCE` — registers the D3D11 NV12 texture (zero-copy input).
#[repr(C)]
pub struct NV_ENC_REGISTER_RESOURCE {
    pub version: u32,
    pub resourceType: NV_ENC_INPUT_RESOURCE_TYPE,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub subResourceIndex: u32,
    pub resourceToRegister: *mut c_void, // ID3D11Texture2D*
    pub registeredResource: *mut c_void, // out
    pub bufferFormat: NV_ENC_BUFFER_FORMAT,
    pub bufferUsage: c_int,
    pub pInputFencePoint: *mut c_void,
    pub reserved1: [u32; 247],
    pub reserved2: [*mut c_void; 60],
}

/// `NV_ENC_MAP_INPUT_RESOURCE`
#[repr(C)]
pub struct NV_ENC_MAP_INPUT_RESOURCE {
    pub version: u32,
    pub subResourceIndex: u32,
    pub inputResource: *mut c_void,
    pub registeredResource: *mut c_void,
    pub mappedResource: *mut c_void, // out
    pub mappedBufferFmt: NV_ENC_BUFFER_FORMAT, // out
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 63],
}

/// `NV_ENC_CREATE_BITSTREAM_BUFFER`
#[repr(C)]
pub struct NV_ENC_CREATE_BITSTREAM_BUFFER {
    pub version: u32,
    pub size: u32,
    pub memoryHeap: c_int,
    pub reserved: u32,
    pub bitstreamBuffer: *mut c_void, // out
    pub bitstreamBufferPtr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

/// `NV_ENC_PIC_PARAMS` (subset + padding).
#[repr(C)]
pub struct NV_ENC_PIC_PARAMS {
    pub version: u32,
    pub inputWidth: u32,
    pub inputHeight: u32,
    pub inputPitch: u32,
    pub encodePicFlags: u32,
    pub frameIdx: u32,
    pub inputTimeStamp: u64,
    pub inputDuration: u64,
    pub inputBuffer: *mut c_void,
    pub outputBitstream: *mut c_void,
    pub completionEvent: *mut c_void,
    pub bufferFmt: NV_ENC_BUFFER_FORMAT,
    pub pictureStruct: NV_ENC_PIC_STRUCT,
    pub pictureType: c_int,
    pub codecPicParams_padding: [u8; 1024],
    pub reserved: [u32; 128],
    pub reserved2: [*mut c_void; 64],
}

/// `NV_ENC_LOCK_BITSTREAM` — field order/sizes match nvEncodeAPI.h exactly.
#[repr(C)]
pub struct NV_ENC_LOCK_BITSTREAM {
    pub version: u32,
    pub bitfields: u32, // doNotWait:1, ltrFrame:1, getRCStats:1, reservedBitFields:29
    pub outputBitstream: *mut c_void,
    pub sliceOffsets: *mut u32,
    pub frameIdx: u32,
    pub hwEncodeStatus: u32,
    pub numSlices: u32,
    pub bitstreamSizeInBytes: u32, // out
    pub outputTimeStamp: u64,
    pub outputDuration: u64,
    pub bitstreamBufferPtr: *mut c_void, // out: pointer to the encoded data
    pub pictureType: c_int,              // out (NV_ENC_PIC_TYPE)
    pub pictureStruct: NV_ENC_PIC_STRUCT,
    pub frameAvgQP: u32,
    pub frameSatd: u32,
    pub ltrFrameIdx: u32,
    pub ltrFrameBitmap: u32,
    pub temporalId: u32,
    pub intraMBCount: u32,
    pub interMBCount: u32,
    pub averageMVX: i32,
    pub averageMVY: i32,
    pub alphaLayerSizeInBytes: u32,
    pub outputStatsPtrSize: u32,
    pub reserved: u32,
    pub outputStatsPtr: *mut c_void,
    pub frameIdxDisplay: u32,
    pub reserved1: [u32; 219],
    pub reserved2: [*mut c_void; 63],
    pub reservedInternal: [u32; 8],
}

/// The NVENC function-pointer table (`NV_ENCODE_API_FUNCTION_LIST`). We only type
/// the entries we call; the rest are opaque pointers to preserve layout/order.
//
// IMPORTANT: every function-pointer field is `Option<extern "C" fn ...>`. These are
// non-null fn pointers; wrapping in Option makes the all-zero bit pattern valid
// (= None), so `mem::zeroed()` on this struct is sound. `NvEncodeAPICreateInstance`
// fills them in. Call sites use `(self.api.field.unwrap())(...)`.
// Field order and count match nvEncodeAPI.h EXACTLY — the table is populated by
// NvEncodeAPICreateInstance, so any misordering makes a call land on the wrong
// pointer (symptom: NV_ENC_ERR_INVALID_VERSION / status 15 at OpenEncodeSessionEx).
// Entries we don't call are left as `*mut c_void` placeholders of the right size.
// Final padding is reserved2[275], per the header.
#[repr(C)]
pub struct NV_ENCODE_API_FUNCTION_LIST {
    pub version: u32,
    pub reserved: u32,
    pub nvEncOpenEncodeSession: *mut c_void,
    pub nvEncGetEncodeGUIDCount: *mut c_void,
    pub nvEncGetEncodeProfileGUIDCount: *mut c_void,
    pub nvEncGetEncodeProfileGUIDs: *mut c_void,
    pub nvEncGetEncodeGUIDs: *mut c_void,
    pub nvEncGetInputFormatCount: *mut c_void,
    pub nvEncGetInputFormats: *mut c_void,
    pub nvEncGetEncodeCaps: *mut c_void,
    pub nvEncGetEncodePresetCount: *mut c_void,
    pub nvEncGetEncodePresetGUIDs: *mut c_void,
    pub nvEncGetEncodePresetConfig: *mut c_void,
    pub nvEncInitializeEncoder:
        Option<extern "C" fn(*mut c_void, *mut NV_ENC_INITIALIZE_PARAMS) -> NVENCSTATUS>,
    pub nvEncCreateInputBuffer: *mut c_void,
    pub nvEncDestroyInputBuffer: *mut c_void,
    pub nvEncCreateBitstreamBuffer:
        Option<extern "C" fn(*mut c_void, *mut NV_ENC_CREATE_BITSTREAM_BUFFER) -> NVENCSTATUS>,
    pub nvEncDestroyBitstreamBuffer:
        Option<extern "C" fn(*mut c_void, *mut c_void) -> NVENCSTATUS>,
    pub nvEncEncodePicture:
        Option<extern "C" fn(*mut c_void, *mut NV_ENC_PIC_PARAMS) -> NVENCSTATUS>,
    pub nvEncLockBitstream:
        Option<extern "C" fn(*mut c_void, *mut NV_ENC_LOCK_BITSTREAM) -> NVENCSTATUS>,
    pub nvEncUnlockBitstream: Option<extern "C" fn(*mut c_void, *mut c_void) -> NVENCSTATUS>,
    pub nvEncLockInputBuffer: *mut c_void,
    pub nvEncUnlockInputBuffer: *mut c_void,
    pub nvEncGetEncodeStats: *mut c_void,
    pub nvEncGetSequenceParams: *mut c_void,
    pub nvEncRegisterAsyncEvent: *mut c_void,
    pub nvEncUnregisterAsyncEvent: *mut c_void,
    pub nvEncMapInputResource:
        Option<extern "C" fn(*mut c_void, *mut NV_ENC_MAP_INPUT_RESOURCE) -> NVENCSTATUS>,
    pub nvEncUnmapInputResource: Option<extern "C" fn(*mut c_void, *mut c_void) -> NVENCSTATUS>,
    pub nvEncDestroyEncoder: Option<extern "C" fn(*mut c_void) -> NVENCSTATUS>,
    pub nvEncInvalidateRefFrames: *mut c_void,
    pub nvEncOpenEncodeSessionEx: Option<
        extern "C" fn(
            *mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
            *mut *mut c_void,
        ) -> NVENCSTATUS,
    >,
    pub nvEncRegisterResource:
        Option<extern "C" fn(*mut c_void, *mut NV_ENC_REGISTER_RESOURCE) -> NVENCSTATUS>,
    pub nvEncUnregisterResource: Option<extern "C" fn(*mut c_void, *mut c_void) -> NVENCSTATUS>,
    pub nvEncReconfigureEncoder: *mut c_void,
    pub reserved1: *mut c_void,
    pub nvEncCreateMVBuffer: *mut c_void,
    pub nvEncDestroyMVBuffer: *mut c_void,
    pub nvEncRunMotionEstimationOnly: *mut c_void,
    pub nvEncGetLastErrorString: *mut c_void,
    pub nvEncSetIOCudaStreams: *mut c_void,
    // GetEncodePresetConfigEx lives HERE, near the end — not next to PresetConfig.
    pub nvEncGetEncodePresetConfigEx: Option<
        extern "C" fn(
            *mut c_void,
            GUID,
            GUID,
            NV_ENC_TUNING_INFO,
            *mut NV_ENC_PRESET_CONFIG,
        ) -> NVENCSTATUS,
    >,
    pub nvEncGetSequenceParamEx: *mut c_void,
    pub nvEncRestoreEncoderState: *mut c_void,
    pub nvEncLookaheadPicture: *mut c_void,
    pub reserved2: [*mut c_void; 275],
}

pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(5);
pub const NV_ENC_CONFIG_VER: u32 = struct_version(7) | (1 << 31);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
pub const NV_ENC_RC_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(3);
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2);

// The single exported entry point of nvEncodeAPI64.dll. We load it dynamically
// (LoadLibrary + GetProcAddress) at runtime so there is no SDK import-lib at build.
pub type PFN_NvEncodeAPICreateInstance =
    extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
