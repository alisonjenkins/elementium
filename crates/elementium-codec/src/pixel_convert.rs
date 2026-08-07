use elementium_types::{I420Frame, VideoFrame};
use yuv::{
    BufferStoreMut, YuvConversionMode, YuvPlanarImage, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix,
};

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

/// The color conversion standard used throughout this module. Matches the BT.601 full-range
/// (0-255, not studio-range 16-235/16-240) coefficients the hand-rolled math this module used
/// to use, so swapping to the `yuv` crate's SIMD-accelerated (AVX2/SSE/NEON, with scalar
/// fallback) implementation doesn't change output pixel values.
const RANGE: YuvRange = YuvRange::Full;
const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt601;

/// Build a same-size zeroed `I420Frame`, used as the fail-safe fallback when a `yuv` crate
/// conversion call errors (e.g. malformed/odd dimensions) -- callers of these `pub fn`s expect
/// an infallible, always-correctly-sized frame back, matching the pre-SIMD behavior of quietly
/// leaving pixels at their zeroed default rather than panicking or propagating a `Result`.
fn empty_i420(width: u32, height: u32) -> I420Frame {
    let w = dim(width);
    let h = dim(height);
    let uv_w = half(w);
    let uv_h = half(h);
    I420Frame {
        width,
        height,
        y: vec![0u8; w.saturating_mul(h)],
        u: vec![0u8; uv_w.saturating_mul(uv_h)],
        v: vec![0u8; uv_w.saturating_mul(uv_h)],
        timestamp_us: 0,
    }
}

/// Shared RGB(A)-family -> I420 conversion, dispatching to the `yuv` crate's SIMD-accelerated
/// converter for the given source channel layout.
fn convert_to_i420(
    width: u32,
    height: u32,
    data: &[u8],
    stride: usize,
    convert: impl FnOnce(&mut YuvPlanarImageMut<'_, u8>, &[u8], u32) -> Result<(), yuv::YuvError>,
) -> I420Frame {
    let w = dim(width);
    let h = dim(height);
    let uv_w = half(w);
    let uv_h = half(h);

    let mut y_plane = vec![0u8; w.saturating_mul(h)];
    let mut u_plane = vec![0u8; uv_w.saturating_mul(uv_h)];
    let mut v_plane = vec![0u8; uv_w.saturating_mul(uv_h)];

    let mut target = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(&mut y_plane),
        y_stride: width,
        u_plane: BufferStoreMut::Borrowed(&mut u_plane),
        u_stride: u32::try_from(uv_w).unwrap_or(0),
        v_plane: BufferStoreMut::Borrowed(&mut v_plane),
        v_stride: u32::try_from(uv_w).unwrap_or(0),
        width,
        height,
    };

    let row_stride = u32::try_from(w.saturating_mul(stride)).unwrap_or(0);
    if let Err(e) = convert(&mut target, data, row_stride) {
        tracing::error!(width, height, error = %e, "SIMD RGB->I420 conversion failed, returning blank frame");
        return empty_i420(width, height);
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
    convert_to_i420(width, height, bgra, 4, |target, data, stride| {
        yuv::bgra_to_yuv420(target, data, stride, RANGE, MATRIX, YuvConversionMode::default())
    })
}

/// Convert RGBA pixel data (4 bytes per pixel) to I420 (YUV 4:2:0 planar).
/// Used for camera frames that have been converted to RGBA format.
#[must_use]
pub fn rgba_to_i420(width: u32, height: u32, rgba: &[u8]) -> I420Frame {
    convert_to_i420(width, height, rgba, 4, |target, data, stride| {
        yuv::rgba_to_yuv420(target, data, stride, RANGE, MATRIX, YuvConversionMode::default())
    })
}

/// Convert RGB pixel data (3 bytes per pixel) to I420 (YUV 4:2:0 planar).
/// Used for camera frames from nokhwa which outputs RGB.
#[must_use]
pub fn rgb_to_i420(width: u32, height: u32, rgb: &[u8]) -> I420Frame {
    convert_to_i420(width, height, rgb, 3, |target, data, stride| {
        yuv::rgb_to_yuv420(target, data, stride, RANGE, MATRIX, YuvConversionMode::default())
    })
}

/// Halve an RGBA image's width and height, averaging each 2x2 block.
///
/// For the local self-view, which is displayed a few centimetres across and does not need
/// the camera's full resolution. A 1280x720 RGBA frame is 3.7MB, and every one of them
/// crosses the Rust-to-webview IPC boundary; at 30fps that is 110MB/s of copying to draw a
/// thumbnail. Halving each axis cuts it fourfold.
///
/// Averaging rather than dropping pixels: nearest-neighbour on a downscale aliases badly
/// on exactly the high-contrast detail a webcam view is full of (hair, text, edges), and
/// the cost of the average is trivial next to the copy it saves.
///
/// Returns `None` for degenerate geometry or a buffer that does not match it, rather than
/// producing a subtly wrong image from a wrong assumption.
#[must_use]
pub fn halve_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    if w < 2 || h < 2 || rgba.len() < w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }

    // Odd dimensions lose their last row/column, which is invisible at preview size and
    // avoids a special case that would otherwise read past the edge.
    let (out_w, out_h) = (w / 2, h / 2);
    let mut out = Vec::with_capacity(out_w.saturating_mul(out_h).saturating_mul(4));

    for y in 0..out_h {
        let row0 = y.saturating_mul(2).saturating_mul(w).saturating_mul(4);
        let row1 = row0.saturating_add(w.saturating_mul(4));
        for x in 0..out_w {
            let col = x.saturating_mul(8);
            for channel in 0..4_usize {
                let mut sum = 0_u16;
                for base in [row0, row1] {
                    for pixel in [0_usize, 4] {
                        let at = base
                            .saturating_add(col)
                            .saturating_add(pixel)
                            .saturating_add(channel);
                        sum = sum.saturating_add(u16::from(*rgba.get(at)?));
                    }
                }
                out.push(u8::try_from(sum / 4).unwrap_or(u8::MAX));
            }
        }
    }

    Some((out, u32::try_from(out_w).ok()?, u32::try_from(out_h).ok()?))
}

/// Convert I420 (YUV 4:2:0 planar) to RGBA pixel data.
#[must_use]
pub fn i420_to_rgba(frame: &I420Frame) -> VideoFrame {
    let img_w = dim(frame.width);
    let img_h = dim(frame.height);
    let uv_w = half(img_w);

    let mut rgba = vec![0u8; img_w.saturating_mul(img_h).saturating_mul(4)];

    let source = YuvPlanarImage {
        y_plane: &frame.y,
        y_stride: frame.width,
        u_plane: &frame.u,
        u_stride: u32::try_from(uv_w).unwrap_or(0),
        v_plane: &frame.v,
        v_stride: u32::try_from(uv_w).unwrap_or(0),
        width: frame.width,
        height: frame.height,
    };
    let rgba_stride = u32::try_from(img_w.saturating_mul(4)).unwrap_or(0);

    if let Err(e) = yuv::yuv420_to_rgba(&source, &mut rgba, rgba_stride, RANGE, MATRIX) {
        tracing::error!(
            width = frame.width,
            height = frame.height,
            error = %e,
            "SIMD I420->RGBA conversion failed, returning blank frame"
        );
        rgba.fill(0);
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod halve_tests {
    use super::halve_rgba;

    /// A 2x2 block of one colour must halve to that same colour, not to something the
    /// averaging arithmetic mangled.
    #[test]
    fn a_uniform_image_keeps_its_colour() {
        let src: Vec<u8> = [10_u8, 20, 30, 255].repeat(4 * 4);
        let (out, w, h) = halve_rgba(4, 4, &src).expect("halved");
        assert_eq!((w, h), (2, 2));
        assert_eq!(out.len(), 2 * 2 * 4);
        assert!(
            out.chunks_exact(4).all(|px| px == [10, 20, 30, 255]),
            "got {out:?}"
        );
    }

    /// Each output pixel must be the mean of its 2x2 source block -- a dropped-pixel
    /// downscale would return one corner's value instead.
    #[test]
    fn each_output_pixel_averages_its_source_block() {
        // One 2x2 block: red channel 0, 100, 200, 255 -> mean 138 (integer division).
        let src: Vec<u8> = vec![
            0, 0, 0, 255, 100, 0, 0, 255, // top row: two pixels
            200, 0, 0, 255, 252, 0, 0, 255, // bottom row
        ];
        let (out, _, _) = halve_rgba(2, 2, &src).expect("halved");
        assert_eq!(out.first().copied(), Some(138), "mean of 0, 100, 200, 252");
    }

    /// The whole point is the byte count: a preview frame must be a quarter the size.
    #[test]
    fn the_output_is_a_quarter_of_the_input() {
        let src = vec![0_u8; 1280 * 720 * 4];
        let (out, w, h) = halve_rgba(1280, 720, &src).expect("halved");
        assert_eq!((w, h), (640, 360));
        assert_eq!(out.len(), src.len() / 4);
        assert_eq!(out.len(), 640 * 360 * 4);
    }

    /// A buffer that does not match its claimed geometry is refused, not silently
    /// half-processed -- the bug this whole area has had twice already.
    #[test]
    fn a_short_buffer_is_refused() {
        assert!(halve_rgba(1280, 720, &[0_u8; 100]).is_none());
    }

    /// Nothing to halve.
    #[test]
    fn degenerate_geometry_is_refused() {
        assert!(halve_rgba(1, 1, &[0_u8; 4]).is_none());
        assert!(halve_rgba(0, 0, &[]).is_none());
    }
}
