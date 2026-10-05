//! Error types shared across the engine.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("capture error: {0}")]
    Capture(String),

    #[error("color conversion error: {0}")]
    Convert(String),

    #[error("encoder error: {0}")]
    Encode(String),

    #[error("audio error: {0}")]
    Audio(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("configuration error: {0}")]
    Config(String),

    /// The capture target changed (resolution/display reconnect). The pipeline
    /// should re-create the duplication + encoder rather than crash.
    #[error("capture target changed; reinitialization required")]
    CaptureReinit,

    /// A GPU-backed DXGI output was not available. On RDSH/RDP this usually means
    /// `UseWddmDriver=1` is not set, so DXGI only sees the Microsoft Remote Display
    /// Adapter. See docs/RUN.md.
    #[error("no GPU-backed DXGI output available (see docs/RUN.md: UseWddmDriver)")]
    NoGpuOutput,

    #[error("not supported on this platform/build: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Other(#[from] anyhow_like::BoxError),
}

/// Tiny shim so we can `?` arbitrary boxed errors without pulling `anyhow` into core.
pub mod anyhow_like {
    pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
}

impl Error {
    pub fn capture(msg: impl Into<String>) -> Self {
        Error::Capture(msg.into())
    }
    pub fn convert(msg: impl Into<String>) -> Self {
        Error::Convert(msg.into())
    }
    pub fn encode(msg: impl Into<String>) -> Self {
        Error::Encode(msg.into())
    }
    pub fn audio(msg: impl Into<String>) -> Self {
        Error::Audio(msg.into())
    }
    pub fn transport(msg: impl Into<String>) -> Self {
        Error::Transport(msg.into())
    }
    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }
}
