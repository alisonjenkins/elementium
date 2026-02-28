//! VP8 video encoding and decoding via the `vpx-encode` crate and raw libvpx FFI.

use elementium_types::I420Frame;

/// VP8 encoder wrapping `vpx_encode::Encoder`.
pub struct Vp8Encoder {
    encoder: vpx_encode::Encoder,
    width: u32,
    height: u32,
    pts: i64,
}

/// A single encoded VP8 packet.
pub struct Vp8Packet {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub pts: i64,
}

impl Vp8Encoder {
    /// Create a new VP8 encoder for the given resolution and bitrate (kbps).
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let config = vpx_encode::Config {
            width,
            height,
            timebase: [1, 90_000], // WebRTC uses 90kHz clock
            bitrate: bitrate_kbps,
            codec: vpx_encode::VideoCodecId::VP8,
        };

        let encoder =
            vpx_encode::Encoder::new(config).map_err(|e| format!("VP8 encoder init: {e}"))?;

        Ok(Self {
            encoder,
            width,
            height,
            pts: 0,
        })
    }

    /// Encode an I420 frame. Returns zero or more VP8 packets.
    pub fn encode(&mut self, frame: &I420Frame) -> Result<Vec<Vp8Packet>, String> {
        if frame.width != self.width || frame.height != self.height {
            return Err(format!(
                "Frame size mismatch: encoder={}x{}, frame={}x{}",
                self.width, self.height, frame.width, frame.height
            ));
        }

        // vpx-encode expects a contiguous I420 buffer: Y + U + V
        let mut i420_buf =
            Vec::with_capacity(frame.y.len() + frame.u.len() + frame.v.len());
        i420_buf.extend_from_slice(&frame.y);
        i420_buf.extend_from_slice(&frame.u);
        i420_buf.extend_from_slice(&frame.v);

        let packets = self
            .encoder
            .encode(self.pts, &i420_buf)
            .map_err(|e| format!("VP8 encode: {e}"))?;

        let result = packets
            .into_iter()
            .map(|p| Vp8Packet {
                data: p.data.to_vec(),
                is_keyframe: p.key,
                pts: p.pts,
            })
            .collect();

        self.pts += 1;
        Ok(result)
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
}

/// VP8 decoder using raw libvpx FFI via `vpx-encode`'s bundled vpx_sys.
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
    pub fn new() -> Result<Self, String> {
        use std::mem::MaybeUninit;
        use vpx_sys::*;

        unsafe {
            let mut ctx = MaybeUninit::uninit();
            let cfg = MaybeUninit::<vpx_codec_dec_cfg_t>::zeroed();

            let ret = vpx_codec_dec_init_ver(
                ctx.as_mut_ptr(),
                vpx_codec_vp8_dx(),
                cfg.as_ptr(),
                0,
                VPX_DECODER_ABI_VERSION as i32,
            );

            if ret != VPX_CODEC_OK {
                return Err(format!("VP8 decoder init failed: error code {ret:?}"));
            }

            Ok(Self {
                ctx: ctx.assume_init(),
            })
        }
    }

    /// Decode a VP8 packet and return the decoded I420 frame(s).
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<I420Frame>, String> {
        use std::ptr;
        use vpx_sys::*;

        unsafe {
            let ret = vpx_codec_decode(
                &mut self.ctx,
                data.as_ptr(),
                data.len() as u32,
                ptr::null_mut(),
                0,
            );

            if ret != VPX_CODEC_OK {
                return Err(format!("VP8 decode failed: error code {ret:?}"));
            }

            let mut frames = Vec::new();
            let mut iter: vpx_codec_iter_t = ptr::null();

            loop {
                let img = vpx_codec_get_frame(&mut self.ctx, &mut iter);
                if img.is_null() {
                    break;
                }
                let im = &*img;
                let w = im.d_w;
                let h = im.d_h;

                let y_stride = im.stride[0] as usize;
                let u_stride = im.stride[1] as usize;
                let v_stride = im.stride[2] as usize;

                let mut y = vec![0u8; (w * h) as usize];
                let mut u = vec![0u8; ((w / 2) * (h / 2)) as usize];
                let mut v = vec![0u8; ((w / 2) * (h / 2)) as usize];

                // Copy row by row (stride may differ from width)
                for row in 0..h as usize {
                    let src = std::slice::from_raw_parts(
                        im.planes[0].add(row * y_stride),
                        w as usize,
                    );
                    y[row * w as usize..(row + 1) * w as usize].copy_from_slice(src);
                }
                for row in 0..(h / 2) as usize {
                    let src_u = std::slice::from_raw_parts(
                        im.planes[1].add(row * u_stride),
                        (w / 2) as usize,
                    );
                    let src_v = std::slice::from_raw_parts(
                        im.planes[2].add(row * v_stride),
                        (w / 2) as usize,
                    );
                    u[row * (w / 2) as usize..(row + 1) * (w / 2) as usize]
                        .copy_from_slice(src_u);
                    v[row * (w / 2) as usize..(row + 1) * (w / 2) as usize]
                        .copy_from_slice(src_v);
                }

                frames.push(I420Frame {
                    width: w,
                    height: h,
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
        unsafe {
            vpx_sys::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vp8_encode_decode_roundtrip() {
        let width = 320u32;
        let height = 240u32;

        // Create a solid-color I420 frame (green-ish)
        let y_plane = vec![150u8; (width * height) as usize];
        let u_plane = vec![128u8; ((width / 2) * (height / 2)) as usize];
        let v_plane = vec![128u8; ((width / 2) * (height / 2)) as usize];

        let frame = I420Frame {
            width,
            height,
            y: y_plane.clone(),
            u: u_plane.clone(),
            v: v_plane.clone(),
            timestamp_us: 0,
        };

        let mut encoder = Vp8Encoder::new(width, height, 500).expect("encoder creation");
        let mut decoder = Vp8Decoder::new().expect("decoder creation");

        // Encode the frame
        let packets = encoder.encode(&frame).expect("encode");
        assert!(!packets.is_empty(), "Should produce at least one packet");
        assert!(packets[0].is_keyframe, "First frame should be keyframe");

        // Decode the packet
        let decoded_frames = decoder.decode(&packets[0].data).expect("decode");
        assert_eq!(decoded_frames.len(), 1, "Should decode exactly one frame");

        let decoded = &decoded_frames[0];
        assert_eq!(decoded.width, width);
        assert_eq!(decoded.height, height);
        assert_eq!(decoded.y.len(), (width * height) as usize);
        assert_eq!(decoded.u.len(), ((width / 2) * (height / 2)) as usize);

        // Check Y values are approximately correct (lossy codec)
        let avg_y: f64 =
            decoded.y.iter().map(|&v| v as f64).sum::<f64>() / decoded.y.len() as f64;
        assert!(
            (avg_y - 150.0).abs() < 10.0,
            "Average Y should be ~150, got {avg_y}"
        );
    }
}
