//! Engine configuration, built from CLI flags / env / YAML.
//!
//! Stream keys are treated as secrets: the `Debug` impl is redacted and they are
//! never logged. See `RtmpConfig::redacted_url`.

use crate::profile::OutputKind;

/// Which outputs are enabled.
#[derive(Debug, Clone, Default)]
pub struct Outputs {
    pub webrtc: bool,
    pub youtube: bool,
}

impl Outputs {
    /// Parse `--output webrtc,youtube`.
    pub fn parse(s: &str) -> crate::Result<Self> {
        let mut out = Outputs::default();
        for part in s.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
            match part {
                "webrtc" => out.webrtc = true,
                "youtube" => out.youtube = true,
                other => {
                    return Err(crate::Error::config(format!(
                        "unknown output '{other}' (expected webrtc and/or youtube)"
                    )))
                }
            }
        }
        if !out.webrtc && !out.youtube {
            return Err(crate::Error::config("no outputs selected"));
        }
        Ok(out)
    }

    pub fn kinds(&self) -> Vec<OutputKind> {
        let mut v = Vec::new();
        if self.webrtc {
            v.push(OutputKind::WebRtc);
        }
        if self.youtube {
            v.push(OutputKind::Youtube);
        }
        v
    }
}

/// Video capture + encode geometry.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub display_index: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            display_index: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 8_000_000,
        }
    }
}

/// RTMP/RTMPS destination. Generic so it works for YouTube, Twitch, custom servers.
#[derive(Clone)]
pub struct RtmpConfig {
    /// e.g. "rtmps://a.rtmps.youtube.com/live2" (NO key in the URL).
    pub url: String,
    /// Secret stream key. Never logged.
    pub stream_key: Secret,
}

impl RtmpConfig {
    /// URL safe to log: host/app path only, key never included.
    pub fn redacted_url(&self) -> String {
        // The key is stored separately and never concatenated for logs.
        self.url.clone()
    }
}

impl std::fmt::Debug for RtmpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtmpConfig")
            .field("url", &self.url)
            .field("stream_key", &self.stream_key) // Secret redacts itself
            .finish()
    }
}

/// A secret string whose Debug/Display never reveals the value.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    /// Explicit, auditable access to the raw value. Only the RTMP connect path calls this.
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"***REDACTED***\"")
    }
}

/// Audio toggle + basic params.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub enabled: bool,
    pub sample_rate: u32,
    pub channels: u8,
    /// Opus bitrate for WebRTC.
    pub opus_bitrate_bps: u32,
    /// AAC bitrate for YouTube.
    pub aac_bitrate_bps: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_rate: 48_000,
            channels: 2,
            opus_bitrate_bps: 128_000,
            aac_bitrate_bps: 160_000,
        }
    }
}

/// Local signaling / viewer server bind.
#[derive(Debug, Clone)]
pub struct SignalingConfig {
    pub bind_addr: String,
    pub port: u16,
}

impl Default for SignalingConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1".to_string(),
            port: 8080,
        }
    }
}
