pub mod capture_format;
pub mod codec_error;
pub mod h264_codec;
pub mod hardware;
pub mod opus_codec;
pub mod pixel_convert;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub mod vaapi;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub mod vaapi_probe;
pub mod video;
pub mod vpx_codec;

pub use capture_format::{CaptureFormat, ConversionCost, EncodeTarget};
pub use codec_error::{Codec, CodecError, CodecErrorKind};
pub use hardware::{EncoderBackend, EncoderCapability, available_encoders, best_backend};
pub use opus_codec::{OpusDecoder, OpusEncoder, OpusEncoderConfig};
pub use pixel_convert::{
    bgra_to_i420, halve_i420, halve_rgba, i420_to_rgba, rgb_to_i420, rgba_to_i420, scale_i420,
    scaled_dimensions,
};
pub use video::{
    EncodedFrame, EncoderConfig, NegotiatedDecoder, NegotiatedEncoder, PixelLayout, VideoCodec,
    VideoDecoder, VideoEncoder,
};
pub use vpx_codec::{Vp8Decoder, Vp8Encoder, Vp8Packet};
