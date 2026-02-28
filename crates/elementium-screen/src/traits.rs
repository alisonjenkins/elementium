use elementium_types::{CaptureSource, VideoFrame};

/// Platform-agnostic screen capture trait.
pub trait ScreenCapturer: Send {
    /// List available capture sources (monitors and windows).
    fn sources(&self) -> Result<Vec<CaptureSource>, elementium_types::ElementiumError>;

    /// Start capturing from the given source. Returns frames via the callback.
    fn start(
        &mut self,
        source_id: &str,
        callback: Box<dyn Fn(VideoFrame) + Send>,
    ) -> Result<(), elementium_types::ElementiumError>;

    /// Stop the current capture session.
    fn stop(&mut self) -> Result<(), elementium_types::ElementiumError>;
}
