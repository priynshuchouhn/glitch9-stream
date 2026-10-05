//! `g9-core` — shared, platform-independent contracts for the Glitch9 streaming engine.
//!
//! This crate deliberately contains **no** Windows/NVIDIA code so it builds on any host.
//! It defines the types that flow between capture → convert → encode → transport, plus
//! the traits that keep those stages decoupled (so capture is never duplicated and a
//! transport can be swapped without touching the media path).

pub mod config;
pub mod error;
pub mod frame;
pub mod h264;
pub mod metrics;
pub mod profile;
pub mod time;
pub mod transport;

pub use error::{Error, Result};
pub use frame::{EncodedFrame, FrameKind, VideoCodec, VideoFormat};
pub use profile::{EncoderProfile, OutputKind, RateControl};
pub use time::PtsClock;
pub use transport::{AudioPacket, MediaTransport, TransportState};
