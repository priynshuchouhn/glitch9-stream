//! AAC AudioSpecificConfig construction — pure byte logic, platform-independent, tested.
//! Used by the AAC encoder to produce the FLV AAC sequence header.

/// AAC-LC object type.
pub const AAC_LC: u8 = 2;

/// Build a 2-byte AAC AudioSpecificConfig (ISO 14496-3):
///   5 bits audioObjectType, 4 bits samplingFreqIndex, 4 bits channelConfig, pad.
pub fn audio_specific_config(object_type: u8, sample_rate: u32, channels: u8) -> [u8; 2] {
    let freq_index = sample_rate_index(sample_rate);
    let b0 = (object_type << 3) | (freq_index >> 1);
    let b1 = ((freq_index & 1) << 7) | ((channels & 0x0F) << 3);
    [b0, b1]
}

/// MPEG-4 sampling frequency index table.
pub fn sample_rate_index(sr: u32) -> u8 {
    match sr {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        7350 => 12,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_for_48k_stereo_aac_lc() {
        let asc = audio_specific_config(AAC_LC, 48000, 2);
        assert_eq!(asc, [0x11, 0x90]);
    }

    #[test]
    fn asc_for_44100_stereo() {
        let asc = audio_specific_config(AAC_LC, 44100, 2);
        assert_eq!(asc, [0x12, 0x10]);
    }
}
