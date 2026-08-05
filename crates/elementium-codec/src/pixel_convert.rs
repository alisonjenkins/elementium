use elementium_types::{I420Frame, VideoFrame};

/// Converts a `u32` frame dimension to `usize`.
// u32 always fits in usize on the 32/64-bit platforms this crate targets.
#[allow(clippy::expect_used)]
fn dim(n: u32) -> usize {
    usize::try_from(n).expect("u32 fits in usize on supported platforms")
}

/// Halves a plane dimension for 4:2:0 chroma subsampling. Division by the
/// literal 2 cannot overflow or panic (the only failure mode, div-by-zero,
/// is impossible with a nonzero literal divisor).
#[allow(clippy::arithmetic_side_effects)]
const fn half(n: usize) -> usize {
    n / 2
}

/// Quantizes a color-space conversion result to a byte. The value is
/// clamped to the valid 0.0-255.0 range first, so the truncation is an
/// intentional, bounded lossy step of the conversion (not an overflow bug).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::as_conversions
)]
const fn quantize(v: f32) -> u8 {
    v.clamp(0.0, 255.0) as u8
}

/// BT.601 RGB -> luma.
fn luma(r: f32, g: f32, b: f32) -> u8 {
    quantize(b.mul_add(0.114, g.mul_add(0.587, r * 0.299)))
}

/// BT.601 RGB -> chroma (U, V).
fn chroma(r: f32, g: f32, b: f32) -> (u8, u8) {
    let chroma_u = quantize(b.mul_add(0.500, g.mul_add(-0.331, r.mul_add(-0.169, 128.0))));
    let chroma_v = quantize(b.mul_add(-0.081, g.mul_add(-0.419, r.mul_add(0.500, 128.0))));
    (chroma_u, chroma_v)
}

/// Shared row/column-subsampled RGB(A)-family -> I420 conversion.
///
/// `stride` is the number of bytes per pixel in `data`, and `extract` pulls
/// the (r, g, b) triple (as widened `f32`s) out of one pixel's byte slice.
fn convert_to_i420(
    width: u32,
    height: u32,
    data: &[u8],
    stride: usize,
    extract: impl Fn(&[u8]) -> Option<(f32, f32, f32)>,
) -> I420Frame {
    let w = dim(width);
    let h = dim(height);
    let uv_w = half(w);
    let uv_h = half(h);

    let mut y_plane = vec![0u8; w.saturating_mul(h)];
    let mut u_plane = vec![0u8; uv_w.saturating_mul(uv_h)];
    let mut v_plane = vec![0u8; uv_w.saturating_mul(uv_h)];

    let row_bytes = w.saturating_mul(stride);

    for (y_row, data_row) in y_plane.chunks_mut(w).zip(data.chunks_exact(row_bytes)) {
        for (y_px, px) in y_row.iter_mut().zip(data_row.chunks_exact(stride)) {
            if let Some((r, g, b)) = extract(px) {
                *y_px = luma(r, g, b);
            }
        }
    }

    for ((u_row, v_row), data_row) in u_plane
        .chunks_mut(uv_w)
        .zip(v_plane.chunks_mut(uv_w))
        .zip(data.chunks_exact(row_bytes).step_by(2))
    {
        for ((u_px, v_px), px) in u_row
            .iter_mut()
            .zip(v_row.iter_mut())
            .zip(data_row.chunks_exact(stride).step_by(2))
        {
            if let Some((r, g, b)) = extract(px) {
                let (u, v) = chroma(r, g, b);
                *u_px = u;
                *v_px = v;
            }
        }
    }

    I420Frame {
        width,
        height,
        y: y_plane,
        u: u_plane,
        v: v_plane,
        timestamp_us: 0,
    }
}

/// Convert BGRA pixel data to I420 (YUV 4:2:0 planar).
#[must_use]
pub fn bgra_to_i420(width: u32, height: u32, bgra: &[u8]) -> I420Frame {
    convert_to_i420(width, height, bgra, 4, |px| match *px {
        [b, g, r, _a] => Some((f32::from(r), f32::from(g), f32::from(b))),
        _ => None,
    })
}

/// Convert RGBA pixel data (4 bytes per pixel) to I420 (YUV 4:2:0 planar).
/// Used for camera frames that have been converted to RGBA format.
#[must_use]
pub fn rgba_to_i420(width: u32, height: u32, rgba: &[u8]) -> I420Frame {
    convert_to_i420(width, height, rgba, 4, |px| match *px {
        [r, g, b, _a] => Some((f32::from(r), f32::from(g), f32::from(b))),
        _ => None,
    })
}

/// Convert RGB pixel data (3 bytes per pixel) to I420 (YUV 4:2:0 planar).
/// Used for camera frames from nokhwa which outputs RGB.
#[must_use]
pub fn rgb_to_i420(width: u32, height: u32, rgb: &[u8]) -> I420Frame {
    convert_to_i420(width, height, rgb, 3, |px| match *px {
        [r, g, b] => Some((f32::from(r), f32::from(g), f32::from(b))),
        _ => None,
    })
}

/// Convert I420 (YUV 4:2:0 planar) to RGBA pixel data.
#[must_use]
pub fn i420_to_rgba(frame: &I420Frame) -> VideoFrame {
    let img_w = dim(frame.width);
    let img_h = dim(frame.height);
    let uv_w = half(img_w);

    let mut rgba = vec![0u8; img_w.saturating_mul(img_h).saturating_mul(4)];
    let row_bytes = img_w.saturating_mul(4);

    for (row_idx, (rgba_row, y_row)) in rgba
        .chunks_mut(row_bytes)
        .zip(frame.y.chunks(img_w))
        .enumerate()
    {
        let uv_row = half(row_idx);
        let uv_start = uv_row.saturating_mul(uv_w);
        let Some(blue_chroma_row) = frame.u.get(uv_start..uv_start.saturating_add(uv_w)) else {
            continue;
        };
        let Some(red_chroma_row) = frame.v.get(uv_start..uv_start.saturating_add(uv_w)) else {
            continue;
        };

        for (col_idx, (rgba_px, &y_u8)) in rgba_row.chunks_exact_mut(4).zip(y_row).enumerate() {
            let uv_col = half(col_idx);
            let (Some(&u_u8), Some(&v_u8)) = (blue_chroma_row.get(uv_col), red_chroma_row.get(uv_col)) else {
                continue;
            };

            let y_val = f32::from(y_u8);
            let u_val = f32::from(u_u8) - 128.0;
            let v_val = f32::from(v_u8) - 128.0;

            let r = quantize(v_val.mul_add(1.402, y_val));
            let g = quantize(u_val.mul_add(-0.344, v_val.mul_add(-0.714, y_val)));
            let b = quantize(u_val.mul_add(1.772, y_val));

            if let [rp, gp, bp, ap] = rgba_px {
                *rp = r;
                *gp = g;
                *bp = b;
                *ap = 255;
            }
        }
    }

    VideoFrame {
        width: frame.width,
        height: frame.height,
        data: rgba,
        timestamp_us: frame.timestamp_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Test-only: panicking on failure via expect is the idiomatic way to
    // fail a `#[test]`.
    #[allow(clippy::expect_used)]
    fn bgra_i420_roundtrip() {
        let width = 4u32;
        let height = 4u32;
        // Create a simple test pattern: solid red in BGRA
        let pixel_count = dim(width).saturating_mul(dim(height));
        let mut bgra = vec![0u8; pixel_count.saturating_mul(4)];
        for px in bgra.chunks_exact_mut(4) {
            if let [b, g, r, a] = px {
                *b = 0;
                *g = 0;
                *r = 255;
                *a = 255;
            }
        }

        let i420 = bgra_to_i420(width, height, &bgra);
        assert_eq!(i420.y.len(), 16);
        assert_eq!(i420.u.len(), 4);
        assert_eq!(i420.v.len(), 4);

        let rgba = i420_to_rgba(&i420);
        assert_eq!(rgba.data.len(), pixel_count.saturating_mul(4));
        // Check first pixel is approximately red (lossy conversion)
        let [r, g, b, a] = rgba
            .data
            .first_chunk::<4>()
            .copied()
            .expect("rgba buffer should have at least one pixel");
        assert!(r > 200); // R
        assert!(g < 50); // G
        assert!(b < 50); // B
        assert_eq!(a, 255); // A
    }
}
