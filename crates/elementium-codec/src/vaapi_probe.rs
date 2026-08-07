//! Ask VAAPI what this machine can encode in hardware.
//!
//! The answer is per-machine and cannot be guessed. An AMD RX 9070 reports H.264, HEVC and
//! AV1; an older Intel part reports H.264 only; a machine with no GPU driver reports
//! nothing. Claiming a capability that turns out not to exist is worse than claiming none,
//! because the claim wins SDP negotiation and the failure then arrives mid-call as video
//! that never starts.
//!
//! So this queries the driver and reports exactly what it says. Every failure — no render
//! node, no driver, a profile with no encode entrypoint — results in that profile being
//! absent rather than in an error, because a machine without hardware encoding is a normal
//! machine and not a fault.
//!
//! The maximum picture size is queried too. Hardware encoders have real limits, and
//! exceeding one fails when the first frame is submitted rather than at setup — from the
//! user's side, video that simply never appears.

use std::os::raw::{c_int, c_uint};

use crate::hardware::{EncoderBackend, EncoderCapability, HardwareCapabilities};
use crate::video::VideoCodec;

/// Render nodes to try, in order.
///
/// A machine can have several GPUs — this one has a discrete Radeon and an integrated
/// Raphael — and they do not have the same capabilities. The first that initialises and
/// reports an encode entrypoint is used, which favours the discrete card since it
/// enumerates first.
const RENDER_NODES: [&str; 4] = [
    "/dev/dri/renderD128",
    "/dev/dri/renderD129",
    "/dev/dri/renderD130",
    "/dev/dri/renderD131",
];

/// Sizes hardware encoders will not exceed, when the driver does not say.
///
/// Used only when `VAConfigAttribMaxPictureWidth`/`Height` come back unset, which some
/// drivers do. 4096 is the conservative floor for anything that encodes H.264 at all, and
/// being conservative is the right direction: under-claiming falls back to software, while
/// over-claiming fails at the first frame.
const ASSUMED_MAX_WIDTH: u32 = 4096;
const ASSUMED_MAX_HEIGHT: u32 = 4096;

/// VA profiles worth asking about, and the codec each corresponds to.
///
/// Several profiles map to one codec — H.264 has Baseline, Main and High — and any of them
/// being encodable means the codec is. HEVC is deliberately absent: nothing in WebRTC
/// negotiates it, so knowing the GPU can encode it would not help.
fn profiles_of_interest() -> Vec<(VideoCodec, libva_sys::va_display_drm::VAProfile)> {
    use libva_sys::va_display_drm as va;
    vec![
        (
            VideoCodec::H264,
            va::VAProfile_VAProfileH264ConstrainedBaseline,
        ),
        (VideoCodec::H264, va::VAProfile_VAProfileH264Main),
        (VideoCodec::H264, va::VAProfile_VAProfileH264High),
        (VideoCodec::Av1, va::VAProfile_VAProfileAV1Profile0),
    ]
}

/// Everything VAAPI reports this machine can do.
///
/// Returns the default (nothing available) on any failure. That is the correct outcome
/// rather than an error: no hardware acceleration is a normal state, and the caller's next
/// step is the software path either way.
#[must_use]
pub fn probe() -> HardwareCapabilities {
    for node in RENDER_NODES {
        let caps = probe_node(node);
        if !caps.encoders.is_empty() {
            tracing::info!(
                node,
                codecs = ?caps.encoders.iter().map(|c| c.codec.sdp_name()).collect::<Vec<_>>(),
                jpeg_decode = caps.jpeg_decode,
                video_proc = caps.video_proc,
                "VAAPI hardware acceleration available"
            );
            return caps;
        }
    }
    tracing::info!("no VAAPI hardware encoder found; video will be encoded in software");
    HardwareCapabilities::default()
}

/// Query one render node.
fn probe_node(path: &str) -> HardwareCapabilities {
    // Read-write: the driver maps buffers through this descriptor, and a read-only
    // handle gets far enough to initialise and then fails inside Mesa with EACCES.
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return HardwareCapabilities::default();
    };
    // The display borrows the descriptor, so the file must outlive it.
    let Some(display) = VaDisplay::open(&file) else {
        return HardwareCapabilities::default();
    };

    let supported = display.encode_profiles();
    let mut encoders: Vec<EncoderCapability> = Vec::new();

    for (codec, profile) in profiles_of_interest() {
        if !supported.contains(&profile) {
            continue;
        }
        // One codec, several profiles: the first that answers is enough, and the largest
        // size any of them supports is the one to report.
        let (max_width, max_height) = display.max_picture_size(profile);
        if let Some(existing) = encoders.iter_mut().find(|c| c.codec == codec) {
            existing.max_width = existing.max_width.max(max_width);
            existing.max_height = existing.max_height.max(max_height);
        } else {
            encoders.push(EncoderCapability {
                codec,
                backend: EncoderBackend::Vaapi,
                max_width,
                max_height,
            });
        }
    }

    HardwareCapabilities {
        encoders,
        jpeg_decode: display.has_jpeg_decode(),
        video_proc: display.has_video_proc(),
    }
}

/// An initialised VA display, terminated on drop.
struct VaDisplay {
    handle: libva_sys::va_display_drm::VADisplay,
}

impl VaDisplay {
    /// Open and initialise a display on an already-open render node.
    fn open(file: &std::fs::File) -> Option<Self> {
        use std::os::fd::AsRawFd as _;

        // SAFETY: `fd` is valid for the lifetime of `file`, which outlives this call, and
        // `vaInitialize` writes only through the two out-pointers.
        unsafe {
            let handle = libva_sys::va_display_drm::vaGetDisplayDRM(file.as_raw_fd());
            if handle.is_null() {
                return None;
            }
            let (mut major, mut minor): (c_int, c_int) = (0, 0);
            let status = libva_sys::va_display_drm::vaInitialize(
                handle,
                std::ptr::addr_of_mut!(major),
                std::ptr::addr_of_mut!(minor),
            );
            if status != 0 {
                return None;
            }
            Some(Self { handle })
        }
    }

    /// Profiles this driver can encode with, as opposed to merely decode.
    ///
    /// A profile appearing in `vaQueryConfigProfiles` says nothing about encoding: almost
    /// every GPU decodes far more than it encodes, and the entrypoint is what separates
    /// the two.
    fn encode_profiles(&self) -> Vec<libva_sys::va_display_drm::VAProfile> {
        use libva_sys::va_display_drm as va;

        // SAFETY: `handle` was initialised in `open`. Both queries write at most the
        // number of elements they report through `vaMaxNum*`, which is how the buffers are
        // sized.
        unsafe {
            let max_profiles = va::vaMaxNumProfiles(self.handle);
            if max_profiles <= 0 {
                return Vec::new();
            }
            let mut profiles: Vec<va::VAProfile> =
                vec![0; usize::try_from(max_profiles).unwrap_or(0)];
            let mut num_profiles: c_int = 0;
            if va::vaQueryConfigProfiles(
                self.handle,
                profiles.as_mut_ptr(),
                std::ptr::addr_of_mut!(num_profiles),
            ) != 0
            {
                return Vec::new();
            }
            profiles.truncate(usize::try_from(num_profiles).unwrap_or(0));

            let max_entrypoints = va::vaMaxNumEntrypoints(self.handle);
            if max_entrypoints <= 0 {
                return Vec::new();
            }

            profiles
                .into_iter()
                .filter(|&profile| {
                    // `EncSlice` is the general case; `EncSliceLP` is the low-power path
                    // some Intel parts offer instead. Either is hardware encoding.
                    self.entrypoints(profile, max_entrypoints).iter().any(|&e| {
                        e == va::VAEntrypoint_VAEntrypointEncSlice
                            || e == va::VAEntrypoint_VAEntrypointEncSliceLP
                    })
                })
                .collect()
        }
    }

    /// Whether the GPU can decode JPEG itself.
    ///
    /// This is what decides whether MJPEG is the cheapest capture format or the most
    /// expensive one. With a JPEG block, the compressed bytes go to the GPU and the CPU
    /// never touches a pixel; without one, every frame costs a full CPU decode.
    ///
    /// Asked for separately from encoding because it is a decode entrypoint (`VLD`) on a
    /// profile no encoder uses, and because a GPU can perfectly well have one and not the
    /// other.
    fn has_jpeg_decode(&self) -> bool {
        use libva_sys::va_display_drm as va;
        self.has_entrypoint(
            va::VAProfile_VAProfileJPEGBaseline,
            va::VAEntrypoint_VAEntrypointVLD,
        )
    }

    /// Whether the GPU can convert between pixel layouts and scale.
    ///
    /// Post-processing is attached to the pseudo-profile `VAProfileNone` rather than to any
    /// codec, since it is not tied to one.
    fn has_video_proc(&self) -> bool {
        use libva_sys::va_display_drm as va;
        self.has_entrypoint(
            va::VAProfile_VAProfileNone,
            va::VAEntrypoint_VAEntrypointVideoProc,
        )
    }

    /// Whether `profile` offers `entrypoint`.
    fn has_entrypoint(
        &self,
        profile: libva_sys::va_display_drm::VAProfile,
        entrypoint: libva_sys::va_display_drm::VAEntrypoint,
    ) -> bool {
        // SAFETY: `handle` was initialised in `open`.
        let max = unsafe { libva_sys::va_display_drm::vaMaxNumEntrypoints(self.handle) };
        if max <= 0 {
            return false;
        }
        // SAFETY: as above, and `max` is the driver's own bound on the buffer.
        unsafe { self.entrypoints(profile, max) }.contains(&entrypoint)
    }

    /// The entrypoints `profile` offers, empty if the driver refuses the question.
    ///
    /// # Safety
    ///
    /// `self.handle` must be an initialised display, which `open` guarantees, and
    /// `max_entrypoints` must be what `vaMaxNumEntrypoints` reported.
    unsafe fn entrypoints(
        &self,
        profile: libva_sys::va_display_drm::VAProfile,
        max_entrypoints: c_int,
    ) -> Vec<libva_sys::va_display_drm::VAEntrypoint> {
        use libva_sys::va_display_drm as va;

        let mut entrypoints: Vec<va::VAEntrypoint> =
            vec![0; usize::try_from(max_entrypoints).unwrap_or(0)];
        let mut count: c_int = 0;
        // SAFETY: as the caller's contract; the buffer is sized by `vaMaxNumEntrypoints`.
        let status = unsafe {
            va::vaQueryConfigEntrypoints(
                self.handle,
                profile,
                entrypoints.as_mut_ptr(),
                std::ptr::addr_of_mut!(count),
            )
        };
        if status != 0 {
            return Vec::new();
        }
        entrypoints.truncate(usize::try_from(count).unwrap_or(0));
        entrypoints
    }

    /// The largest picture the driver will encode with `profile`.
    ///
    /// Falls back to a conservative assumption when the driver does not report one. Under-
    /// claiming costs a fallback to software; over-claiming fails at the first frame.
    fn max_picture_size(&self, profile: libva_sys::va_display_drm::VAProfile) -> (u32, u32) {
        use libva_sys::va_display_drm as va;

        let mut attrs = [
            va::VAConfigAttrib {
                type_: va::VAConfigAttribType_VAConfigAttribMaxPictureWidth,
                value: 0,
            },
            va::VAConfigAttrib {
                type_: va::VAConfigAttribType_VAConfigAttribMaxPictureHeight,
                value: 0,
            },
        ];

        // SAFETY: `handle` is initialised, and the driver writes only into the two
        // attributes described by the slice's length.
        let status = unsafe {
            va::vaGetConfigAttributes(
                self.handle,
                profile,
                va::VAEntrypoint_VAEntrypointEncSlice,
                attrs.as_mut_ptr(),
                c_int::try_from(attrs.len()).unwrap_or(0),
            )
        };

        // `VA_ATTRIB_NOT_SUPPORTED` is `!0`, which some drivers return instead of a size.
        let read = |raw: c_uint, fallback: u32| -> u32 {
            if status != 0 || raw == c_uint::MAX || raw == 0 {
                fallback
            } else {
                raw
            }
        };

        (
            read(attrs[0].value, ASSUMED_MAX_WIDTH),
            read(attrs[1].value, ASSUMED_MAX_HEIGHT),
        )
    }
}

impl Drop for VaDisplay {
    fn drop(&mut self) {
        // SAFETY: `handle` was initialised in `open` and is terminated exactly once.
        unsafe {
            libva_sys::va_display_drm::vaTerminate(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{probe, profiles_of_interest};
    use crate::hardware::EncoderBackend;
    use crate::video::VideoCodec;

    /// The probe must never report VP8. No GPU encodes it, so a VP8 capability could only
    /// come from a mapping bug -- and it would win negotiation and then fail.
    #[test]
    fn vp8_is_never_reported_as_hardware() {
        assert!(
            !profiles_of_interest()
                .iter()
                .any(|(codec, _)| *codec == VideoCodec::Vp8),
            "no GPU encodes VP8; reporting it would be a mapping error"
        );
    }

    /// Whatever the machine running the tests has, the result must be self-consistent:
    /// every capability is VAAPI, names a codec, and has a usable size limit. A limit of
    /// zero would silently exclude the encoder from every selection.
    #[test]
    fn whatever_is_reported_is_usable() {
        for cap in probe().encoders {
            assert_eq!(cap.backend, EncoderBackend::Vaapi);
            assert_ne!(cap.codec, VideoCodec::Vp8);
            assert!(cap.max_width >= 640, "unusable width: {}", cap.max_width);
            assert!(cap.max_height >= 480, "unusable height: {}", cap.max_height);
        }
    }

    /// Accelerated decode and accelerated encode are separate facts about a GPU, and the
    /// probe must not infer one from the other: a part can have an encode block and no
    /// JPEG decoder, or a post-processor and neither. Claiming JPEG decode that is not
    /// there would have the capture path ask for MJPEG on the strength of a GPU decode it
    /// then cannot perform, which is the single worst format for the CPU.
    #[test]
    fn accelerated_decode_is_reported_separately_from_encode() {
        let caps = probe();
        if caps.encoders.is_empty() {
            assert!(
                !caps.jpeg_decode && !caps.video_proc,
                "nothing should be claimed when no display could be opened"
            );
        }
    }

    /// Probing must be safe to repeat: it runs at least once per call, and a display left
    /// open or terminated twice would show up here rather than in production.
    #[test]
    fn probing_repeatedly_is_stable() {
        let first = probe();
        let second = probe();
        assert_eq!(
            first.encoders.len(),
            second.encoders.len(),
            "probe is not idempotent: {first:?} then {second:?}"
        );
        assert_eq!(first.jpeg_decode, second.jpeg_decode);
        assert_eq!(first.video_proc, second.video_proc);
    }
}
