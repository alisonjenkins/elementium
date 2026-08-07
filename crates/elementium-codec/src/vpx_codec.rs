//! VP8 video encoding and decoding via the `vpx-encode` crate and raw libvpx FFI.

use elementium_types::{I420Frame, PlaintextMedia};

/// VP8 encoder wrapping `vpx_encode::Encoder`.
pub struct Vp8Encoder {
    encoder: vpx_encode::Encoder,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    pts: i64,
}

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
    /// Returns an error string if the underlying `vpx_encode` encoder fails
    /// to initialize.
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let config = vpx_encode::Config {
            width,
            height,
            timebase: [1, 90_000], // WebRTC uses 90kHz clock
            bitrate: bitrate_kbps,
            codec: vpx_encode::VideoCodecId::VP8,
        };

        let encoder = vpx_encode::Encoder::new(config).map_err(|e| {
            tracing::error!(
                width,
                height,
                bitrate_kbps,
                error_kind = "encoder_init",
                error = %e,
                "Failed to initialize VP8 encoder"
            );
            format!("VP8 encoder init: {e}")
        })?;

        Ok(Self {
            encoder,
            width,
            height,
            bitrate_kbps,
            pts: 0,
        })
    }

    /// The geometry this encoder was configured for.
    ///
    /// Frames of any other size are rejected, so a caller whose source can renegotiate
    /// must check this rather than assume the size it asked for is the size it gets.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Make the next encoded frame a keyframe.
    ///
    /// A receiver cannot decode anything until it has one: interframes reference frames it
    /// never saw. libvpx's default maximum keyframe distance is 9999 frames -- over two
    /// minutes at 60fps -- so a peer that subscribes after our first keyframe, or that
    /// loses one, sees nothing at all until the encoder happens to emit another. Real
    /// stacks solve this by responding to a receiver's keyframe request; this is the
    /// mechanism that makes such a response possible.
    ///
    /// Implemented by rebuilding the encoder, because `vpx_encode::Encoder` exposes no way
    /// to set `VPX_EFLAG_FORCE_KF` on a single frame. The cost is the encoder's
    /// rate-control state, which it rebuilds within a few frames; the alternative is
    /// hand-rolling the FFI encoder, which is a great deal more code to get wrong for a
    /// path that runs seconds apart.
    ///
    /// # Errors
    ///
    /// Returns an error string if the replacement encoder fails to initialize. The old
    /// encoder is kept in that case, so a failure degrades to "no keyframe yet" rather
    /// than to no video at all.
    pub fn force_keyframe(&mut self) -> Result<(), String> {
        let replacement = Self::new(self.width, self.height, self.bitrate_kbps)?;
        self.encoder = replacement.encoder;
        Ok(())
    }

    /// Encode an I420 frame. Returns zero or more VP8 packets.
    ///
    /// # Errors
    ///
    /// Returns an error string if `frame`'s dimensions don't match the
    /// encoder's configured dimensions, or if the underlying `vpx_encode`
    /// encoder fails.
    pub fn encode(&mut self, frame: &I420Frame) -> Result<Vec<Vp8Packet>, String> {
        if frame.width != self.width || frame.height != self.height {
            tracing::error!(
                encoder_width = self.width,
                encoder_height = self.height,
                frame_width = frame.width,
                frame_height = frame.height,
                error_kind = "frame_size_mismatch",
                "VP8 encode: frame size does not match encoder configuration"
            );
            return Err(format!(
                "Frame size mismatch: encoder={}x{}, frame={}x{}",
                self.width, self.height, frame.width, frame.height
            ));
        }

        // vpx-encode expects a contiguous I420 buffer: Y + U + V
        let mut i420_buf =
            Vec::with_capacity(frame.y.len().saturating_add(frame.u.len()).saturating_add(frame.v.len()));
        i420_buf.extend_from_slice(&frame.y);
        i420_buf.extend_from_slice(&frame.u);
        i420_buf.extend_from_slice(&frame.v);

        let packets = self.encoder.encode(self.pts, &i420_buf).map_err(|e| {
            tracing::error!(
                width = self.width,
                height = self.height,
                buffer_len = i420_buf.len(),
                pts = self.pts,
                error_kind = "encode",
                error = %e,
                "VP8 encode failed"
            );
            format!("VP8 encode: {e}")
        })?;

        let result = packets
            .into_iter()
            .map(|p| Vp8Packet {
                data: PlaintextMedia::from_encoder(p.data.to_vec()),
                is_keyframe: p.key,
                pts: p.pts,
            })
            .collect();

        self.pts = self.pts.saturating_add(1);
        Ok(result)
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
    /// Returns an error string if the underlying libvpx decoder fails to
    /// initialize.
    pub fn new() -> Result<Self, String> {
        use std::mem::MaybeUninit;
        use vpx_sys::{
            vpx_codec_dec_cfg_t, vpx_codec_dec_init_ver, vpx_codec_vp8_dx, VPX_CODEC_OK,
            VPX_DECODER_ABI_VERSION,
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
                return Err(format!("VP8 decoder init failed: error code {ret:?}"));
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
    /// Returns an error string if the underlying libvpx decoder rejects the
    /// packet.
    pub fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, String> {
        use std::ptr;
        use vpx_sys::{vpx_codec_decode, vpx_codec_get_frame, vpx_codec_iter_t, VPX_CODEC_OK};

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
            "VP8 decode: packet too large".to_string()
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
                return Err(format!("VP8 decode failed: error code {ret:?}"));
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
                let img_w = im.d_w;
                let img_h = im.d_h;
                let w_usize = usize::try_from(img_w).map_err(|_| "VP8 decode: invalid width".to_string())?;
                let h_usize = usize::try_from(img_h).map_err(|_| "VP8 decode: invalid height".to_string())?;
                let uv_w = w_usize / 2;
                let uv_h = h_usize / 2;

                let Some(&y_stride) = im.stride.first() else {
                    return Err("VP8 decode: missing Y stride".to_string());
                };
                let Some(&u_stride) = im.stride.get(1) else {
                    return Err("VP8 decode: missing U stride".to_string());
                };
                let Some(&v_stride) = im.stride.get(2) else {
                    return Err("VP8 decode: missing V stride".to_string());
                };
                let y_stride = usize::try_from(y_stride)
                    .map_err(|_| "VP8 decode: invalid Y stride".to_string())?;
                let u_stride = usize::try_from(u_stride)
                    .map_err(|_| "VP8 decode: invalid U stride".to_string())?;
                let v_stride = usize::try_from(v_stride)
                    .map_err(|_| "VP8 decode: invalid V stride".to_string())?;

                let Some(&y_plane_ptr) = im.planes.first() else {
                    return Err("VP8 decode: missing Y plane".to_string());
                };
                let Some(&u_plane_ptr) = im.planes.get(1) else {
                    return Err("VP8 decode: missing U plane".to_string());
                };
                let Some(&v_plane_ptr) = im.planes.get(2) else {
                    return Err("VP8 decode: missing V plane".to_string());
                };

                let mut y = vec![0u8; w_usize.saturating_mul(h_usize)];
                let mut u = vec![0u8; uv_w.saturating_mul(uv_h)];
                let mut v = vec![0u8; uv_w.saturating_mul(uv_h)];

                // Copy row by row (stride may differ from width).
                // SAFETY: libvpx guarantees each plane has at least
                // `stride * height` (or `stride * uv_height` for chroma)
                // valid bytes for a decoded image of dimensions w x h.
                for (row, y_row) in y.chunks_mut(w_usize).enumerate() {
                    let offset = row.saturating_mul(y_stride);
                    let src = std::slice::from_raw_parts(y_plane_ptr.add(offset), w_usize);
                    y_row.copy_from_slice(src);
                }
                for ((row, u_row), v_row) in u
                    .chunks_mut(uv_w)
                    .enumerate()
                    .zip(v.chunks_mut(uv_w))
                {
                    let u_offset = row.saturating_mul(u_stride);
                    let v_offset = row.saturating_mul(v_stride);
                    let src_u = std::slice::from_raw_parts(u_plane_ptr.add(u_offset), uv_w);
                    let src_v = std::slice::from_raw_parts(v_plane_ptr.add(v_offset), uv_w);
                    u_row.copy_from_slice(src_u);
                    v_row.copy_from_slice(src_v);
                }

                frames.push(I420Frame {
                    width: img_w,
                    height: img_h,
                    y,
                    u,
                    v,
                    timestamp_us: 0,
                });
            }

            Ok(frames)
        }
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

        let frame = I420Frame {
            width,
            height,
            y: y_plane,
            u: u_plane,
            v: v_plane,
            timestamp_us: 0,
        };

        let mut encoder = Vp8Encoder::new(width, height, 500).expect("encoder creation");
        let mut decoder = Vp8Decoder::new().expect("decoder creation");

        // Encode the frame
        let packets = encoder.encode(&frame).expect("encode");
        assert!(!packets.is_empty(), "Should produce at least one packet");
        let first_packet = packets.first().expect("packets should be non-empty (checked above)");
        assert!(first_packet.is_keyframe, "First frame should be keyframe");

        // Decode the packet
        let decoded_frames = decoder.decode(&first_packet.data).expect("decode");
        assert_eq!(decoded_frames.len(), 1, "Should decode exactly one frame");

        let out_frame = decoded_frames
            .first()
            .expect("decoded_frames should have exactly one frame (checked above)");
        assert_eq!(out_frame.width, width);
        assert_eq!(out_frame.height, height);
        assert_eq!(out_frame.y.len(), w_usize.saturating_mul(h_usize));
        assert_eq!(out_frame.u.len(), uv_w.saturating_mul(uv_h));

        // Check Y values are approximately correct (lossy codec)
        let sum: f64 = out_frame.y.iter().map(|&v| f64::from(v)).sum();
        // usize -> f64 for a diagnostic average only; precision loss on a
        // pixel count (far below 2^52) is immaterial here.
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let len_f64 = out_frame.y.len() as f64;
        let avg_y: f64 = sum / len_f64;
        assert!(
            (avg_y - 150.0).abs() < 10.0,
            "Average Y should be ~150, got {avg_y}"
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn encode_rejects_mismatched_frame_dimensions() {
        let mut encoder = Vp8Encoder::new(320, 240, 500).expect("encoder creation");
        let frame = I420Frame {
            width: 640,
            height: 480,
            y: vec![0u8; 640 * 480],
            u: vec![0u8; 320 * 240],
            v: vec![0u8; 320 * 240],
            timestamp_us: 0,
        };
        let result = encoder.encode(&frame);
        assert!(
            result.is_err_and(|e| e.contains("size mismatch")),
            "expected a frame size mismatch error"
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn decode_rejects_garbage_packet() {
        let mut decoder = Vp8Decoder::new().expect("decoder creation");
        // Not a valid VP8 bitstream -- libvpx must reject it, not panic or hang.
        let garbage = PlaintextMedia::from_encoder(vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert!(decoder.decode(&garbage).is_err());
    }

    /// A receiver that joins late can only start decoding at a keyframe, so the encoder
    /// must be able to produce one on demand. Without this, libvpx's default maximum
    /// keyframe distance (9999 frames) leaves a late subscriber staring at nothing for
    /// minutes while every interframe references pictures it never received.
    #[test]
    #[allow(clippy::expect_used)]
    fn force_keyframe_makes_the_next_frame_decodable_on_its_own() {
        let (width, height) = (320_u32, 240_u32);
        let mut encoder = Vp8Encoder::new(width, height, 500).expect("encoder creation");

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

        encoder.force_keyframe().expect("rebuild the encoder");
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
        let mut encoder = Vp8Encoder::new(640, 480, 500).expect("encoder creation");
        encoder.force_keyframe().expect("rebuild the encoder");
        assert_eq!(encoder.size(), (640, 480));
    }

    #[allow(clippy::expect_used)]
    fn solid_frame(width: u32, height: u32, luma: u8) -> I420Frame {
        let w = usize::try_from(width).expect("bad width");
        let h = usize::try_from(height).expect("bad height");
        let uv = (w / 2).saturating_mul(h / 2);
        I420Frame {
            width,
            height,
            y: vec![luma; w.saturating_mul(h)],
            u: vec![128u8; uv],
            v: vec![128u8; uv],
            timestamp_us: 0,
        }
    }
}
