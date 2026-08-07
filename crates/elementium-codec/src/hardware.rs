//! Choosing between software and hardware video encoders.
//!
//! Hardware encoding is not a faster version of the same thing. It changes what can be
//! negotiated, because no GPU encodes VP8 — the profiles an AMD RX 9070 offers are H.264,
//! HEVC and AV1, and Intel and Apple are much the same. A machine with hardware encoding
//! can only use it if the far end accepts one of those codecs, so the decision is
//! entangled with SDP negotiation rather than being a local optimisation.
//!
//! It is also not reliably available. A backend can be present and still refuse a
//! particular resolution, run out of surfaces, be held by another process, or be missing a
//! driver. Every path here therefore falls back to software rather than failing, and a
//! caller that wants to know what happened asks the encoder which backend it got.
//!
//! # Why this is one module rather than three
//!
//! The platforms differ in their APIs — VAAPI on Linux, `VideoToolbox` on macOS, Media
//! Foundation on Windows — but not in what the pipeline needs to ask: which codecs can be
//! encoded in hardware, and give me an encoder for one. Those two questions are the whole
//! interface, so the selection logic is written once and only the probe and the
//! construction are per-platform.

use crate::video::VideoCodec;

/// Where encoding happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncoderBackend {
    /// libvpx or similar, on the CPU. Always available.
    Software,
    /// VAAPI: Intel, AMD and some ARM on Linux.
    Vaapi,
    /// `VideoToolbox` on macOS and iOS.
    VideoToolbox,
    /// Media Foundation on Windows, covering Quick Sync, NVENC and AMF through one API.
    MediaFoundation,
}

impl EncoderBackend {
    /// Whether this backend uses dedicated hardware.
    #[must_use]
    pub const fn is_hardware(self) -> bool {
        !matches!(self, Self::Software)
    }

    /// A short name for logs and stats.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Vaapi => "vaapi",
            Self::VideoToolbox => "videotoolbox",
            Self::MediaFoundation => "media-foundation",
        }
    }
}

/// One thing a machine can do: encode `codec` on `backend`, up to a size.
///
/// The size limit is carried because hardware encoders have real ones and exceeding them
/// fails at encode time rather than at setup — which, without this, looks like video
/// stopping for no reason mid-call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderCapability {
    pub codec: VideoCodec,
    pub backend: EncoderBackend,
    pub max_width: u32,
    pub max_height: u32,
}

/// Everything this machine can encode, hardware and software.
///
/// Software support is unconditional; hardware support is whatever the platform reports.
#[must_use]
pub fn available_encoders() -> Vec<EncoderCapability> {
    let mut caps: Vec<EncoderCapability> = VideoCodec::software_supported()
        .iter()
        .map(|&codec| EncoderCapability {
            codec,
            backend: EncoderBackend::Software,
            // No meaningful limit: a software encoder is bounded by memory and patience.
            max_width: u32::MAX,
            max_height: u32::MAX,
        })
        .collect();
    caps.extend(platform::probe());
    caps
}

/// Pick the backend to encode `codec` with, preferring hardware.
///
/// Returns [`EncoderBackend::Software`] when no hardware backend offers the codec at that
/// size, which is the common case for VP8 — no GPU encodes it.
#[must_use]
pub fn best_backend(codec: VideoCodec, width: u32, height: u32) -> EncoderBackend {
    best_backend_from(&available_encoders(), codec, width, height)
}

/// [`best_backend`] against a given capability list, so the policy is testable without
/// depending on the machine the tests run on.
#[must_use]
pub fn best_backend_from(
    caps: &[EncoderCapability],
    codec: VideoCodec,
    width: u32,
    height: u32,
) -> EncoderBackend {
    caps.iter()
        .find(|c| {
            c.codec == codec
                && c.backend.is_hardware()
                && c.max_width >= width
                && c.max_height >= height
        })
        .map_or(EncoderBackend::Software, |c| c.backend)
}

/// The codecs worth offering in SDP, best first.
///
/// Ordering is a negotiation decision, not a preference: a codec that this machine can
/// encode in hardware saves a core's worth of CPU on every call that uses it, so it is
/// offered ahead of one that cannot, even if the software encoder is more mature.
///
/// VP8 stays in the list regardless, and last: everything speaks it, and it is the
/// fallback when a peer offers nothing better.
#[must_use]
pub fn negotiation_order(width: u32, height: u32) -> Vec<VideoCodec> {
    let caps = available_encoders();
    let mut codecs: Vec<VideoCodec> = VideoCodec::all().to_vec();
    codecs.sort_by_key(|&codec| {
        let hardware = best_backend_from(&caps, codec, width, height).is_hardware();
        // Hardware first, then the codec's own preference order.
        (!hardware, codec.negotiation_rank())
    });
    codecs
}

#[cfg(target_os = "linux")]
mod platform {
    use super::EncoderCapability;

    /// Hardware encoders VAAPI reports.
    ///
    /// Not yet implemented: enumerating VA profiles needs a libva binding and a rendering
    /// node, and returning a guess would be worse than returning nothing — a capability
    /// claimed and then not delivered fails at encode time, mid-call, with video simply
    /// stopping.
    ///
    /// Until then every call selects the software backend, which is the behaviour that
    /// existed before this module and is correct, just slower.
    #[allow(clippy::missing_const_for_fn)] // Will not be const once it queries the driver.
    pub fn probe() -> Vec<EncoderCapability> {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::EncoderCapability;

    /// Hardware encoders `VideoToolbox` reports. Not yet implemented; see the Linux probe.
    #[allow(clippy::missing_const_for_fn)] // Will not be const once it queries the driver.
    pub fn probe() -> Vec<EncoderCapability> {
        Vec::new()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::EncoderCapability;

    /// Hardware encoders Media Foundation reports. Not yet implemented; see the Linux
    /// probe.
    #[allow(clippy::missing_const_for_fn)] // Will not be const once it queries the driver.
    pub fn probe() -> Vec<EncoderCapability> {
        Vec::new()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::EncoderCapability;

    /// No hardware encoding is known for this platform.
    #[allow(clippy::missing_const_for_fn)] // Will not be const once it queries the driver.
    pub fn probe() -> Vec<EncoderCapability> {
        Vec::new()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        EncoderBackend, EncoderCapability, available_encoders, best_backend_from,
        negotiation_order,
    };
    use crate::video::VideoCodec;

    fn hardware(codec: VideoCodec, max_width: u32, max_height: u32) -> EncoderCapability {
        EncoderCapability {
            codec,
            backend: EncoderBackend::Vaapi,
            max_width,
            max_height,
        }
    }

    fn software(codec: VideoCodec) -> EncoderCapability {
        EncoderCapability {
            codec,
            backend: EncoderBackend::Software,
            max_width: u32::MAX,
            max_height: u32::MAX,
        }
    }

    /// Software encoding must always be an option. Every hardware path can fail at
    /// runtime -- a busy GPU, a missing driver, an unsupported size -- and a call that
    /// cannot fall back is a call that drops.
    #[test]
    fn software_is_always_available() {
        let caps = available_encoders();
        assert!(
            caps.iter().any(|c| c.backend == EncoderBackend::Software),
            "no software encoder reported"
        );
        assert!(
            caps.iter()
                .any(|c| c.codec == VideoCodec::Vp8 && c.backend == EncoderBackend::Software),
            "VP8 must always be encodable"
        );
    }

    /// Hardware is preferred when it can do the job.
    #[test]
    fn hardware_wins_when_it_supports_the_codec_and_size() {
        let caps = [software(VideoCodec::Vp8), hardware(VideoCodec::H264, 4096, 4096)];
        assert_eq!(
            best_backend_from(&caps, VideoCodec::H264, 1920, 1080),
            EncoderBackend::Vaapi
        );
    }

    /// A hardware encoder that cannot manage the resolution must not be chosen.
    ///
    /// Exceeding a hardware limit fails at encode time rather than at setup, so the
    /// failure appears mid-call as video stopping for no visible reason.
    #[test]
    fn hardware_is_rejected_above_its_size_limit() {
        let caps = [software(VideoCodec::H264), hardware(VideoCodec::H264, 1920, 1080)];
        assert_eq!(
            best_backend_from(&caps, VideoCodec::H264, 3840, 2160),
            EncoderBackend::Software,
            "4K on a 1080p encoder must fall back"
        );
        assert_eq!(
            best_backend_from(&caps, VideoCodec::H264, 1920, 1080),
            EncoderBackend::Vaapi,
            "exactly at the limit is still supported"
        );
    }

    /// The case that matters most in practice: no GPU encodes VP8, so a machine full of
    /// hardware encoders still encodes VP8 on the CPU.
    #[test]
    fn vp8_falls_back_to_software_even_on_a_capable_machine() {
        let caps = [
            software(VideoCodec::Vp8),
            hardware(VideoCodec::H264, 4096, 4096),
            hardware(VideoCodec::Av1, 4096, 4096),
        ];
        assert_eq!(
            best_backend_from(&caps, VideoCodec::Vp8, 1280, 720),
            EncoderBackend::Software
        );
    }

    /// Codecs this machine can encode in hardware are offered first, because the saving is
    /// a core's worth of CPU on every call that uses one.
    #[test]
    fn negotiation_offers_every_codec() {
        let order = negotiation_order(1280, 720);
        assert!(
            order.contains(&VideoCodec::Vp8),
            "VP8 must always be offered: it is the one every peer speaks"
        );
        assert_eq!(
            order.len(),
            VideoCodec::all().len(),
            "every codec must be offered, ordered rather than filtered"
        );
    }

    /// Backends must be distinguishable in logs and stats: "the call is slow" and "the
    /// call fell back to software encoding" are the same observation until one names it.
    #[test]
    fn backends_are_named_and_classified() {
        assert!(!EncoderBackend::Software.is_hardware());
        assert!(EncoderBackend::Vaapi.is_hardware());
        assert!(EncoderBackend::VideoToolbox.is_hardware());
        assert!(EncoderBackend::MediaFoundation.is_hardware());
        assert_eq!(EncoderBackend::Software.name(), "software");
    }
}
