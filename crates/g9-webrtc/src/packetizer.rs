//! H.264 → RTP payloading per RFC 6184 (single NAL unit mode + FU-A fragmentation).
//!
//! We split each access unit into its NAL units, then for each NAL:
//! - if it fits in one RTP payload (<= MTU), send it as a single NAL unit packet;
//! - otherwise fragment it with FU-A.
//!
//! SPS/PPS are sent as their own single-NAL packets ahead of an IDR so a late
//! viewer can decode. This module produces payload byte-vectors; webrtc-rs owns the
//! RTP header, SSRC, sequence numbers and SRTP encryption.
//!
//! Note: the production WebRTC path delegates RFC 6184 payloading to webrtc-rs'
//! `TrackLocalStaticSample`, so these functions aren't on the hot path today. They
//! are kept and unit-tested as a self-contained, verified reference packetizer (and
//! for a future manual-RTP `TrackLocalStaticRTP` path), hence the allow below.
#![allow(dead_code)]

use g9_core::h264::{iter_annexb_nals, nal_type, NAL_IDR};

/// Max RTP payload size (conservative: 1200 keeps us under typical MTU with headers).
pub const MAX_PAYLOAD: usize = 1200;

/// One RTP payload plus whether it is the last packet of the access unit (RTP marker).
#[derive(Debug, Clone)]
pub struct RtpPayload {
    pub data: Vec<u8>,
    pub marker: bool,
}

/// Packetize one Annex-B access unit into RTP payloads.
///
/// `sps`/`pps` are prepended (as single-NAL packets) when the access unit is a
/// keyframe, so viewers that join mid-stream get decoder config with the IDR.
pub fn packetize(
    annexb: &[u8],
    sps: Option<&[u8]>,
    pps: Option<&[u8]>,
) -> Vec<RtpPayload> {
    let mut nals: Vec<Vec<u8>> = Vec::new();

    let is_key = annexb_contains_idr(annexb);
    if is_key {
        if let Some(s) = sps {
            nals.push(s.to_vec());
        }
        if let Some(p) = pps {
            nals.push(p.to_vec());
        }
    }
    for nal in iter_annexb_nals(annexb) {
        // Skip in-band SPS/PPS if we already prepended them, to avoid duplicates.
        if is_key && matches!(nal_type(nal), 7 | 8) {
            continue;
        }
        nals.push(nal.to_vec());
    }

    let mut payloads = Vec::new();
    let last_idx = nals.len().saturating_sub(1);
    for (i, nal) in nals.iter().enumerate() {
        let is_last_nal = i == last_idx;
        if nal.len() <= MAX_PAYLOAD {
            payloads.push(RtpPayload {
                data: nal.clone(),
                marker: is_last_nal,
            });
        } else {
            fragment_fu_a(nal, is_last_nal, &mut payloads);
        }
    }
    payloads
}

fn annexb_contains_idr(annexb: &[u8]) -> bool {
    iter_annexb_nals(annexb)
        .iter()
        .any(|n| nal_type(n) == NAL_IDR)
}

/// FU-A fragmentation (RFC 6184 §5.8).
fn fragment_fu_a(nal: &[u8], is_last_nal: bool, out: &mut Vec<RtpPayload>) {
    let header = nal[0];
    let nri = header & 0x60;
    let typ = header & 0x1F;
    let fu_indicator = nri | 28; // FU-A type = 28
    let payload = &nal[1..]; // skip original NAL header

    // Reserve 2 bytes per fragment for FU indicator + FU header.
    let chunk = MAX_PAYLOAD - 2;
    let total = payload.len();
    let mut offset = 0;
    let mut first = true;
    while offset < total {
        let end = (offset + chunk).min(total);
        let is_last_frag = end == total;
        let mut fu_header = typ;
        if first {
            fu_header |= 0x80; // Start bit
        }
        if is_last_frag {
            fu_header |= 0x40; // End bit
        }
        let mut data = Vec::with_capacity(2 + (end - offset));
        data.push(fu_indicator);
        data.push(fu_header);
        data.extend_from_slice(&payload[offset..end]);
        out.push(RtpPayload {
            data,
            // RTP marker set on the final fragment of the final NAL of the AU.
            marker: is_last_nal && is_last_frag,
        });
        offset = end;
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_nal_small() {
        // One small IDR NAL.
        let au = [0, 0, 0, 1, 0x65, 1, 2, 3];
        let p = packetize(&au, None, None);
        assert_eq!(p.len(), 1);
        assert!(p[0].marker);
        assert_eq!(p[0].data, vec![0x65, 1, 2, 3]);
    }

    #[test]
    fn fu_a_fragmentation() {
        // One large non-IDR NAL that must fragment.
        let mut au = vec![0, 0, 0, 1, 0x61];
        au.extend(std::iter::repeat(0xAB).take(MAX_PAYLOAD * 2));
        let p = packetize(&au, None, None);
        assert!(p.len() >= 3);
        // First fragment has Start bit.
        assert_eq!(p[0].data[0] & 0x1F, 28); // FU-A
        assert_eq!(p[0].data[1] & 0x80, 0x80); // S bit
        // Last fragment has End bit + marker.
        let last = p.last().unwrap();
        assert_eq!(last.data[1] & 0x40, 0x40); // E bit
        assert!(last.marker);
    }

    #[test]
    fn prepends_sps_pps_on_keyframe() {
        let au = [0, 0, 0, 1, 0x65, 9, 9]; // IDR
        let sps = [0x67, 0x42, 0, 0x1f];
        let pps = [0x68, 0xce];
        let p = packetize(&au, Some(&sps), Some(&pps));
        assert_eq!(p.len(), 3); // SPS, PPS, IDR
        assert_eq!(p[0].data[0] & 0x1F, 7);
        assert_eq!(p[1].data[0] & 0x1F, 8);
        assert_eq!(p[2].data[0] & 0x1F, 5);
    }
}
