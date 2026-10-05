//! H.264 bitstream helpers — pure byte manipulation, platform-independent.
//!
//! Used by both transports:
//! - WebRTC RTP packetizer walks Annex-B NAL units.
//! - RTMP/FLV muxer needs AVCC (length-prefixed) NALs + SPS/PPS for the
//!   AVCDecoderConfigurationRecord.

use crate::frame::ParameterSets;
use bytes::Bytes;

/// NAL unit types we care about.
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_IDR: u8 = 5;

/// Iterate Annex-B NAL units (payload between `00 00 01` / `00 00 00 01` start codes).
/// Returns slices WITHOUT the start code.
pub fn iter_annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let n = data.len();
    // Find first start code.
    let mut start = find_start_code(data, 0);
    while let Some((sc_pos, sc_len)) = start {
        let nal_start = sc_pos + sc_len;
        // Next start code marks the end of this NAL.
        let next = find_start_code(data, nal_start);
        let nal_end = match next {
            Some((pos, _)) => pos,
            None => n,
        };
        if nal_end > nal_start {
            nals.push(&data[nal_start..nal_end]);
        }
        start = next;
        i = nal_end;
        if i >= n {
            break;
        }
    }
    nals
}

/// Returns (position_of_start_code, start_code_length) at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let n = data.len();
    let mut i = from;
    while i + 3 <= n {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some((i, 3));
            }
            if i + 4 <= n && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

/// NAL type is the low 5 bits of the first byte.
pub fn nal_type(nal: &[u8]) -> u8 {
    if nal.is_empty() {
        0
    } else {
        nal[0] & 0x1F
    }
}

/// Extract SPS/PPS from an Annex-B access unit, if present.
pub fn extract_parameter_sets(annexb: &[u8]) -> Option<ParameterSets> {
    let mut sps: Option<Bytes> = None;
    let mut pps: Option<Bytes> = None;
    for nal in iter_annexb_nals(annexb) {
        match nal_type(nal) {
            NAL_SPS => sps = Some(Bytes::copy_from_slice(nal)),
            NAL_PPS => pps = Some(Bytes::copy_from_slice(nal)),
            _ => {}
        }
    }
    let sps = sps?;
    let pps = pps?;
    // SPS payload: byte0 = NAL header, byte1 = profile_idc, byte2 = constraints,
    // byte3 = level_idc.
    let (profile_idc, profile_compat, level_idc) = if sps.len() >= 4 {
        (sps[1], sps[2], sps[3])
    } else {
        (0, 0, 0)
    };
    Some(ParameterSets {
        sps,
        pps,
        profile_idc,
        profile_compat,
        level_idc,
    })
}

/// True if the access unit contains an IDR NAL.
pub fn contains_idr(annexb: &[u8]) -> bool {
    iter_annexb_nals(annexb)
        .iter()
        .any(|n| nal_type(n) == NAL_IDR)
}

/// Convert an Annex-B access unit to AVCC (4-byte length-prefixed NALs), skipping
/// SPS/PPS (which live out-of-band in the FLV sequence header).
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annexb.len());
    for nal in iter_annexb_nals(annexb) {
        match nal_type(nal) {
            NAL_SPS | NAL_PPS => continue, // carried in the config record
            _ => {
                let len = nal.len() as u32;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_annexb_nals() {
        // start(4) SPS, start(3) PPS, start(4) IDR
        let data = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, // SPS (type 7)
            0, 0, 1, 0x68, 0xCE, // PPS (type 8)
            0, 0, 0, 1, 0x65, 0xAA, 0xBB, // IDR (type 5)
        ];
        let nals = iter_annexb_nals(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0]), NAL_SPS);
        assert_eq!(nal_type(nals[1]), NAL_PPS);
        assert_eq!(nal_type(nals[2]), NAL_IDR);
        assert!(contains_idr(&data));

        let ps = extract_parameter_sets(&data).unwrap();
        assert_eq!(ps.profile_idc, 0x42);
        assert_eq!(ps.level_idc, 0x1f);

        // AVCC should contain only the IDR, length-prefixed.
        let avcc = annexb_to_avcc(&data);
        assert_eq!(&avcc[0..4], &3u32.to_be_bytes()); // IDR is 3 bytes
        assert_eq!(avcc[4], 0x65);
    }
}
