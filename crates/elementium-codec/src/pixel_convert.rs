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
