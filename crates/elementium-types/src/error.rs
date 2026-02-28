use thiserror::Error;

#[derive(Error, Debug)]
pub enum ElementiumError {
    #[error("WebRTC error: {0}")]
    WebRtc(String),

    #[error("Codec error: {0}")]
    Codec(String),

    #[error("Media device error: {0}")]
    MediaDevice(String),

    #[error("Screen capture error: {0}")]
    ScreenCapture(String),

    #[error("Signaling error: {0}")]
    Signaling(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ElementiumError>;
