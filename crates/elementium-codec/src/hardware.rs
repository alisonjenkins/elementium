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
    /// Whether an encoder for this backend actually exists in this build.
    ///
    /// Distinct from the hardware being *present*. The probe reports what the GPU can do,
    /// which is a fact about the machine; this reports what we can do with it, which is a
    /// fact about the code. Conflating them offers a codec in SDP, wins the negotiation,
    /// and then fails to produce a single frame — the far end sees a track that never
    /// starts, with nothing to explain it.
    ///
    /// VAAPI's encoder exists on Linux builds with the feature enabled; the other two are
    /// still to come, and each flips its arm as its encoder lands.
    #[must_use]
    pub const fn has_encoder(self) -> bool {
        match self {
            Self::Software => true,
            Self::Vaapi => cfg!(all(target_os = "linux", feature = "vaapi")),
            Self::VideoToolbox | Self::MediaFoundation => false,
        }
    }

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

/// Everything the graphics hardware offers the capture and encode path.
///
/// Encoding is the headline, but not the whole of it. Whether the GPU can decode JPEG
/// decides which format is worth asking the camera for — MJPEG is the most expensive
/// option when the CPU must decode it and the cheapest when the GPU can — and whether it
/// has a post-processor decides whether a layout the encoder does not read can be accepted
/// at all. All three come from the same driver query, so they are reported together rather
/// than opening a VA display three times.
#[derive(Debug, Default, Clone)]
pub struct HardwareCapabilities {
    /// Codecs the hardware can encode, with their size limits.
    pub encoders: Vec<EncoderCapability>,
    /// Whether the GPU has a JPEG decode block reachable through the driver.
    pub jpeg_decode: bool,
    /// Whether the GPU can convert between pixel layouts and scale.
    pub video_proc: bool,
}

/// What this machine's graphics hardware can do.
///
/// Probed once per process and cached. The answer cannot change while the application runs
/// -- a GPU is not hot-plugged mid-call -- and asking is not free: initialising a VA
/// display and enumerating its profiles takes milliseconds and logs as it goes, which is
/// unacceptable on a path consulted whenever a codec is chosen.
#[must_use]
pub fn capabilities() -> &'static HardwareCapabilities {
    static CACHE: std::sync::OnceLock<HardwareCapabilities> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let mut caps = platform::probe();
        // Software support is unconditional, and listed first so it is never absent.
        let software = VideoCodec::software_supported().iter().map(|&codec| {
            EncoderCapability {
                codec,
                backend: EncoderBackend::Software,
                // No meaningful limit: a software encoder is bounded by memory and
                // patience.
                max_width: u32::MAX,
                max_height: u32::MAX,
            }
        });
        let mut encoders: Vec<EncoderCapability> = software.collect();
        encoders.append(&mut caps.encoders);
        caps.encoders = encoders;
        caps
    })
}

/// Everything this machine can encode, hardware and software.
#[must_use]
pub fn available_encoders() -> &'static [EncoderCapability] {
    &capabilities().encoders
}

/// Pick the backend to encode `codec` with, preferring hardware.
///
/// Returns [`EncoderBackend::Software`] when no hardware backend offers the codec at that
/// size, which is the common case for VP8 — no GPU encodes it.
#[must_use]
pub fn best_backend(codec: VideoCodec, width: u32, height: u32) -> EncoderBackend {
    selectable_backend_from(available_encoders(), codec, width, height)
}

/// Which backend the *policy* would choose, ignoring whether it is implemented.
///
/// Kept separate from [`selectable_backend_from`] because they answer different questions
/// and both matter. This one is about the machine: does a hardware encoder exist that
/// handles this codec at this size? That answer stays true as encoders are implemented,
/// so the policy can be tested without the tests changing every time one lands.
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

/// Which backend can actually be used, which is what every caller wants.
///
/// The policy's choice, narrowed to backends this build has an encoder for. Hardware being
/// present on the machine and being usable from here are different facts, and offering a
/// codec on the strength of the first wins the SDP negotiation and then produces no
/// frames.
#[must_use]
pub fn selectable_backend_from(
    caps: &[EncoderCapability],
    codec: VideoCodec,
    width: u32,
    height: u32,
) -> EncoderBackend {
    let chosen = best_backend_from(caps, codec, width, height);
    if chosen.has_encoder() {
        chosen
    } else {
        EncoderBackend::Software
    }
}

/// The codecs worth offering, best first, limited to what the server permits.
///
/// `server_allows` is the SFU's `enabled_publish_codecs` from its join response. Offering
/// a codec it has not enabled wastes a negotiation round at best; at worst the SFU accepts
/// the m-line and forwards a stream no subscriber asked for. An empty list is treated as
/// "the server did not say", and everything is offered.
///
/// Ordering is a negotiation decision rather than a taste. Two things drive it:
///
/// **Hardware first.** A codec this machine encodes in hardware saves a core's worth of
/// CPU for the whole call, which on a laptop is battery and on a desktop is whatever else
/// the machine is doing.
///
/// **Then compression.** AV1 carries the same quality in roughly two thirds of H.264's
/// bitrate, so it is preferred where both are available.
///
/// The two can conflict, and hardware wins deliberately. AV1 encoded in software costs
/// far more CPU than it saves in bandwidth for a real-time call, and the SFU does not
/// transcode -- so every subscriber must decode whatever is sent, and AV1 hardware
/// *decode* is much rarer than AV1 hardware encode. Preferring software AV1 would spend
/// our CPU to encode it and every peer's battery to decode it.
///
/// VP8 stays in the list regardless, and last: everything speaks it, and it is what
/// [`BackupCodecPolicy`](https://docs.rs/livekit-protocol) regression falls back to.
#[must_use]
pub fn negotiation_order(width: u32, height: u32, server_allows: &[VideoCodec]) -> Vec<VideoCodec> {
    let caps = available_encoders();
    let mut codecs: Vec<VideoCodec> = VideoCodec::all()
        .iter()
        .copied()
        .filter(|codec| server_allows.is_empty() || server_allows.contains(codec))
        // A codec we cannot actually produce must never be offered: the far end would
        // agree to it and then receive nothing. Hardware being *present* is not enough --
        // the encoder for it has to exist in this build too.
        .filter(|&codec| {
            VideoCodec::software_supported().contains(&codec)
                || selectable_backend_from(caps, codec, width, height).is_hardware()
        })
        .collect();

    codecs.sort_by_key(|&codec| {
        let hardware = selectable_backend_from(caps, codec, width, height).is_hardware();
        (!hardware, codec.negotiation_rank())
    });
    codecs
}

#[cfg(target_os = "linux")]
mod platform {
    use super::HardwareCapabilities;

    /// What VAAPI reports.
    ///
    /// Behind a feature because libva is a system library: a build without it should fail
    /// to find hardware rather than fail to compile, and a machine without a GPU driver is
    /// a normal machine.
    #[cfg(feature = "vaapi")]
    pub fn probe() -> HardwareCapabilities {
        crate::vaapi_probe::probe()
    }

    /// No VAAPI support compiled in.
    #[cfg(not(feature = "vaapi"))]
    pub fn probe() -> HardwareCapabilities {
        HardwareCapabilities::default()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::HardwareCapabilities;

    /// What `VideoToolbox` reports. Not yet implemented; see the Linux probe.
    pub fn probe() -> HardwareCapabilities {
        HardwareCapabilities::default()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::HardwareCapabilities;

    /// What Media Foundation reports. Not yet implemented; see the Linux probe.
    pub fn probe() -> HardwareCapabilities {
        HardwareCapabilities::default()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::HardwareCapabilities;

    /// No hardware video acceleration is known for this platform.
    pub fn probe() -> HardwareCapabilities {
        HardwareCapabilities::default()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        EncoderBackend, EncoderCapability, available_encoders, best_backend_from,
        negotiation_order, selectable_backend_from,
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

    /// Hardware that is present but has no encoder in this build must not be selected.
    ///
    /// The probe reports what the GPU can do, which is a fact about the machine. Whether
    /// we can use it is a fact about the code, and conflating the two offers a codec in
    /// SDP, wins the negotiation, and then produces no frames at all.
    #[test]
    fn present_hardware_without_an_encoder_is_not_selected() {
        let caps = [
            software(VideoCodec::Vp8),
            hardware(VideoCodec::H264, 4096, 4096),
        ];
        assert_eq!(
            best_backend_from(&caps, VideoCodec::H264, 1920, 1080),
            EncoderBackend::Vaapi,
            "the policy should still say hardware is the right choice"
        );
        assert_eq!(
            selectable_backend_from(&caps, VideoCodec::H264, 1920, 1080),
            if EncoderBackend::Vaapi.has_encoder() {
                EncoderBackend::Vaapi
            } else {
                EncoderBackend::Software
            },
            "but it can only be selected once an encoder for it exists"
        );
    }

    /// The policy prefers hardware that can do the job.
    #[test]
    fn hardware_wins_when_it_supports_the_codec_and_size() {
        let caps = [
            software(VideoCodec::Vp8),
            hardware(VideoCodec::H264, 4096, 4096),
        ];
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
        let caps = [
            software(VideoCodec::H264),
            hardware(VideoCodec::H264, 1920, 1080),
        ];
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

    /// A codec with no encoder must never be offered. The far end would agree to it and
    /// then receive nothing, which is worse than never having offered it.
    ///
    /// "Producible" means software support *or* a hardware encoder this build can actually
    /// reach -- not merely hardware the machine happens to have, which is the distinction
    /// `selectable_backend_from` exists to draw.
    #[test]
    fn only_codecs_we_can_actually_produce_are_offered() {
        let caps = available_encoders();
        let order = negotiation_order(1280, 720, &[]);
        assert!(
            order.contains(&VideoCodec::Vp8),
            "VP8 must always be offered: it is the one every peer speaks"
        );
        for codec in &order {
            assert!(
                VideoCodec::software_supported().contains(codec)
                    || selectable_backend_from(caps, *codec, 1280, 720).is_hardware(),
                "{codec:?} was offered with no encoder available for it"
            );
        }
    }

    /// The server's `enabled_publish_codecs` is a constraint, not advice.
    #[test]
    fn the_servers_list_limits_what_is_offered() {
        let order = negotiation_order(1280, 720, &[VideoCodec::H264]);
        assert!(
            !order.contains(&VideoCodec::Vp8),
            "VP8 must not be offered when the server has not enabled it"
        );
    }

    /// An empty server list means it did not say, not that nothing is allowed.
    #[test]
    fn an_empty_server_list_permits_everything() {
        assert!(!negotiation_order(1280, 720, &[]).is_empty());
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
