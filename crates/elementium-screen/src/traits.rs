use elementium_types::{CaptureSource, I420Frame};

/// Platform-agnostic screen capture trait.
pub trait ScreenCapturer: Send {
    /// List available capture sources (monitors and windows).
    ///
    /// # Errors
    ///
    /// Returns an error if the platform capture backend fails to enumerate sources.
    fn sources(&self) -> Result<Vec<CaptureSource>, elementium_types::ElementiumError>;

    /// Start capturing from the given source. Returns frames via the callback.
    ///
    /// # Errors
    ///
    /// Returns an error if the capture session fails to start.
    fn start(
        &mut self,
        source_id: &str,
        callback: Box<dyn Fn(I420Frame) + Send>,
    ) -> Result<(), elementium_types::ElementiumError>;

    /// Stop the current capture session.
    ///
    /// # Errors
    ///
    /// Returns an error if the capture session fails to stop cleanly.
    fn stop(&mut self) -> Result<(), elementium_types::ElementiumError>;
}
