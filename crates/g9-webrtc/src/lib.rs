//! `g9-webrtc` — WebRTC view-only transport: H.264 RTP packetization + DTLS-SRTP via
//! webrtc-rs, plus a local signaling server and the browser viewer assets.

mod packetizer;
pub mod signaling;
mod transport;
mod whip;
mod whep_sub;

pub use transport::WebRtcTransport;
pub use whip::WhipTransport;
pub use whep_sub::{FacecamSample, WhepSubscriber};
