//! `g9-rtmp` — RTMP/RTMPS publish transport with an FLV muxer for YouTube Live.
//!
//! Pipeline: NVENC H.264 (AVCC) + AAC → FLV tags → RTMP publish → RTMPS (TLS).

mod amf0;
mod chunk;
mod client;
pub mod flv;
mod handshake;
mod transport;

pub use client::{RtmpClient, RtmpUrl};
pub use transport::RtmpTransport;
