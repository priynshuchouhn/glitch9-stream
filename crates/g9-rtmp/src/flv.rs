//! FLV tag muxing for RTMP publish (pure byte logic, platform-independent, tested).
//!
//! YouTube ingests H.264 (AVC) video + AAC audio inside FLV over RTMP. We produce:
//! - A **video sequence header** tag (AVCDecoderConfigurationRecord) once at start
//!   and again after every reconnect, built from SPS/PPS.
//! - Per-frame **AVC NALU** video tags (AVCC, length-prefixed) with composition time 0
//!   (we disable B-frames, so DTS == PTS).
//! - An **AAC sequence header** tag (AudioSpecificConfig) once, then per-frame AAC tags.
//!
//! Timestamps are milliseconds from the unified PtsClock.

use bytes::{BufMut, Bytes, BytesMut};
use g9_core::frame::ParameterSets;

/// FLV tag types.
const TAG_AUDIO: u8 = 8;
const TAG_VIDEO: u8 = 9;

/// Build the AVCDecoderConfigurationRecord (ISO 14496-15) from SPS/PPS.
pub fn avc_decoder_config_record(ps: &ParameterSets) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(1); // configurationVersion
    b.put_u8(ps.profile_idc);
    b.put_u8(ps.profile_compat);
    b.put_u8(ps.level_idc);
    b.put_u8(0xFF); // 6 bits reserved + 2 bits lengthSizeMinusOne (=3 → 4-byte NAL length)
    b.put_u8(0xE1); // 3 bits reserved + 5 bits numOfSPS (=1)
    b.put_u16(ps.sps.len() as u16);
    b.put_slice(&ps.sps);
    b.put_u8(1); // numOfPPS
    b.put_u16(ps.pps.len() as u16);
    b.put_slice(&ps.pps);
    b.freeze()
}

/// Video sequence header FLV tag payload (AVC seq header). FrameType=1 (keyframe),
/// CodecID=7 (AVC), AVCPacketType=0 (seq header), CompositionTime=0.
pub fn video_sequence_header(ps: &ParameterSets) -> Bytes {
    let cfg = avc_decoder_config_record(ps);
    let mut b = BytesMut::new();
    b.put_u8(0x17); // 1=keyframe <<4 | 7=AVC
    b.put_u8(0x00); // AVCPacketType = seq header
    b.put_u8(0);
    b.put_u8(0);
    b.put_u8(0); // composition time (3 bytes)
    b.put_slice(&cfg);
    b.freeze()
}

/// Video NALU FLV tag payload for one access unit.
/// `avcc` is length-prefixed NALs (SPS/PPS stripped — they're in the seq header).
pub fn video_nalu(avcc: &[u8], is_keyframe: bool) -> Bytes {
    let mut b = BytesMut::new();
    let frame_type = if is_keyframe { 0x1 } else { 0x2 };
    b.put_u8((frame_type << 4) | 7); // AVC
    b.put_u8(0x01); // AVCPacketType = NALU
    b.put_u8(0);
    b.put_u8(0);
    b.put_u8(0); // composition time = 0 (no B-frames)
    b.put_slice(avcc);
    b.freeze()
}

/// AAC audio sequence header FLV tag payload (AudioSpecificConfig).
/// SoundFormat=10 (AAC), SoundRate=3 (44/48k flag), SoundSize=1 (16-bit),
/// SoundType=1 (stereo); AACPacketType=0 (seq header).
pub fn audio_sequence_header(asc: &[u8]) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(0xAF); // 10<<4 | 3<<2 | 1<<1 | 1
    b.put_u8(0x00); // AAC seq header
    b.put_slice(asc);
    b.freeze()
}

/// AAC audio data FLV tag payload (raw AAC frame, AACPacketType=1).
pub fn audio_data(aac: &[u8]) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(0xAF);
    b.put_u8(0x01); // AAC raw
    b.put_slice(aac);
    b.freeze()
}

/// Wrap a tag payload in a full FLV tag (used for file-based debugging / tests).
/// Over RTMP the payload is sent as an RTMP message of the matching type instead;
/// this function documents the exact tag framing.
pub fn flv_tag(tag_type_video: bool, payload: &[u8], timestamp_ms: u32) -> Bytes {
    let tag_type = if tag_type_video { TAG_VIDEO } else { TAG_AUDIO };
    let mut b = BytesMut::new();
    b.put_u8(tag_type);
    // DataSize (24-bit)
    let size = payload.len() as u32;
    b.put_u8((size >> 16) as u8);
    b.put_u8((size >> 8) as u8);
    b.put_u8(size as u8);
    // Timestamp (24-bit) + extended (8-bit)
    b.put_u8((timestamp_ms >> 16) as u8);
    b.put_u8((timestamp_ms >> 8) as u8);
    b.put_u8(timestamp_ms as u8);
    b.put_u8((timestamp_ms >> 24) as u8);
    // StreamID (always 0)
    b.put_u8(0);
    b.put_u8(0);
    b.put_u8(0);
    b.put_slice(payload);
    // PreviousTagSize
    b.put_u32(11 + size);
    b.freeze()
}

/// Which RTMP message type a tag payload should be sent as.
#[derive(Debug, Clone, Copy)]
pub enum RtmpMediaKind {
    Video,
    Audio,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn ps() -> ParameterSets {
        ParameterSets {
            sps: Bytes::from_static(&[0x67, 0x42, 0x00, 0x1f, 0x11]),
            pps: Bytes::from_static(&[0x68, 0xce, 0x3c]),
            profile_idc: 0x42,
            profile_compat: 0x00,
            level_idc: 0x1f,
        }
    }

    #[test]
    fn builds_avcc_config_record() {
        let rec = avc_decoder_config_record(&ps());
        assert_eq!(rec[0], 1); // version
        assert_eq!(rec[1], 0x42); // profile
        assert_eq!(rec[3], 0x1f); // level
        assert_eq!(rec[4], 0xFF); // lengthSizeMinusOne = 3
        assert_eq!(rec[5], 0xE1); // numSPS = 1
    }

    #[test]
    fn video_seq_header_marks_keyframe_avc() {
        let h = video_sequence_header(&ps());
        assert_eq!(h[0], 0x17); // keyframe + AVC
        assert_eq!(h[1], 0x00); // seq header
    }

    #[test]
    fn video_nalu_tag_flags() {
        let key = video_nalu(&[0, 0, 0, 2, 0x65, 0x88], true);
        assert_eq!(key[0], 0x17);
        assert_eq!(key[1], 0x01);
        let delta = video_nalu(&[0, 0, 0, 2, 0x41, 0x88], false);
        assert_eq!(delta[0], 0x27);
    }

    #[test]
    fn aac_tags() {
        let seq = audio_sequence_header(&[0x11, 0x90]);
        assert_eq!(seq[0], 0xAF);
        assert_eq!(seq[1], 0x00);
        let data = audio_data(&[0xDE, 0xAD]);
        assert_eq!(data[1], 0x01);
    }
}
