//! A frame as it arrives, which is not always pixels.
//!
//! Capture used to hand over `I420Frame` and nothing else, which meant every MJPEG frame was
//! decoded on the CPU before anything could look at it. That decode is the largest single
//! cost in the application — profiling put it at 37.6% of the capture path — and on a
//! machine whose GPU can decode JPEG it is entirely avoidable, but only if the compressed
//! bytes survive long enough to reach the encoder.
//!
//! So a frame is either decoded pixels or the JPEG it came as, and the consumer decides.
//! An encoder that can take the compressed bytes takes them; anything that needs pixels
//! asks, and pays for the decode at that point.
//!
//! # The preview still costs something
//!
//! The self-view needs pixels whatever the encoder does, so on the accelerated path a
//! frame that reaches the encoder untouched is still decoded for the preview. Two things
//! make that acceptable: the preview is rate-limited well below capture, so at 60fps most
//! frames are never decoded at all, and it is decoded at half size, which libjpeg-turbo
//! does for roughly a quarter of the work rather than by decoding and then shrinking.

use elementium_types::I420Frame;

/// A frame from a capture source.
#[derive(Debug, Clone)]
pub enum CapturedFrame {
    /// Decoded pixels, ready for anything.
    Planar(I420Frame),
    /// One complete baseline JPEG, undecoded.
    ///
    /// Produced only when the negotiated encoder can decode it in hardware. There is no
    /// point carrying compressed bytes to a consumer that will only have to decode them on
    /// the CPU anyway, and doing so would move the cost rather than remove it.
    Mjpeg {
        data: Vec<u8>,
        width: u32,
        height: u32,
    },
}

impl CapturedFrame {
    /// The frame's width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        match self {
            Self::Planar(frame) => frame.width(),
            Self::Mjpeg { width, .. } => *width,
        }
    }

    /// The frame's height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        match self {
            Self::Planar(frame) => frame.height(),
            Self::Mjpeg { height, .. } => *height,
        }
    }

    /// The compressed bytes, if this frame still has them.
    #[must_use]
    pub const fn mjpeg(&self) -> Option<&[u8]> {
        match self {
            Self::Planar(_) => None,
            Self::Mjpeg { data, .. } => Some(data.as_slice()),
        }
    }

    /// The frame as pixels, decoding it if necessary.
    ///
    /// Returns `None` only if the JPEG cannot be decoded, which for a frame that arrived
    /// from a camera means it was truncated or corrupt.
    #[must_use]
    pub fn to_planar(&self) -> Option<I420Frame> {
        match self {
            Self::Planar(frame) => Some(frame.clone()),
            Self::Mjpeg { data, .. } => crate::pipewire_capture::decode_mjpeg_to_i420(data),
        }
    }

    /// The frame as pixels at half size, for the self-view.
    ///
    /// Half size because that is what the preview displays, and arriving there directly is
    /// far cheaper than arriving at full size and shrinking. libjpeg-turbo decodes at 1/2
    /// scale for roughly a quarter of the work, since it can skip most of each block's
    /// coefficients rather than reconstructing them and throwing them away.
    #[must_use]
    pub fn to_preview(&self) -> Option<I420Frame> {
        match self {
            Self::Planar(frame) => {
                Some(elementium_codec::halve_i420(frame).unwrap_or_else(|| frame.clone()))
            }
            Self::Mjpeg { data, .. } => crate::pipewire_capture::decode_mjpeg_half_scale(data),
        }
    }
}

impl From<I420Frame> for CapturedFrame {
    fn from(frame: I420Frame) -> Self {
        Self::Planar(frame)
    }
}
