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

/// Halves a dimension when *downscaling*. Truncating is correct here: a 2x2 average has no
/// meaning for a trailing odd row or column, so the output simply omits it.
///
/// Division by the literal 2 cannot overflow or panic (the only failure mode, div-by-zero,
/// is impossible with a nonzero literal divisor).
#[allow(clippy::arithmetic_side_effects)]
const fn half(n: usize) -> usize {
    n / 2
}

/// The size of a 4:2:0 chroma plane dimension for a frame of dimension `n`.
///
/// Rounds *up*, and the distinction from [`half`] is not pedantry: I420 stores one chroma
/// sample per 2x2 block, so an odd dimension still needs a whole trailing sample to cover
/// its last row or column. Truncating instead under-allocates the U and V planes by one
/// row or column, which every conversion then rejects.
///
/// Only reachable with an odd dimension, which is why it survived: a camera negotiates even
/// geometry, so nothing hit it until a screen share of a window did -- a window is whatever
/// width the user dragged it to. The symptom was a blank frame and
/// `Chroma plane have invalid size, it must be at least 598956, but it was 598253` at
/// 1703x1406, followed by a VP8 encoder refusing to initialise at 0x0.
const fn chroma_dim(n: usize) -> usize {
    n.div_ceil(2)
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
    let uv_w = chroma_dim(w);
    let uv_h = chroma_dim(h);
    // `from_padded` rather than `from_planes`: a zeroed buffer of exactly the right size
    // is trivially valid at tight strides, so this cannot fail for any geometry that fits
    // in memory, and expressing it this way avoids needing a fallback that cannot happen.
    let luma = w.saturating_mul(h);
    let chroma = uv_w.saturating_mul(uv_h);
    let bytes = luma.saturating_add(chroma.saturating_mul(2));
    I420Frame::from_padded(width, height, vec![0u8; bytes], w, uv_w, 0).unwrap_or_else(|| {
        tracing::error!(width, height, "could not build a blank I420 frame");
        I420Frame::from_padded(0, 0, Vec::new(), 0, 0, 0).unwrap_or_default()
    })
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
    let uv_w = chroma_dim(w);
    let uv_h = chroma_dim(h);

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

    I420Frame::from_planes(width, height, &y_plane, &u_plane, &v_plane, 0)
        .unwrap_or_else(|| empty_i420(width, height))
}

/// Convert BGRA pixel data to I420 (YUV 4:2:0 planar).
#[must_use]
pub fn bgra_to_i420(width: u32, height: u32, bgra: &[u8]) -> I420Frame {
    convert_to_i420(width, height, bgra, 4, |target, data, stride| {
        yuv::bgra_to_yuv420(
            target,
            data,
            stride,
            RANGE,
            MATRIX,
            YuvConversionMode::default(),
        )
    })
}

/// Convert RGBA pixel data (4 bytes per pixel) to I420 (YUV 4:2:0 planar).
/// Used for camera frames that have been converted to RGBA format.
#[must_use]
pub fn rgba_to_i420(width: u32, height: u32, rgba: &[u8]) -> I420Frame {
    convert_to_i420(width, height, rgba, 4, |target, data, stride| {
        yuv::rgba_to_yuv420(
            target,
            data,
            stride,
            RANGE,
            MATRIX,
            YuvConversionMode::default(),
        )
    })
}

/// Convert RGB pixel data (3 bytes per pixel) to I420 (YUV 4:2:0 planar).
/// Used for camera frames from nokhwa which outputs RGB.
#[must_use]
pub fn rgb_to_i420(width: u32, height: u32, rgb: &[u8]) -> I420Frame {
    convert_to_i420(width, height, rgb, 3, |target, data, stride| {
        yuv::rgb_to_yuv420(
            target,
            data,
            stride,
            RANGE,
            MATRIX,
            YuvConversionMode::default(),
        )
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
    let row_bytes = w.checked_mul(4)?;
    if w < 2 || h < 2 || rgba.len() < row_bytes.checked_mul(h)? {
        return None;
    }

    // Odd dimensions lose their last row/column, which is invisible at preview size and
    // avoids a special case that would otherwise read past the edge.
    let (out_w, out_h) = (w / 2, h / 2);
    let mut out = Vec::with_capacity(out_w.saturating_mul(out_h).saturating_mul(4));

    for y in 0..out_h {
        let top_start = y.checked_mul(2)?.checked_mul(row_bytes)?;
        let bottom_start = top_start.checked_add(row_bytes)?;
        let top = rgba.get(top_start..top_start.checked_add(row_bytes)?)?;
        let bottom = rgba.get(bottom_start..bottom_start.checked_add(row_bytes)?)?;

        // Two source pixels at a time, as fixed-size arrays. The bounds check happens once
        // per 8 bytes instead of once per byte, and the channels are unrolled rather than
        // indexed -- which matters far more than it looks: at one checked read per byte
        // this took 111ms per 720p frame in a debug build, an 9fps ceiling on the camera
        // thread, while costing 1.4ms in release. `just dev` builds debug, so the
        // unoptimised cost is the one developers actually run into.
        for (t, b) in top.chunks_exact(8).zip(bottom.chunks_exact(8)) {
            let [t0, t1, t2, t3, t4, t5, t6, t7]: [u8; 8] = t.try_into().ok()?;
            let [b0, b1, b2, b3, b4, b5, b6, b7]: [u8; 8] = b.try_into().ok()?;
            out.push(mean4(t0, t4, b0, b4));
            out.push(mean4(t1, t5, b1, b5));
            out.push(mean4(t2, t6, b2, b6));
            out.push(mean4(t3, t7, b3, b7));
        }
    }

    Some((out, u32::try_from(out_w).ok()?, u32::try_from(out_h).ok()?))
}

/// Mean of four samples. Cannot overflow: four `u8`s sum to at most 1020.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::as_conversions
)]
const fn mean4(a: u8, b: u8, c: u8, d: u8) -> u8 {
    ((a as u16 + b as u16 + c as u16 + d as u16) / 4) as u8
}

/// Halve an I420 frame's width and height, averaging each 2x2 block per plane.
///
/// The self-view is displayed a few centimetres across and does not need the camera's full
/// resolution, and halving before converting to RGBA is far cheaper than the reverse: I420
/// is 1.5 bytes per pixel against RGBA's 4, so this touches under 40% of the memory the
/// equivalent RGBA downscale does, and the conversion that follows then runs on a quarter
/// of the pixels.
///
/// Chroma planes are already half resolution, so halving them again is a 2x2 average of
/// the subsampled planes -- correct, because both planes are being scaled by the same
/// factor.
///
/// Returns `None` for degenerate geometry, or for planes that do not match the frame's
/// stated size, rather than producing a subtly wrong image from a wrong assumption.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn halve_i420(frame: &I420Frame) -> Option<I420Frame> {
    let (w, h) = (dim(frame.width()), dim(frame.height()));
    if w < 4 || h < 4 {
        return None;
    }
    let (uv_w, uv_h) = (chroma_dim(w), chroma_dim(h));

    // Rows are read at the frame's stride and written tightly packed: the result is ours,
    // so it has no reason to carry the source's padding.
    // The output geometry first, because the chroma planes have to be sized to cover *it*,
    // not to be a naive halving of the source's chroma. For an odd result those differ by a
    // row, and the frame is refused at construction if they disagree.
    let (out_w, out_h) = (half(w), half(h));
    let (out_uv_w, out_uv_h) = (chroma_dim(out_w), chroma_dim(out_h));

    let y = halve_plane(frame.y(), w, h, frame.y_stride(), out_w, out_h)?;
    let u = halve_plane(frame.u(), uv_w, uv_h, frame.uv_stride(), out_uv_w, out_uv_h)?;
    let v = halve_plane(frame.v(), uv_w, uv_h, frame.uv_stride(), out_uv_w, out_uv_h)?;

    I420Frame::from_planes(
        u32::try_from(out_w).ok()?,
        u32::try_from(out_h).ok()?,
        &y,
        &u,
        &v,
        frame.timestamp_us(),
    )
}

/// Halve one 8-bit plane, averaging each 2x2 block.
///
/// Two rows at a time as fixed-size pairs, so the bounds check happens once per pair
/// rather than once per sample -- the difference measured 7x on the RGBA equivalent in an
/// unoptimised build, which is what `just dev` produces.
/// `out_w`/`out_h` are given by the caller rather than derived, because luma and chroma
/// want different rounding from the same source. The output frame's luma is `width/2`
/// truncated, but its chroma must cover that luma with a sample per 2x2 block -- which for
/// an odd result is one more row or column than truncation yields. Deriving both from the
/// source produced a chroma plane one row short, and `I420Frame::from_planes` correctly
/// refused the frame, so an odd geometry lost its self-view entirely.
///
/// A source row or column with no partner is averaged with itself rather than dropped: the
/// alternative is an output that silently omits the last row of the picture.
fn halve_plane(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    out_w: usize,
    out_h: usize,
) -> Option<Vec<u8>> {
    if stride < width {
        return None;
    }
    let mut out = Vec::with_capacity(out_w.saturating_mul(out_h));

    for y in 0..out_h {
        let top_y = y.checked_mul(2)?;
        // Clamped, not assumed: `top_y + 1` is off the end for the trailing row of an odd
        // source, and reading it would either fail or splice in the next plane's bytes.
        let bottom_y = top_y.checked_add(1)?.min(height.saturating_sub(1));
        let top_start = top_y.checked_mul(stride)?;
        let bottom_start = bottom_y.checked_mul(stride)?;
        let top = src.get(top_start..top_start.checked_add(width)?)?;
        let bottom = src.get(bottom_start..bottom_start.checked_add(width)?)?;

        for x in 0..out_w {
            let left = x.checked_mul(2)?;
            let right = left.checked_add(1)?.min(width.saturating_sub(1));
            let (t0, t1) = (*top.get(left)?, *top.get(right)?);
            let (b0, b1) = (*bottom.get(left)?, *bottom.get(right)?);
            out.push(mean4(t0, t1, b0, b1));
        }
    }

    Some(out)
}

/// Convert I420 (YUV 4:2:0 planar) to RGBA pixel data.
#[must_use]
pub fn i420_to_rgba(frame: &I420Frame) -> VideoFrame {
    let img_w = dim(frame.width());
    let img_h = dim(frame.height());

    let mut rgba = vec![0u8; img_w.saturating_mul(img_h).saturating_mul(4)];

    let source = YuvPlanarImage {
        y_plane: frame.y(),
        // The frame's own strides, not its width: a frame adopted from a decoder has
        // padded rows, and reading it at width would shear the picture.
        y_stride: u32::try_from(frame.y_stride()).unwrap_or(0),
        u_plane: frame.u(),
        u_stride: u32::try_from(frame.uv_stride()).unwrap_or(0),
        v_plane: frame.v(),
        v_stride: u32::try_from(frame.uv_stride()).unwrap_or(0),
        width: frame.width(),
        height: frame.height(),
    };
    let rgba_stride = u32::try_from(img_w.saturating_mul(4)).unwrap_or(0);

    if let Err(e) = yuv::yuv420_to_rgba(&source, &mut rgba, rgba_stride, RANGE, MATRIX) {
        tracing::error!(
            width = frame.width(),
            height = frame.height(),
            error = %e,
            "SIMD I420->RGBA conversion failed, returning blank frame"
        );
        rgba.fill(0);
    }

    VideoFrame {
        width: frame.width(),
        height: frame.height(),
        data: rgba,
        timestamp_us: frame.timestamp_us(),
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
        assert_eq!(i420.y().len(), 16);
        assert_eq!(i420.u().len(), 4);
        assert_eq!(i420.v().len(), 4);

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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::arithmetic_side_effects,
    clippy::many_single_char_names
)]
mod halve_i420_tests {
    use super::halve_i420;
    use elementium_types::I420Frame;

    fn frame(width: u32, height: u32, y: u8, u: u8, v: u8) -> I420Frame {
        let (w, h) = (width as usize, height as usize);
        let uv = (w / 2) * (h / 2);
        I420Frame::from_planes(
            width,
            height,
            &vec![y; w * h],
            &vec![u; uv],
            &vec![v; uv],
            7,
        )
        .expect("planes match the geometry")
    }

    /// Halving must produce a frame whose planes match its stated geometry. A frame whose
    /// planes disagree with its size is the exact fault that shears a picture into bands.
    #[test]
    fn the_result_is_self_consistent() {
        let out = halve_i420(&frame(1280, 720, 100, 110, 120)).expect("halved");
        assert_eq!((out.width(), out.height()), (640, 360));
        assert_eq!(out.y().len(), 640 * 360);
        assert_eq!(out.u().len(), 320 * 180);
        assert_eq!(out.v().len(), 320 * 180);
        assert_eq!(out.timestamp_us(), 7, "the frame's time must survive");
    }

    /// A uniform frame must keep its values: the averaging must not drift.
    #[test]
    fn a_uniform_frame_keeps_its_values() {
        let out = halve_i420(&frame(64, 64, 100, 110, 120)).expect("halved");
        assert!(out.y().iter().all(|&s| s == 100));
        assert!(out.u().iter().all(|&s| s == 110));
        assert!(out.v().iter().all(|&s| s == 120));
    }

    /// Each output sample is the mean of its 2x2 source block, not a dropped pixel.
    #[test]
    fn each_sample_averages_its_block() {
        // Top-left 2x2 of the Y plane: 0, 100, 200, 252 -> mean 138.
        let mut y = vec![0_u8; 16];
        y[0] = 0;
        y[1] = 100;
        y[4] = 200;
        y[5] = 252;
        let f = I420Frame::from_planes(4, 4, &y, &[128; 4], &[128; 4], 0).expect("frame");
        let out = halve_i420(&f).expect("halved");
        assert_eq!(out.y().first().copied(), Some(138));
    }

    /// Planes that do not match the stated geometry are refused at construction, so a
    /// frame that could be misread cannot be built in the first place.
    #[test]
    fn a_frame_with_short_planes_cannot_be_built() {
        assert!(I420Frame::from_planes(64, 64, &[0; 10], &[0; 1024], &[0; 1024], 0).is_none());
    }

    /// Nothing useful to halve.
    #[test]
    fn degenerate_geometry_is_refused() {
        assert!(halve_i420(&frame(2, 2, 0, 128, 128)).is_none());
    }
}

// A failed setup step in a test should stop that test loudly; the workspace's `expect_used`
// ban is aimed at the shipping paths, not at assertions.
#[allow(clippy::expect_used)]
#[cfg(test)]
mod odd_geometry_tests {
    use super::{bgra_to_i420, halve_i420};

    /// A frame of odd width must convert to a real picture, not a blank one.
    ///
    /// Found by sharing a window 1703 pixels wide. Nothing before this could produce an odd
    /// dimension -- a camera negotiates even geometry -- so the chroma planes were sized by
    /// truncating division and came out one column short, the conversion refused the frame,
    /// and every frame of that share was blank. The encoder then failed to initialise at
    /// 0x0, which is where it finally became visible.
    #[test]
    fn an_odd_width_frame_converts_to_something_other_than_blank() {
        let (w, h) = (1703_u32, 1406_u32);
        let px = usize::try_from(w).unwrap_or(0) * usize::try_from(h).unwrap_or(0);
        // Mid-grey rather than black, so "blank" and "converted" are distinguishable: a
        // failed conversion returns zeroed planes, and zero is a legitimate luma value.
        let bgra = vec![128_u8; px * 4];

        let frame = bgra_to_i420(w, h, &bgra);

        assert_eq!(frame.width(), w);
        assert_eq!(frame.height(), h);
        assert!(
            frame.y().iter().any(|&v| v != 0),
            "an odd-width frame converted to an all-zero luma plane, which is the blank \
             frame this test exists to catch"
        );
    }

    /// Odd dimensions must survive the self-view halving too.
    #[test]
    fn an_odd_width_frame_can_be_halved() {
        let (w, h) = (1703_u32, 1406_u32);
        let px = usize::try_from(w).unwrap_or(0) * usize::try_from(h).unwrap_or(0);
        let frame = bgra_to_i420(w, h, &vec![128_u8; px * 4]);

        let halved = halve_i420(&frame).expect("an odd-width frame must halve");
        assert_eq!(halved.width(), w / 2);
        assert_eq!(halved.height(), h / 2);
        assert!(halved.y().iter().any(|&v| v != 0));
    }
}
