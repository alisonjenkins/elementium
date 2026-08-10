//! VP8 video encoding and decoding via raw libvpx FFI.

use elementium_types::{I420Frame, PlaintextMedia};

use crate::codec_error::{Codec, CodecError, CodecErrorKind, VpxStatus};

/// How much encoding quality to trade for speed, 0 (best quality) to 16 (fastest).
///
/// libvpx defaults VP8 to 0, and the `vpx-encode` wrapper this code used to go through
/// set it only for VP9 -- so every frame was encoded at maximum effort. At 1280x720 that
/// costs far more CPU per frame than a video call can afford, and the encoder becomes the
/// slowest stage in the pipeline. Real-time WebRTC stacks run VP8 in this range; the
/// picture difference at conversational bitrates is not visible, and the CPU difference is
/// several-fold.
const CPU_USED: i32 = 8;

/// Frames the encoder may buffer before emitting one.
///
/// Must be zero for a call. Any lag is added directly to the latency of every frame, and
/// libvpx's default assumes offline encoding where that does not matter.
const LAG_IN_FRAMES: u32 = 0;

/// The rate controller's buffer, in milliseconds of budget.
///
/// One second, half of it available at the start, six tenths as the level it aims to hold.
/// These are the values libwebrtc uses for real-time VP8 and the reasoning is the same: a
/// deep buffer smooths quality by spending future budget now, which in a call means latency
/// -- the one thing that cannot be recovered later.
const RC_BUFFER_MS: u32 = 1_000;
const RC_BUFFER_INITIAL_MS: u32 = 500;
const RC_BUFFER_OPTIMAL_MS: u32 = 600;

/// How far a frame may overshoot or undershoot its share of the budget, as a percentage.
///
/// libvpx defaults to allowing a large overshoot, which suits a file and not a call: the
/// spike lands on a congestion controller that can only respond by dropping it.
const RC_OVERSHOOT_PCT: u32 = 15;
const RC_UNDERSHOOT_PCT: u32 = 100;

/// Longest run of frames without a keyframe, as a backstop.
///
/// A receiver that joins mid-call cannot decode anything until it sees one. Keyframe
/// requests are the real mechanism (see [`Vp8Encoder::force_keyframe`]); this bounds the
/// wait for a receiver that never asks. libvpx's own default is 9999 -- over five minutes
/// at 30fps.
const KEYFRAME_MAX_DIST: u32 = 150;

/// VP8 encoder over libvpx, configured for real-time use.
/// One frame's duration in 90kHz ticks, for a stated frame rate.
///
/// 3000 at 30fps. Clamped to a sane rate rather than trusting the caller: a zero would divide
/// by zero, and a nonsense rate would give libvpx a clock that runs at the wrong speed, which
/// is precisely the fault this exists to fix.
fn frame_duration_ticks(max_framerate: u32) -> i64 {
    const TICKS_PER_SECOND: i64 = 90_000;
    let fps = i64::from(max_framerate.clamp(1, 240));
    TICKS_PER_SECOND.checked_div(fps).unwrap_or(3_000).max(1)
}

pub struct Vp8Encoder {
    ctx: vpx_sys::vpx_codec_ctx_t,
    /// Kept so the bitrate can be retargeted without rebuilding the encoder: congestion
    /// control changes the target during a call, and libvpx needs the whole config back.
    cfg: vpx_sys::vpx_codec_enc_cfg_t,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    pts: i64,
    /// How long one frame lasts, in the 90kHz ticks `g_timebase` counts.
    ///
    /// Handed to `vpx_codec_encode` as the frame's duration and added to `pts` afterwards, so
    /// libvpx's clock advances at the rate frames are actually produced. It was `1` for both:
    /// one tick at 90kHz is 11 microseconds, so the encoder believed it was encoding 90,000
    /// frames a second and its rate control never saw a second pass. Measured against a
    /// moving 720p picture asked for 1700 kbps, it produced 9045 -- five times the budget,
    /// with the congestion controller downstream left to throw the excess away.
    frame_duration_ticks: i64,
    /// Set by [`Vp8Encoder::force_keyframe`], consumed by the next [`Vp8Encoder::encode`].
    force_keyframe: bool,
}

// SAFETY: the encoder context is only ever touched through `&mut self` methods, so it is
// never accessed concurrently. Matches `Vp8Decoder`'s reasoning.
unsafe impl Send for Vp8Encoder {}

/// A single encoded VP8 packet.
#[derive(Debug, Clone)]
pub struct Vp8Packet {
    /// Encoded VP8 bytes in the clear -- must be encrypted before reaching the wire.
    /// See [`PlaintextMedia`] for why this is typed rather than a bare `Vec<u8>`.
    pub data: PlaintextMedia,
    pub is_keyframe: bool,
    pub pts: i64,
}

impl Vp8Encoder {
    /// Create a new VP8 encoder for the given resolution and bitrate (kbps).
    ///
    /// # Errors
    ///
    /// Returns [`CodecErrorKind::OddDimensions`] if the dimensions are not even (libvpx
    /// requires it for 4:2:0 chroma), [`CodecErrorKind::DefaultConfig`] if libvpx will not
    /// produce a default configuration, or [`CodecErrorKind::EncoderInit`] if it refuses the
    /// configuration built from it.
    #[allow(clippy::as_conversions, clippy::cast_possible_wrap)]
    pub fn new(
        width: u32,
        height: u32,
        bitrate_kbps: u32,
        max_framerate: u32,
    ) -> Result<Self, CodecError> {
        use std::mem::MaybeUninit;
        use vpx_sys::{
            VPX_CODEC_OK, VPX_ENCODER_ABI_VERSION, vp8e_enc_control_id::VP8E_SET_CPUUSED,
            vpx_codec_enc_config_default, vpx_codec_enc_init_ver, vpx_codec_vp8_cx,
            vpx_kf_mode::VPX_KF_AUTO, vpx_rc_mode::VPX_CBR,
        };

        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(CodecError::new(
                Codec::Vp8,
                CodecErrorKind::OddDimensions { width, height },
            ));
        }

        // SAFETY: `cfg` and `ctx` are plain-old-data FFI structs that libvpx fully
        // initializes on success. The interface pointer is a static returned by libvpx.
        unsafe {
            let iface = vpx_codec_vp8_cx();
            let mut cfg = MaybeUninit::zeroed().assume_init();
            let ret = vpx_codec_enc_config_default(iface, &raw mut cfg, 0);
            if ret != VPX_CODEC_OK {
                return Err(CodecError::new(
                    Codec::Vp8,
                    CodecErrorKind::DefaultConfig(VpxStatus(ret)),
                ));
            }

            cfg.g_w = width;
            cfg.g_h = height;
            // WebRTC's 90kHz clock, so `pts` is in the same units as the RTP timestamps.
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = 90_000;
            cfg.rc_target_bitrate = bitrate_kbps;
            // Constant bitrate: a call has a fixed budget every second, not on average
            // over a file.
            cfg.rc_end_usage = VPX_CBR;
            cfg.g_lag_in_frames = LAG_IN_FRAMES;
            // The buffer model, without which "constant bitrate" is a suggestion. libvpx's
            // defaults are sized for offline encoding and let a hard scene run far over
            // budget before rate control pulls it back: measured on a moving 720p picture
            // asked for 1700 kbps, 2335 with the clock fixed but these left at their
            // defaults. A conferencing encoder wants a shallow buffer -- one second of
            // budget, half of it to start -- because latency is the thing being protected
            // and a deep buffer converts an overshoot into delay.
            cfg.rc_buf_sz = RC_BUFFER_MS;
            cfg.rc_buf_initial_sz = RC_BUFFER_INITIAL_MS;
            cfg.rc_buf_optimal_sz = RC_BUFFER_OPTIMAL_MS;
            // How far over the per-frame budget a frame may go. The default lets a keyframe
            // or a hard scene spend several frames' worth at once, which is exactly the
            // spike a congestion controller downstream then has to absorb.
            cfg.rc_overshoot_pct = RC_OVERSHOOT_PCT;
            cfg.rc_undershoot_pct = RC_UNDERSHOOT_PCT;
            cfg.kf_mode = VPX_KF_AUTO;
            cfg.kf_max_dist = KEYFRAME_MAX_DIST;
            // Let libvpx recover from packet loss without needing a fresh keyframe for
            // every damaged frame.
            cfg.g_error_resilient = vpx_sys::VPX_ERROR_RESILIENT_DEFAULT;
            cfg.g_threads = std::thread::available_parallelism()
                .map_or(4, |n| u32::try_from(n.get()).unwrap_or(4).clamp(1, 8));

            let mut ctx = MaybeUninit::zeroed().assume_init();
            let ret = vpx_codec_enc_init_ver(
                &raw mut ctx,
                iface,
                &raw const cfg,
                0,
                VPX_ENCODER_ABI_VERSION as i32,
            );
            if ret != VPX_CODEC_OK {
                // The geometry is named as a *cause* when it is the obvious one, because
                // the bare failure sends people to the encoder when the fault is upstream.
                // A zero size means nothing ever recorded a frame size -- capture or
                // conversion failed earlier -- and an odd one means the capture handed over
                // geometry VP8 cannot take. Both were real faults; both cost time reading
                // "encoder_init" as an encoder problem.
                let likely_cause = if width == 0 || height == 0 {
                    "geometry was never set upstream; capture or conversion produced no sized frame"
                } else if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
                    "VP8 requires even dimensions and this geometry is odd"
                } else {
                    "not a geometry problem; see the vpx error code"
                };
                tracing::error!(
                    width,
                    height,
                    bitrate_kbps,
                    error_kind = "encoder_init",
                    vpx_error_code = ?ret,
                    likely_cause,
                    "Failed to initialize VP8 encoder"
                );
                return Err(CodecError::new(
                    Codec::Vp8,
                    CodecErrorKind::EncoderInit(VpxStatus(ret)),
                ));
            }

            // The speed/quality control the previous wrapper never applied to VP8.
            let ret = vpx_sys::vpx_codec_control_(&raw mut ctx, VP8E_SET_CPUUSED as i32, CPU_USED);
            if ret != VPX_CODEC_OK {
                // Not fatal: the encoder works, just slowly. Worth knowing about, because
                // "the encoder is slow" is otherwise indistinguishable from a slow machine.
                tracing::warn!(vpx_error_code = ?ret, "could not set VP8 cpu_used; encoding will be slower");
            }

            tracing::info!(
                width,
                height,
                bitrate_kbps,
                cpu_used = CPU_USED,
                threads = cfg.g_threads,
                "VP8 encoder initialized"
            );

            Ok(Self {
                ctx,
                cfg,
                width,
                height,
                bitrate_kbps,
                pts: 0,
                frame_duration_ticks: frame_duration_ticks(max_framerate),
                force_keyframe: false,
            })
        }
    }

    /// The geometry this encoder was configured for.
    ///
    /// Frames of any other size are rejected, so a caller whose source can renegotiate
    /// must check this rather than assume the size it asked for is the size it gets.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The bitrate this encoder was configured for, in kbps.
    #[must_use]
    pub const fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// Make the next encoded frame a keyframe.
    ///
    /// A receiver cannot decode anything until it has one: interframes reference frames it
    /// never saw. This is how a receiver's RTCP keyframe request is honoured, and it is
    /// the difference between recovering in one frame and displaying a broken picture
    /// until the next scheduled keyframe.
    ///
    /// Infallible: it sets a flag consumed by the next `encode`, rather than rebuilding
    /// the encoder as an earlier version had to.
    pub const fn force_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Retarget the encoder's bitrate without rebuilding it.
    ///
    /// A call's available bandwidth is not fixed: congestion control lowers the target
    /// when the link degrades and raises it when it recovers. Rebuilding the encoder to
    /// change it would emit a keyframe every time, which is the opposite of what a
    /// congested link needs.
    ///
    /// # Errors
    ///
    /// Returns [`CodecErrorKind::SetBitrate`] if libvpx rejects the new configuration.
    pub fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<(), CodecError> {
        if kbps == self.bitrate_kbps {
            return Ok(());
        }
        self.cfg.rc_target_bitrate = kbps;
        // SAFETY: `ctx` was initialised by `new` and `cfg` is the configuration it was
        // created from, with one field changed.
        let ret = unsafe {
            vpx_sys::vpx_codec_enc_config_set(std::ptr::addr_of_mut!(self.ctx), &raw const self.cfg)
        };
        if ret != vpx_sys::VPX_CODEC_OK {
            return Err(CodecError::new(
                Codec::Vp8,
                CodecErrorKind::SetBitrate {
                    kbps,
                    source: VpxStatus(ret),
                },
            ));
        }
        tracing::info!(
            from_kbps = self.bitrate_kbps,
            to_kbps = kbps,
            "VP8 bitrate retargeted"
        );
        self.bitrate_kbps = kbps;
        Ok(())
    }

    /// Encode an I420 frame. Returns zero or more VP8 packets.
    ///
    /// # Errors
    ///
    /// Returns [`CodecErrorKind::SizeMismatch`] if `frame`'s dimensions don't match the
    /// encoder's configured dimensions, [`CodecErrorKind::PlanesTooSmall`] if the planes are
    /// too small for those dimensions, [`CodecErrorKind::ImageWrapFailed`] if libvpx will not
    /// wrap the frame, or [`CodecErrorKind::Encode`] if libvpx fails to encode it.
    #[allow(clippy::as_conversions, clippy::cast_possible_wrap)]
    pub fn encode(&mut self, frame: &I420Frame) -> Result<Vec<Vp8Packet>, CodecError> {
        use vpx_sys::{
            VPX_CODEC_OK, VPX_DL_REALTIME, VPX_EFLAG_FORCE_KF, vpx_codec_encode, vpx_img_fmt,
            vpx_img_wrap,
        };

        if frame.width() != self.width || frame.height() != self.height {
            tracing::error!(
                encoder_width = self.width,
                encoder_height = self.height,
                frame_width = frame.width(),
                frame_height = frame.height(),
                error_kind = "frame_size_mismatch",
                "VP8 encode: frame size does not match encoder configuration"
            );
            return Err(CodecError::new(
                Codec::Vp8,
                CodecErrorKind::SizeMismatch {
                    expected: (self.width, self.height),
                    actual: (frame.width(), frame.height()),
                },
            ));
        }

        // libvpx reads the planes through raw pointers, so short planes are a buffer
        // overrun, not a bad picture. Checked here rather than trusted, using the frame's
        // own strides -- a padded plane is longer than width*height, and comparing against
        // the tightly-packed size would reject valid frames.
        let h = frame.height() as usize;
        let luma = frame.y_stride().saturating_mul(h);
        let chroma = frame.uv_stride().saturating_mul(h.div_ceil(2));
        if frame.y().len() < luma || frame.u().len() < chroma || frame.v().len() < chroma {
            return Err(CodecError::new(
                Codec::Vp8,
                CodecErrorKind::PlanesTooSmall {
                    width: frame.width(),
                    height: frame.height(),
                },
            ));
        }

        // The planes are pointed at directly, padding and all: libvpx takes a stride per
        // plane, so a decoder's padded output needs no repacking. `vpx-encode` required
        // one contiguous tightly-packed buffer, which cost a 1.4MB copy per frame at 720p
        // for nothing.
        let mut image = std::mem::MaybeUninit::<vpx_sys::vpx_image_t>::zeroed();

        let flags = if self.force_keyframe {
            self.force_keyframe = false;
            VPX_EFLAG_FORCE_KF
        } else {
            0
        };

        let mut packets = Vec::new();

        // SAFETY: `self.ctx` was initialized by `new`. The plane slices outlive this call
        // and are at least as large as the geometry requires (checked above).
        // `vpx_codec_get_cx_data` returns null or a pointer into libvpx-owned state that
        // stays valid until the next encode call.
        unsafe {
            let img = vpx_img_wrap(
                image.as_mut_ptr(),
                vpx_img_fmt::VPX_IMG_FMT_I420,
                frame.width(),
                frame.height(),
                1,
                frame.y().as_ptr().cast_mut(),
            );
            if img.is_null() {
                return Err(CodecError::new(Codec::Vp8, CodecErrorKind::ImageWrapFailed));
            }

            let img = &mut *img;
            img.planes[0] = frame.y().as_ptr().cast_mut();
            img.planes[1] = frame.u().as_ptr().cast_mut();
            img.planes[2] = frame.v().as_ptr().cast_mut();
            img.stride[0] = i32::try_from(frame.y_stride()).unwrap_or(i32::MAX);
            img.stride[1] = i32::try_from(frame.uv_stride()).unwrap_or(i32::MAX);
            img.stride[2] = i32::try_from(frame.uv_stride()).unwrap_or(i32::MAX);

            let ret = vpx_codec_encode(
                std::ptr::addr_of_mut!(self.ctx),
                img,
                self.pts,
                u64::try_from(self.frame_duration_ticks).unwrap_or(3_000),
                i64::from(flags),
                VPX_DL_REALTIME.into(),
            );
            if ret != VPX_CODEC_OK {
                tracing::error!(
                    width = self.width,
                    height = self.height,
                    pts = self.pts,
                    error_kind = "encode",
                    vpx_error_code = ?ret,
                    "VP8 encode failed"
                );
                return Err(CodecError::new(Codec::Vp8, CodecErrorKind::Encode(VpxStatus(ret))));
            }

            // SAFETY: `self.ctx` was just encoded into above, and stays valid for the
            // duration of this call.
            packets.extend(Self::collect_encoded_packets(&mut self.ctx));
        }

        // One frame's worth of 90kHz ticks, matching the duration handed to the encoder
        // above. The RTP timestamps are still derived separately from real elapsed time --
        // this clock exists for libvpx's rate control, which needs to believe time passes at
        // the rate frames arrive or it cannot hold a bitrate.
        self.pts = self.pts.saturating_add(self.frame_duration_ticks);
        Ok(packets)
    }

    /// Drain every packet libvpx produced for the frame just encoded.
    ///
    /// Split out of [`Vp8Encoder::encode`] so that function stays under the workspace's
    /// line limit; the loop itself is unchanged.
    ///
    /// # Safety
    ///
    /// `ctx` must have just been used in a successful `vpx_codec_encode` call, and must
    /// still be valid.
    unsafe fn collect_encoded_packets(ctx: &mut vpx_sys::vpx_codec_ctx_t) -> Vec<Vp8Packet> {
        use vpx_sys::{
            VPX_FRAME_IS_KEY, vpx_codec_cx_pkt_kind, vpx_codec_get_cx_data, vpx_codec_iter_t,
        };

        let mut packets = Vec::new();
        let mut iter: vpx_codec_iter_t = std::ptr::null();
        // SAFETY: as documented on this function.
        unsafe {
            loop {
                let pkt = vpx_codec_get_cx_data(std::ptr::addr_of_mut!(*ctx), &raw mut iter);
                if pkt.is_null() {
                    break;
                }
                let pkt = &*pkt;
                if pkt.kind != vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let frame_pkt = pkt.data.frame;
                let bytes = std::slice::from_raw_parts(frame_pkt.buf.cast::<u8>(), frame_pkt.sz);
                packets.push(Vp8Packet {
                    data: PlaintextMedia::from_encoder(bytes.to_vec()),
                    is_keyframe: (frame_pkt.flags & VPX_FRAME_IS_KEY) != 0,
                    pts: frame_pkt.pts,
                });
            }
        }
        packets
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for Vp8Encoder {
    fn drop(&mut self) {
        // SAFETY: `ctx` was initialized in `new` and is destroyed exactly once.
        unsafe {
            vpx_sys::vpx_codec_destroy(std::ptr::addr_of_mut!(self.ctx));
        }
    }
}

/// VP8 decoder using raw libvpx FFI via `vpx-encode`'s bundled `vpx_sys`.
///
/// Since `vpx-encode` only provides encoding, we use the underlying FFI
/// symbols that it links against. The decoder functions are in the same
/// libvpx shared library.
pub struct Vp8Decoder {
    ctx: vpx_sys::vpx_codec_ctx_t,
}

// SAFETY: The vpx decoder context is safe to send between threads as long
// as it's not accessed concurrently. We ensure this via &mut self methods.
unsafe impl Send for Vp8Decoder {}

impl Vp8Decoder {
    /// Create a new VP8 decoder.
    ///
    /// # Errors
    ///
    /// Returns [`CodecErrorKind::DecoderInit`] if the underlying libvpx decoder fails to
    /// initialize.
    pub fn new() -> Result<Self, CodecError> {
        use std::mem::MaybeUninit;
        use vpx_sys::{
            VPX_CODEC_OK, VPX_DECODER_ABI_VERSION, vpx_codec_dec_cfg_t, vpx_codec_dec_init_ver,
            vpx_codec_vp8_dx,
        };

        // SAFETY: `ctx` and `cfg` are plain-old-data FFI structs; libvpx
        // fully initializes `ctx` on success, and a zeroed `cfg` requests
        // default decoder settings, which is the documented usage pattern.
        unsafe {
            let mut ctx = MaybeUninit::uninit();
            let cfg = MaybeUninit::<vpx_codec_dec_cfg_t>::zeroed();

            // Narrowing i32 cast: VPX_DECODER_ABI_VERSION is a small compile-time
            // constant from the vpx_sys bindings, well within i32 range.
            #[allow(clippy::cast_possible_wrap, clippy::as_conversions)]
            let abi_version = VPX_DECODER_ABI_VERSION as i32;

            let ret = vpx_codec_dec_init_ver(
                ctx.as_mut_ptr(),
                vpx_codec_vp8_dx(),
                cfg.as_ptr(),
                0,
                abi_version,
            );

            if ret != VPX_CODEC_OK {
                tracing::error!(
                    error_kind = "decoder_init",
                    vpx_error_code = ?ret,
                    "Failed to initialize VP8 decoder"
                );
                return Err(CodecError::new(
                    Codec::Vp8,
                    CodecErrorKind::DecoderInit(VpxStatus(ret)),
                ));
            }

            Ok(Self {
                ctx: ctx.assume_init(),
            })
        }
    }

    /// Decode a VP8 packet and return the decoded I420 frame(s).
    ///
    /// # Errors
    ///
    /// Returns [`CodecErrorKind::PacketTooLarge`] if `data` cannot fit libvpx's `u32` length
    /// field, [`CodecErrorKind::Decode`] if libvpx rejects the packet, or
    /// [`CodecErrorKind::MalformedVpxImage`] / [`CodecErrorKind::ImplausibleGeometry`] if a
    /// decoded image's own structure cannot be read -- see the variants' docs.
    pub fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, CodecError> {
        use std::ptr;
        use vpx_sys::{VPX_CODEC_OK, vpx_codec_decode, vpx_codec_get_frame, vpx_codec_iter_t};

        let data = data.as_bytes();

        // Narrowing usize -> u32 cast for the FFI call. VP8/WebRTC packets
        // are bounded well under u32::MAX in practice; guard explicitly
        // rather than silently truncating an oversized buffer.
        let data_len = u32::try_from(data.len()).map_err(|_| {
            tracing::error!(
                buffer_len = data.len(),
                error_kind = "packet_too_large",
                "VP8 decode: packet exceeds u32 length"
            );
            CodecError::new(
                Codec::Vp8,
                CodecErrorKind::PacketTooLarge { len: data.len() },
            )
        })?;

        // SAFETY: `self.ctx` was initialized by `Vp8Decoder::new`. `data`
        // is a valid slice for `data.len()` bytes for the duration of this
        // call. `vpx_codec_get_frame` returns either null or a pointer into
        // libvpx-owned decoder state valid until the next decode call.
        unsafe {
            let ret = vpx_codec_decode(
                std::ptr::addr_of_mut!(self.ctx),
                data.as_ptr(),
                data_len,
                ptr::null_mut(),
                0,
            );

            if ret != VPX_CODEC_OK {
                tracing::error!(
                    buffer_len = data.len(),
                    error_kind = "decode",
                    vpx_error_code = ?ret,
                    "VP8 decode failed"
                );
                return Err(CodecError::new(Codec::Vp8, CodecErrorKind::Decode(VpxStatus(ret))));
            }

            let mut frames = Vec::new();
            let mut iter: vpx_codec_iter_t = ptr::null();

            loop {
                let img = vpx_codec_get_frame(
                    std::ptr::addr_of_mut!(self.ctx),
                    std::ptr::addr_of_mut!(iter),
                );
                let Some(im) = img.as_ref() else {
                    break;
                };
                // SAFETY: `im` is the image `vpx_codec_get_frame` just returned, live for
                // the duration of this call, as documented on `image_to_frame`; already
                // inside this function's own `unsafe` block.
                if let Some(frame) = Self::image_to_frame(im)? {
                    frames.push(frame);
                } else {
                    tracing::warn!(
                        width = im.d_w,
                        height = im.d_h,
                        "decoded frame's planes do not match its geometry; dropped"
                    );
                }
            }

            Ok(frames)
        }
    }

    /// Convert one decoded libvpx image into an [`I420Frame`], copying row by row to honour
    /// its own strides.
    ///
    /// Split out of [`Vp8Decoder::decode`] so that function stays under the workspace's line
    /// limit; the logic is unchanged. `None` means the image's planes do not fit its own
    /// stated geometry -- not an error worth failing the whole packet over, since the caller
    /// logs and drops it.
    ///
    /// # Safety
    ///
    /// `im`'s `planes` and `stride` must describe a live decoded image: each plane must have
    /// at least `stride * height` (or `stride * uv_height` for chroma) valid bytes, as
    /// libvpx guarantees for the image `vpx_codec_get_frame` returns.
    unsafe fn image_to_frame(im: &vpx_sys::vpx_image_t) -> Result<Option<I420Frame>, CodecError> {
        let malformed = |field: &'static str| {
            CodecError::new(Codec::Vp8, CodecErrorKind::MalformedVpxImage { field })
        };
        let implausible = |field: &'static str, source: std::num::TryFromIntError| {
            CodecError::new(Codec::Vp8, CodecErrorKind::ImplausibleGeometry { field, source })
        };

        let (img_w, img_h) = (im.d_w, im.d_h);
        let w_usize = usize::try_from(img_w).map_err(|e| implausible("width", e))?;
        let h_usize = usize::try_from(img_h).map_err(|e| implausible("height", e))?;
        let uv_w = w_usize / 2;
        let uv_h = h_usize / 2;

        let Some(&y_stride) = im.stride.first() else {
            return Err(malformed("Y stride"));
        };
        let Some(&u_stride) = im.stride.get(1) else {
            return Err(malformed("U stride"));
        };
        let Some(&v_stride) = im.stride.get(2) else {
            return Err(malformed("V stride"));
        };
        let y_stride = usize::try_from(y_stride).map_err(|e| implausible("Y stride", e))?;
        let u_stride = usize::try_from(u_stride).map_err(|e| implausible("U stride", e))?;
        let v_stride = usize::try_from(v_stride).map_err(|e| implausible("V stride", e))?;

        let Some(&y_plane_ptr) = im.planes.first() else {
            return Err(malformed("Y plane"));
        };
        let Some(&u_plane_ptr) = im.planes.get(1) else {
            return Err(malformed("U plane"));
        };
        let Some(&v_plane_ptr) = im.planes.get(2) else {
            return Err(malformed("V plane"));
        };

        let mut y = vec![0u8; w_usize.saturating_mul(h_usize)];
        let mut u = vec![0u8; uv_w.saturating_mul(uv_h)];
        let mut v = vec![0u8; uv_w.saturating_mul(uv_h)];

        // SAFETY: as documented on this function.
        unsafe {
            for (row, y_row) in y.chunks_mut(w_usize).enumerate() {
                let offset = row.saturating_mul(y_stride);
                let src = std::slice::from_raw_parts(y_plane_ptr.add(offset), w_usize);
                y_row.copy_from_slice(src);
            }
            for ((row, u_row), v_row) in u.chunks_mut(uv_w).enumerate().zip(v.chunks_mut(uv_w)) {
                let u_offset = row.saturating_mul(u_stride);
                let v_offset = row.saturating_mul(v_stride);
                let src_u = std::slice::from_raw_parts(u_plane_ptr.add(u_offset), uv_w);
                let src_v = std::slice::from_raw_parts(v_plane_ptr.add(v_offset), uv_w);
                u_row.copy_from_slice(src_u);
                v_row.copy_from_slice(src_v);
            }
        }

        Ok(I420Frame::from_planes(img_w, img_h, &y, &u, &v, 0))
    }
}

impl Drop for Vp8Decoder {
    fn drop(&mut self) {
        // SAFETY: `self.ctx` was initialized by `Vp8Decoder::new` and is
        // only destroyed once, here.
        unsafe {
            vpx_sys::vpx_codec_destroy(std::ptr::addr_of_mut!(self.ctx));
        }
    }
}

impl crate::video::VideoEncoder for Vp8Encoder {
    fn codec(&self) -> crate::video::VideoCodec {
        crate::video::VideoCodec::Vp8
    }

    fn size(&self) -> (u32, u32) {
        Self::size(self)
    }

    fn encode(&mut self, frame: &I420Frame) -> Result<Vec<crate::video::EncodedFrame>, CodecError> {
        Ok(Self::encode(self, frame)?
            .into_iter()
            .map(|p| crate::video::EncodedFrame {
                data: p.data,
                is_keyframe: p.is_keyframe,
                pts: p.pts,
            })
            .collect())
    }

    fn request_keyframe(&mut self) {
        Self::force_keyframe(self);
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        self.set_bitrate_kbps(kbps)
    }
}

impl crate::video::VideoDecoder for Vp8Decoder {
    fn codec(&self) -> crate::video::VideoCodec {
        crate::video::VideoCodec::Vp8
    }

    fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, CodecError> {
        Self::decode(self, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Test-only: panicking on failure via unwrap/expect is the idiomatic
    // way to fail a `#[test]`; this function intentionally does not
    // propagate errors via `Result`.
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn vp8_encode_decode_roundtrip() {
        let width = 320u32;
        let height = 240u32;

        // Create a solid-color I420 frame (green-ish)
        let w_usize = usize::try_from(width).expect("bad width");
        let h_usize = usize::try_from(height).expect("bad height");
        let uv_w = w_usize / 2;
        let uv_h = h_usize / 2;

        let y_plane = vec![150u8; w_usize.saturating_mul(h_usize)];
        let u_plane = vec![128u8; uv_w.saturating_mul(uv_h)];
        let v_plane = vec![128u8; uv_w.saturating_mul(uv_h)];

        let frame = I420Frame::from_planes(width, height, &y_plane, &u_plane, &v_plane, 0)
            .expect("planes match the geometry");

        let mut encoder = Vp8Encoder::new(width, height, 500, 30).expect("encoder creation");
        let mut decoder = Vp8Decoder::new().expect("decoder creation");

        // Encode the frame
        let packets = encoder.encode(&frame).expect("encode");
        assert!(!packets.is_empty(), "Should produce at least one packet");
        let first_packet = packets
            .first()
            .expect("packets should be non-empty (checked above)");
        assert!(first_packet.is_keyframe, "First frame should be keyframe");

        // Decode the packet
        let decoded_frames = decoder.decode(&first_packet.data).expect("decode");
        assert_eq!(decoded_frames.len(), 1, "Should decode exactly one frame");

        let out_frame = decoded_frames
            .first()
            .expect("decoded_frames should have exactly one frame (checked above)");
        assert_eq!(out_frame.width(), width);
        assert_eq!(out_frame.height(), height);
        assert_eq!(out_frame.y().len(), w_usize.saturating_mul(h_usize));
        assert_eq!(out_frame.u().len(), uv_w.saturating_mul(uv_h));

        // Check Y values are approximately correct (lossy codec)
        let sum: f64 = out_frame.y().iter().map(|&v| f64::from(v)).sum();
        // usize -> f64 for a diagnostic average only; precision loss on a
        // pixel count (far below 2^52) is immaterial here.
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let len_f64 = out_frame.y().len() as f64;
        let avg_y: f64 = sum / len_f64;
        assert!(
            (avg_y - 150.0).abs() < 10.0,
            "Average Y should be ~150, got {avg_y}"
        );
    }

    /// libvpx requires even dimensions for 4:2:0 chroma; an odd one must be refused before
    /// any libvpx call is made, not handed to the encoder to fail confusingly.
    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn odd_dimensions_are_refused_before_touching_libvpx() {
        // `expect_err` needs `Vp8Encoder: Debug`, which it is not; a `let else` reaches the
        // error without that bound.
        let Err(err) = Vp8Encoder::new(321, 240, 500, 30) else {
            panic!("321 is odd");
        };
        assert_eq!(err.codec, crate::codec_error::Codec::Vp8);
        assert!(
            matches!(
                err.kind,
                crate::codec_error::CodecErrorKind::OddDimensions {
                    width: 321,
                    height: 240,
                }
            ),
            "expected OddDimensions{{321,240}}, got {:?}",
            err.kind
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn encode_rejects_mismatched_frame_dimensions() {
        let mut encoder = Vp8Encoder::new(320, 240, 500, 30).expect("encoder creation");
        let frame = I420Frame::from_planes(
            640,
            480,
            &vec![0u8; 640 * 480],
            &vec![0u8; 320 * 240],
            &vec![0u8; 320 * 240],
            0,
        )
        .expect("planes match the geometry");
        let err = encoder.encode(&frame).expect_err("size mismatch must be refused");
        assert_eq!(err.codec, crate::codec_error::Codec::Vp8);
        assert!(
            matches!(
                err.kind,
                crate::codec_error::CodecErrorKind::SizeMismatch {
                    expected: (320, 240),
                    actual: (640, 480),
                }
            ),
            "expected SizeMismatch{{expected:(320,240), actual:(640,480)}}, got {:?}",
            err.kind
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn decode_rejects_garbage_packet() {
        let mut decoder = Vp8Decoder::new().expect("decoder creation");
        // Not a valid VP8 bitstream -- libvpx must reject it, not panic or hang.
        let garbage = PlaintextMedia::from_encoder(vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let err = decoder.decode(&garbage).expect_err("garbage must be refused");
        assert_eq!(err.codec, crate::codec_error::Codec::Vp8);
        assert!(
            matches!(err.kind, crate::codec_error::CodecErrorKind::Decode(_)),
            "expected Decode(_), got {:?}",
            err.kind
        );
    }

    /// A receiver that joins late can only start decoding at a keyframe, so the encoder
    /// must be able to produce one on demand. Without this, libvpx's default maximum
    /// keyframe distance (9999 frames) leaves a late subscriber staring at nothing for
    /// minutes while every interframe references pictures it never received.
    #[test]
    #[allow(clippy::expect_used)]
    fn force_keyframe_makes_the_next_frame_decodable_on_its_own() {
        let (width, height) = (320_u32, 240_u32);
        let mut encoder = Vp8Encoder::new(width, height, 500, 30).expect("encoder creation");

        // Drive several frames so the encoder has settled past its initial keyframe.
        let mut produced_keyframe = false;
        for i in 0..10_u8 {
            let frame = solid_frame(width, height, 100_u8.saturating_add(i));
            let packets = encoder.encode(&frame).expect("encode");
            if i > 0 {
                produced_keyframe |= packets.iter().any(|p| p.is_keyframe);
            }
        }
        assert!(
            !produced_keyframe,
            "the encoder is not expected to emit keyframes on its own here -- if it does, \
             this test no longer proves that force_keyframe is what caused the next one"
        );

        encoder.force_keyframe();
        let packets = encoder
            .encode(&solid_frame(width, height, 200))
            .expect("encode after forcing a keyframe");

        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "the frame after a forced keyframe must be a keyframe"
        );

        // And it must genuinely stand alone: decodable by a decoder that has seen nothing.
        let mut fresh_decoder = Vp8Decoder::new().expect("decoder creation");
        let keyframe = packets
            .iter()
            .find(|p| p.is_keyframe)
            .expect("checked above");
        let decoded = fresh_decoder.decode(&keyframe.data).expect("decode");
        assert_eq!(decoded.len(), 1, "a keyframe decodes with no prior state");
    }

    /// Forcing a keyframe must not change the encoder's configured geometry.
    #[test]
    #[allow(clippy::expect_used)]
    fn force_keyframe_preserves_the_configured_size() {
        let mut encoder = Vp8Encoder::new(640, 480, 500, 30).expect("encoder creation");
        encoder.force_keyframe();
        assert_eq!(encoder.size(), (640, 480));
    }

    #[allow(clippy::expect_used)]
    fn solid_frame(width: u32, height: u32, luma: u8) -> I420Frame {
        let w = usize::try_from(width).expect("bad width");
        let h = usize::try_from(height).expect("bad height");
        let uv = (w / 2).saturating_mul(h / 2);
        I420Frame::from_planes(
            width,
            height,
            &vec![luma; w.saturating_mul(h)],
            &vec![128u8; uv],
            &vec![128u8; uv],
            0,
        )
        .expect("planes match the geometry")
    }
}
