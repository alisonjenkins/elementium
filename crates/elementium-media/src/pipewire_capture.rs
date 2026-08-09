//! Capture video frames from a `PipeWire` node.
//!
//! Direct `V4L2` capture cannot work on a desktop where `PipeWire` owns the camera: the
//! daemon holds `/dev/videoN`, so setting a format returns `EBUSY` while the camera is
//! perfectly healthy. Going through `PipeWire` is not a workaround for that — it is how a
//! camera is meant to be used on such a system, and it is the same mechanism the XDG
//! screencast portal hands back for screen sharing, so one implementation serves both.
//!
//! The stream runs on its own thread with its own `PipeWire` main loop, and frames are
//! handed over a bounded channel. A full channel drops the newest frame rather than
//! blocking: stalling the `PipeWire` callback would back up the whole graph, and a dropped
//! frame is cheaper than a stalled camera.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use elementium_types::I420Frame;

use crate::captured_frame::CapturedFrame;

use crate::pipewire_nodes::PipewireError;

/// Frame rate requested when a caller does not ask for one.
///
/// 30 is what video calls are built around. Streaming and screen capture legitimately want
/// 60 or more, which is why this is a default rather than a limit.
pub const DEFAULT_CAPTURE_FPS: u32 = 30;

/// What kind of source a stream is negotiating with.
///
/// Cameras and screens advertise genuinely different things, and a parameter set tuned for
/// one excludes the other outright. Measured, not guessed: asking a 5120x1440 monitor for
/// the camera parameters fails the whole connection with `no more input formats`, because
/// the camera set caps size at 4096 and pins the frame rate to a range starting at 1/1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceProfile {
    /// A camera: bounded resolution, and a rate it sustains continuously.
    Camera,
    /// A screen, window or application granted by the desktop portal.
    ///
    /// Two differences that both matter. Displays are far larger than cameras -- an
    /// ultrawide is 5120 across and 8K is 7680 -- and compositors advertise a frame rate of
    /// `0/1`, meaning *variable*: frames are produced when something changes, not on a
    /// clock. A range starting at 1/1 excludes that, so a still screen matches nothing.
    Screencast,
}

impl SourceProfile {
    /// The largest frame this profile should agree to.
    const fn max_size(self) -> libspa::utils::Rectangle {
        match self {
            // Bigger than any webcam, small enough to rule out a nonsense negotiation.
            Self::Camera => libspa::utils::Rectangle { width: 4096, height: 4096 },
            // 8K, which is past any display sold today and still a real bound.
            Self::Screencast => libspa::utils::Rectangle { width: 7680, height: 4320 },
        }
    }

    /// The lowest frame rate this profile should agree to.
    ///
    /// `0/1` is not "no frames": in `PipeWire` it means the source does not run on a clock.
    /// Accepting it is what lets a screen that nobody is touching negotiate at all.
    const fn min_framerate(self) -> libspa::utils::Fraction {
        match self {
            Self::Camera => libspa::utils::Fraction { num: 1, denom: 1 },
            Self::Screencast => libspa::utils::Fraction { num: 0, denom: 1 },
        }
    }
}

/// Everything a stream needs to know before it negotiates.
///
/// One struct rather than three parameters threaded through four functions, which is what
/// keeps the signatures inside the workspace's argument-count limit.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest {
    pub target_fps: u32,
    pub target: elementium_codec::EncodeTarget,
    pub profile: SourceProfile,
}

/// Frames buffered before the oldest is dropped.
///
/// Small on purpose: video is only useful live, and a deep queue converts a slow consumer
/// into growing latency instead of visible frame loss.
const FRAME_QUEUE_DEPTH: usize = 4;

/// Pixel layouts a `PipeWire` video source may hand us, of the ones we can convert.
///
/// Kept as our own enum rather than passing `libspa`'s around so the conversion functions
/// are testable without constructing SPA types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// 4 bytes per pixel, red first, 4th byte unused or alpha.
    Rgbx,
    /// 4 bytes per pixel, blue first, 4th byte unused or alpha. The common webcam and
    /// screen-capture layout.
    Bgrx,
    /// 3 bytes per pixel, red first.
    Rgb,
    /// 3 bytes per pixel, blue first.
    Bgr,
    /// Packed YUV 4:2:2, two pixels per four bytes as `Y0 U Y1 V`.
    ///
    /// What UVC webcams produce natively. `PipeWire` does not put a video converter in the
    /// graph by default, so a client that only offers RGB layouts negotiates a format the
    /// camera cannot fill and receives nothing.
    Yuy2,
}

impl SourceFormat {
    /// Bytes each pixel occupies in the source buffer.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgbx | Self::Bgrx => 4,
            Self::Rgb | Self::Bgr => 3,
            // Not a whole number per pixel; 2 is the average and is what stride maths uses.
            Self::Yuy2 => 2,
        }
    }

    /// Map a `libspa` video format onto what we can convert, or `None` if unsupported.
    #[must_use]
    pub const fn from_spa(format: libspa::param::video::VideoFormat) -> Option<Self> {
        use libspa::param::video::VideoFormat as F;
        match format {
            F::RGBx | F::RGBA => Some(Self::Rgbx),
            F::BGRx | F::BGRA => Some(Self::Bgrx),
            F::RGB => Some(Self::Rgb),
            F::BGR => Some(Self::Bgr),
            F::YUY2 => Some(Self::Yuy2),
            _ => None,
        }
    }
}

/// Convert one row-padded source buffer into tightly packed RGBA.
///
/// `stride` is the source's bytes per row, which is frequently larger than
/// `width * bytes_per_pixel`: `PipeWire` aligns rows, and ignoring that shears the image
/// progressively down the frame — a distinctive diagonal skew that is easy to recognise
/// once seen and easy to miss in a still.
///
/// Returns `None` if the buffer is too small for the claimed geometry, rather than
/// producing a partly-garbage frame.
#[must_use]
pub fn to_rgba(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: SourceFormat,
) -> Option<Vec<u8>> {
    let bpp = format.bytes_per_pixel();
    let row_bytes = width.checked_mul(bpp)?;
    if stride < row_bytes {
        return None;
    }
    let needed = stride
        .checked_mul(height.saturating_sub(1))?
        .checked_add(row_bytes)?;
    if src.len() < needed {
        return None;
    }

    let mut out = Vec::with_capacity(width.checked_mul(height)?.checked_mul(4)?);
    for y in 0..height {
        let row_start = stride.checked_mul(y)?;
        let row = src.get(row_start..row_start.checked_add(row_bytes)?)?;
        if format == SourceFormat::Yuy2 {
            yuy2_row_to_rgba(row, &mut out);
            continue;
        }
        for px in row.chunks_exact(bpp) {
            // Only the channel order differs between the 3- and 4-byte layouts; the
            // 4th byte is discarded because our output is always opaque.
            let (r, g, b, a) = match format {
                SourceFormat::Rgbx | SourceFormat::Rgb => {
                    (px.first()?, px.get(1)?, px.get(2)?, 255)
                }
                SourceFormat::Bgrx | SourceFormat::Bgr => {
                    (px.get(2)?, px.get(1)?, px.first()?, 255)
                }
                // Handled a row at a time above; a packed 4:2:2 pixel is not independent.
                SourceFormat::Yuy2 => return None,
            };
            out.push(*r);
            out.push(*g);
            out.push(*b);
            out.push(a);
        }
    }
    Some(out)
}

/// Convert one packed YUY2 row (`Y0 U Y1 V`, two pixels per four bytes) to RGBA.
///
/// BT.601 full-range coefficients, which is what `V4L2` webcams emit. Chroma is shared
/// between each pixel pair, so both pixels take the same U and V.
fn yuy2_row_to_rgba(row: &[u8], out: &mut Vec<u8>) {
    for quad in row.chunks_exact(4) {
        let (Some(&y0), Some(&u), Some(&y1), Some(&v)) =
            (quad.first(), quad.get(1), quad.get(2), quad.get(3))
        else {
            return;
        };
        push_yuv_pixel(y0, u, v, out);
        push_yuv_pixel(y1, u, v, out);
    }
}

/// One YUV sample to RGBA, clamped into range.
fn push_yuv_pixel(y: u8, u: u8, v: u8, out: &mut Vec<u8>) {
    let y = f32::from(y);
    let u = f32::from(u) - 128.0;
    let v = f32::from(v) - 128.0;
    let clamp = |x: f32| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::as_conversions
        )]
        {
            x.clamp(0.0, 255.0) as u8
        }
    };
    out.push(clamp(1.402_f32.mul_add(v, y)));
    out.push(clamp(
        (-0.714_136_f32).mul_add(v, (-0.344_136_f32).mul_add(u, y)),
    ));
    out.push(clamp(1.772_f32.mul_add(u, y)));
    out.push(255);
}

/// Read the `MediaSubtype` straight out of a negotiated format pod.
///
/// `VideoInfoRaw` only describes raw video, so it reports an `Unknown` pixel format for a
/// compressed stream while still parsing the size -- which reads as "a raw format we do not
/// support" when it is really "not a raw format at all". Those need different handling, so
/// the subtype is read directly.
fn negotiated_subtype(pod: &libspa::pod::Pod) -> Option<u32> {
    let (_, value) =
        libspa::pod::deserialize::PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let libspa::pod::Value::Object(obj) = value else {
        return None;
    };
    obj.properties
        .iter()
        .find(|p| p.key == libspa::param::format::FormatProperties::MediaSubtype.as_raw())
        .and_then(|p| match p.value {
            libspa::pod::Value::Id(id) => Some(id.0),
            _ => None,
        })
}

/// Decode one MJPEG buffer straight to I420 planes.
///
/// JPEG stores YCbCr, which is what a video encoder wants, so decoding to RGB and
/// converting back to YUV is the same work done twice to arrive where the data started.
/// Going direct removes both conversions.
///
/// Two things this must not assume, both of which produced a visibly wrong picture when
/// they were assumed:
///
/// **Rows are padded.** libjpeg-turbo aligns every plane's rows, so a plane's stride is
/// not its width. Splitting the buffer by width alone shears the image.
///
/// **The chroma subsampling is the JPEG's choice, not ours.** UVC cameras commonly emit
/// 4:2:2 MJPEG, and some emit 4:4:4. Reading either as though it were 4:2:0 puts chroma
/// samples where luma is expected: the first version of this function did exactly that,
/// and decoded a solid red image to a murky green. Chroma is resampled to 4:2:0 when the
/// source is not already there.
///
/// Returns `None` on a malformed frame rather than a partial image: a camera occasionally
/// emits a truncated JPEG, and half a frame followed by stale pixels looks like a decoder
/// bug rather than a dropped frame.
///
/// The geometry comes from the JPEG itself rather than from what the stream negotiated,
/// because each image carries its own and they are not guaranteed to agree. A frame
/// labelled with the wrong size is read at the wrong stride by everything downstream --
/// the picture breaks into sliding horizontal bands with rotated colour channels, which
/// looks like a corrupt encoder and is nothing of the kind.
#[must_use]
/// Decode one MJPEG buffer to I420 at half size.
///
/// For the self-view, which is displayed at half size anyway. libjpeg-turbo does this for
/// roughly a quarter of the work of a full decode -- it skips most of each block's
/// coefficients rather than reconstructing them and discarding them afterwards -- so it is
/// materially cheaper than decoding and then shrinking.
///
/// Falls back to a full decode followed by a halving where the scaled decode is refused,
/// which some JPEGs provoke; the picture is the same either way.
pub fn decode_mjpeg_half_scale(jpeg: &[u8]) -> Option<I420Frame> {
    let full = || decode_mjpeg_to_i420(jpeg).map(|f| elementium_codec::halve_i420(&f).unwrap_or(f));

    let Ok(mut decompressor) = turbojpeg::Decompressor::new() else {
        return full();
    };
    if decompressor
        .set_scaling_factor(turbojpeg::ScalingFactor::ONE_HALF)
        .is_err()
    {
        return full();
    }
    let Ok(header) = decompressor.read_header(jpeg) else {
        return full();
    };
    let (width, height) = (
        turbojpeg::ScalingFactor::ONE_HALF.scale(header.width),
        turbojpeg::ScalingFactor::ONE_HALF.scale(header.height),
    );

    // 4:2:0 output regardless of the source's own subsampling, so the result needs no
    // resampling afterwards -- the same reason the full-size decode asks for planes.
    let mut image = turbojpeg::YuvImage {
        pixels: vec![
            0_u8;
            turbojpeg::yuv_pixels_len(width, 4, height, turbojpeg::Subsamp::Sub2x2).ok()?
        ],
        align: 4,
        width,
        height,
        subsamp: turbojpeg::Subsamp::Sub2x2,
    };
    if decompressor
        .decompress_to_yuv(jpeg, image.as_deref_mut())
        .is_err()
    {
        return full();
    }
    planes_from_yuv(&image)
}

/// Build a frame from a decoded 4:2:0 YUV image, adopting its buffer.
fn planes_from_yuv(image: &turbojpeg::YuvImage<Vec<u8>>) -> Option<I420Frame> {
    let (y_stride, _) = image.y_size();
    let (uv_stride, _) = image.uv_size();
    I420Frame::from_padded(
        u32::try_from(image.width).ok()?,
        u32::try_from(image.height).ok()?,
        image.pixels.clone(),
        y_stride,
        uv_stride,
        0,
    )
}

pub fn decode_mjpeg_to_i420(jpeg: &[u8]) -> Option<I420Frame> {
    let yuv = turbojpeg::decompress_to_yuv(jpeg).ok()?;
    let width = u32::try_from(yuv.width).ok()?;
    let height = u32::try_from(yuv.height).ok()?;

    let (y_stride, y_rows) = yuv.y_size();
    let (uv_stride, uv_rows) = yuv.uv_size();

    // I420 is 4:2:0. When the JPEG already is, its buffer is exactly the layout a frame
    // wants and can be adopted whole -- no plane is copied, no row is repacked. That is
    // the common case for a webcam and the reason this path exists.
    if yuv.subsamp == turbojpeg::Subsamp::Sub2x2 {
        return I420Frame::from_padded(width, height, yuv.pixels, y_stride, uv_stride, 0);
    }

    // Otherwise the chroma planes are the wrong shape and have to be resampled, which
    // means a copy. UVC cameras commonly emit 4:2:2 and some emit 4:4:4; reading either as
    // 4:2:0 puts chroma samples where luma is expected, which decodes a solid red image to
    // a murky green.
    let y_bytes = y_stride.checked_mul(y_rows)?;
    let uv_bytes = uv_stride.checked_mul(uv_rows)?;
    let u_start = y_bytes;
    let v_start = u_start.checked_add(uv_bytes)?;
    let end = v_start.checked_add(uv_bytes)?;
    if yuv.pixels.len() < end {
        tracing::warn!(
            len = yuv.pixels.len(),
            needed = end,
            width,
            height,
            "MJPEG decode produced fewer bytes than its own geometry needs; frame dropped"
        );
        return None;
    }

    let w = usize::try_from(width).ok()?;
    let h = usize::try_from(height).ok()?;

    let uv_keep = yuv.uv_width().min(uv_stride);
    let u_src = tight_rows(
        yuv.pixels.get(u_start..v_start)?,
        uv_stride,
        uv_keep,
        uv_rows,
    )?;
    let v_src = tight_rows(yuv.pixels.get(v_start..end)?, uv_stride, uv_keep, uv_rows)?;

    let (chroma_w, chroma_h) = (w.div_ceil(2), h.div_ceil(2));
    let src_w = w.div_ceil(yuv.subsamp.width());
    let src_h = h.div_ceil(yuv.subsamp.height());

    // The luma plane is padded too, and `from_planes` expects tight packing -- so it has
    // to be repacked here rather than handed over as-is.
    let y = tight_rows(yuv.pixels.get(..y_bytes)?, y_stride, w, h)?;

    I420Frame::from_planes(
        width,
        height,
        &y,
        &resample_chroma(&u_src, src_w, src_h, chroma_w, chroma_h)?,
        &resample_chroma(&v_src, src_w, src_h, chroma_w, chroma_h)?,
        0,
    )
}

/// Copy `rows` rows of `keep` bytes each out of a padded plane.
fn tight_rows(src: &[u8], stride: usize, keep: usize, rows: usize) -> Option<Vec<u8>> {
    if stride == keep {
        return Some(src.get(..stride.checked_mul(rows)?)?.to_vec());
    }
    let mut out = Vec::with_capacity(keep.checked_mul(rows)?);
    for row in 0..rows {
        let start = row.checked_mul(stride)?;
        out.extend_from_slice(src.get(start..start.checked_add(keep)?)?);
    }
    Some(out)
}

/// Resample a chroma plane to the dimensions I420 requires.
///
/// Nearest-neighbour: chroma is already half-resolution and is about to be halved again
/// for the preview or handed to an encoder that subsamples it further, so the difference
/// between this and a filtered resample is not visible, while the cost difference is paid
/// on every frame. A source already at the target size is returned untouched.
fn resample_chroma(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Option<Vec<u8>> {
    if src_w == dst_w && src_h == dst_h {
        return Some(src.to_vec());
    }
    if src_w == 0 || src_h == 0 {
        return None;
    }

    let mut out = Vec::with_capacity(dst_w.checked_mul(dst_h)?);
    for y in 0..dst_h {
        let src_y = y
            .checked_mul(src_h)?
            .checked_div(dst_h)?
            .min(src_h.saturating_sub(1));
        let row_start = src_y.checked_mul(src_w)?;
        for x in 0..dst_w {
            let src_x = x
                .checked_mul(src_w)?
                .checked_div(dst_w)?
                .min(src_w.saturating_sub(1));
            out.push(*src.get(row_start.checked_add(src_x)?)?);
        }
    }
    Some(out)
}

/// Turn a planar 4:2:0 buffer into a frame, without converting any colour.
///
/// One copy, out of the driver's buffer and into a frame that adopts it — the same single
/// copy every format pays to escape the race on mapped memory (see the process callback),
/// and nothing on top of it. No colour conversion happens because none is needed: these
/// layouts already carry the 4:2:0 subsampling an encoder wants, which is the entire reason
/// they are worth asking a camera for.
///
/// The output is tightly packed rather than carrying the source's row padding. Since a copy
/// is unavoidable, dropping the padding costs nothing and gives every consumer one less
/// thing to get right.
///
/// Trailing padding on the final row is not required. Drivers size buffers exactly as often
/// as they pad them, and demanding bytes that carry no samples would reject perfectly good
/// frames.
///
/// Returns `None` if the buffer is too small for the claimed geometry, rather than
/// producing a frame that is partly whatever was in memory.
#[must_use]
pub fn planar_to_i420(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: PlanarFormat,
) -> Option<I420Frame> {
    if width == 0 || height == 0 || stride < width {
        return None;
    }
    let (chroma_width, chroma_height) = (width.checked_div(2)?, height.checked_div(2)?);
    let luma_bytes = width.checked_mul(height)?;
    let chroma_bytes = chroma_width.checked_mul(chroma_height)?;
    let mut out = Vec::with_capacity(luma_bytes.checked_add(chroma_bytes.checked_mul(2)?)?);

    // Luma is identical in both layouts.
    for row in 0..height {
        let start = stride.checked_mul(row)?;
        out.extend_from_slice(src.get(start..start.checked_add(width)?)?);
    }

    // Past the luma plane, including whatever padding followed its last row.
    let chroma = src.get(stride.checked_mul(height)?..)?;
    match format {
        PlanarFormat::I420 => {
            // U then V, each a half-width plane at half the luma stride.
            let uv_stride = stride.checked_div(2)?;
            for plane in 0..2_usize {
                let plane_start = plane.checked_mul(uv_stride.checked_mul(chroma_height)?)?;
                for row in 0..chroma_height {
                    let start = plane_start.checked_add(uv_stride.checked_mul(row)?)?;
                    out.extend_from_slice(chroma.get(start..start.checked_add(chroma_width)?)?);
                }
            }
        }
        PlanarFormat::Nv12 => {
            // One interleaved plane, `U V U V ...`, at the luma stride. The V samples are
            // gathered separately and appended, since I420 wants them as a whole plane.
            let mut v_plane = Vec::with_capacity(chroma_bytes);
            for row in 0..chroma_height {
                let start = stride.checked_mul(row)?;
                let pairs = chroma
                    .get(start..start.checked_add(chroma_width.checked_mul(2)?)?)?
                    .chunks_exact(2);
                for pair in pairs {
                    out.push(*pair.first()?);
                    v_plane.push(*pair.get(1)?);
                }
            }
            out.append(&mut v_plane);
        }
    }

    I420Frame::from_padded(
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
        out,
        width,
        chroma_width,
        0,
    )
}

/// A planar YUV 4:2:0 layout: already the subsampling every encoder wants.
///
/// Kept apart from [`SourceFormat`] because the difference is not cosmetic. Those layouts
/// need a colour conversion touching every pixel; these need at most a chroma
/// de-interleave, and I420 needs nothing at all. Sharing one enum would put the two on the
/// same footing and lose exactly the distinction that makes them worth asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanarFormat {
    /// Three separate planes: Y, then U, then V.
    I420,
    /// A luma plane, then one plane of interleaved U and V.
    Nv12,
}

impl PlanarFormat {
    /// Map a `libspa` video format onto a planar layout, or `None` if it is not one.
    #[must_use]
    pub const fn from_spa(format: libspa::param::video::VideoFormat) -> Option<Self> {
        use libspa::param::video::VideoFormat as F;
        match format {
            F::I420 => Some(Self::I420),
            F::NV12 => Some(Self::Nv12),
            _ => None,
        }
    }
}

/// Negotiated geometry, shared between the format callback and the frame callback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Encoding {
    /// Uncompressed pixels in a layout `SourceFormat` describes.
    Raw(SourceFormat),
    /// Planar YUV 4:2:0, which reaches the encoder without a colour conversion.
    Planar(PlanarFormat),
    /// Motion JPEG: each buffer is a complete JPEG image.
    ///
    /// What a UVC webcam offers at higher resolutions and frame rates -- often the *only*
    /// thing it offers there. `PipeWire` does not transcode, so a client that cannot decode
    /// JPEG simply receives nothing from such a camera.
    Mjpeg,
    #[default]
    Unsupported,
}

/// Why a conversion returned nothing, in terms of the path that was actually taken.
///
/// The conversions fail for unrelated reasons and only one of them is a short buffer. A
/// single message describing every failure as one sends a reader to check geometry that was
/// never in question -- for MJPEG the geometry is inside the JPEG, and a decode that failed
/// says nothing about how many bytes arrived.
/// How many unconvertible frames are logged in full before the stream falls back to
/// counting them. Three, because the diagnostic they carry is identical after the first
/// couple and the failure rate makes them a recurring warning otherwise.
const UNUSABLE_LOG_LIMIT: u32 = 3;

const fn unusable_reason(encoding: Encoding) -> &'static str {
    match encoding {
        Encoding::Mjpeg => "mjpeg_decode_failed",
        Encoding::Planar(_) | Encoding::Raw(_) => "buffer_short_for_geometry",
        Encoding::Unsupported => "format_not_supported",
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Negotiated {
    width: u32,
    height: u32,
    encoding: Encoding,
}

/// Why a captured buffer never became a frame.
#[derive(Debug, Clone, Copy)]
enum DropReason {
    /// Arrived sooner than the requested frame rate allows.
    RateLimited,
    /// The consumer had not taken the frames already queued.
    QueueFull,
    /// Could not be decoded, or did not match the negotiated geometry.
    Unusable,
    /// Arrived before the stream format was known, which happens once at stream start.
    NotNegotiated,
}

/// Rolling per-frame cost of decoding captured buffers.
///
/// Reported rather than assumed: this is the hottest path in the application, running on
/// every frame from every camera, and on a laptop it is battery while on a desktop it is
/// CPU taken from whatever else the machine is doing.
#[derive(Default)]
struct CaptureTiming {
    frames: u64,
    total: std::time::Duration,
    worst: std::time::Duration,
    bytes: u64,
    /// Smallest source buffer seen in the window, alongside `mean_source_kb`.
    ///
    /// The mean alone cannot tell a genuinely short buffer from an ordinary one: "0.5% of
    /// MJPEG buffers fail to decode, and they are short" is a claim about the tail of this
    /// distribution, not its centre. Tracking the minimum costs one comparison per frame.
    min_bytes: Option<u64>,
    /// Buffers the source handed us, whether or not they became frames.
    ///
    /// The difference between this and `frames` is the whole diagnostic. "Capture is
    /// running below the requested rate" has three quite different causes -- the camera
    /// delivering fewer, us dropping them to hold a rate, or the consumer being too slow to
    /// take them -- and they are indistinguishable from the far end. Counting each
    /// separately turns "it feels choppy" into a number that names the cause.
    offered: u64,
    /// Dropped to hold the requested frame rate.
    rate_limited: u64,
    /// Dropped because the consumer had not taken the previous ones.
    queue_full: u64,
    /// Dropped because the buffer could not be decoded or was malformed.
    unusable: u64,
    /// Dropped because the format was not negotiated yet.
    ///
    /// Separate from `unusable` because it is not a fault: `PipeWire` delivers the first
    /// buffer or two before `param_changed` reports the format, and there is nothing to do
    /// with those but drop them. Counted together they read as a decode failure on every
    /// stream start, which is what sent this measurement looking for a decoder bug.
    not_negotiated: u64,
}

impl CaptureTiming {
    /// Frames between reports. 300 is five seconds at 60fps -- often enough to notice a
    /// change, rare enough that the logging itself is not part of the cost.
    const REPORT_EVERY: u64 = 300;

    /// A buffer arrived from the source.
    const fn offered(&mut self) {
        self.offered = self.offered.saturating_add(1);
    }

    /// A buffer was dropped, and why.
    const fn dropped(&mut self, reason: DropReason) {
        match reason {
            DropReason::RateLimited => self.rate_limited = self.rate_limited.saturating_add(1),
            DropReason::QueueFull => self.queue_full = self.queue_full.saturating_add(1),
            DropReason::Unusable => self.unusable = self.unusable.saturating_add(1),
            DropReason::NotNegotiated => {
                self.not_negotiated = self.not_negotiated.saturating_add(1);
            }
        }
    }

    fn record(&mut self, elapsed: std::time::Duration, bytes: usize) {
        self.frames = self.frames.saturating_add(1);
        self.total = self.total.saturating_add(elapsed);
        self.worst = self.worst.max(elapsed);
        let bytes64 = u64::try_from(bytes).unwrap_or(0);
        self.bytes = self.bytes.saturating_add(bytes64);
        self.min_bytes = Some(self.min_bytes.map_or(bytes64, |m| m.min(bytes64)));

        if self.frames.is_multiple_of(Self::REPORT_EVERY) {
            let mean = self
                .total
                .checked_div(u32::try_from(self.frames).unwrap_or(u32::MAX));
            tracing::info!(
                frames = self.frames,
                mean_ms = mean.map_or(0.0, |d| d.as_secs_f64() * 1000.0),
                worst_ms = self.worst.as_secs_f64() * 1000.0,
                mean_source_kb = self
                    .bytes
                    .checked_div(self.frames)
                    .and_then(|b| b.checked_div(1024))
                    .unwrap_or(0),
                min_source_bytes = self.min_bytes.unwrap_or(0),
                offered = self.offered,
                rate_limited = self.rate_limited,
                queue_full = self.queue_full,
                unusable = self.unusable,
                not_negotiated = self.not_negotiated,
                "capture decode cost"
            );
            *self = Self::default();
        }
    }
}

/// A running `PipeWire` video capture.
pub struct PipewireCapturer {
    frame_rx: mpsc::Receiver<CapturedFrame>,
    stop_tx: mpsc::Sender<()>,
    negotiated: Arc<Mutex<Negotiated>>,
    /// Set when the stream enters its error state.
    ///
    /// A stream can negotiate a format and then fail on the buffers, which leaves it
    /// reporting a perfectly good size and delivering nothing at all. Without this a caller
    /// has no way to tell that from a camera that is merely slow to start, and waits out
    /// its whole timeout on a stream that will never produce a frame.
    failed: Arc<std::sync::atomic::AtomicBool>,
}

impl PipewireCapturer {
    /// Connect to `node_id` and start delivering frames.
    ///
    /// # Errors
    ///
    /// Returns [`PipewireError`] if `PipeWire` cannot be initialised or the daemon is
    /// unreachable. A node that exists but never produces frames is *not* an error here —
    /// it surfaces as no frames arriving, which the caller can time out on.
    pub fn start(node_id: u32) -> Result<Self, PipewireError> {
        Self::start_at(
            node_id,
            DEFAULT_CAPTURE_FPS,
            elementium_codec::EncodeTarget::software(),
        )
    }

    /// Connect to `node_id`, asking for `target_fps` frames per second in whichever format
    /// suits `target` best.
    ///
    /// The rate is requested, not enforced: a source may offer only a fixed rate, and one
    /// that offers more will be asked for this and may still deliver more. Frames arriving
    /// faster than this are dropped *before* being decoded -- decoding is the single
    /// largest cost on this path, and decoding a frame nothing will consume is the most
    /// expensive way to do nothing.
    ///
    /// `target` decides the format preference, and it genuinely changes the answer rather
    /// than nudging it. Encoding in software, MJPEG is the most expensive format the camera
    /// offers, because every frame costs a full JPEG decode on the CPU; encoding on a GPU
    /// that decodes JPEG itself, it is the cheapest, because the compressed bytes go
    /// straight to the hardware and the CPU never sees a pixel. Capture cannot make that
    /// choice without knowing where the frames are going.
    ///
    /// # Errors
    ///
    /// As [`PipewireCapturer::start`].
    pub fn start_at(
        node_id: u32,
        target_fps: u32,
        target: elementium_codec::EncodeTarget,
    ) -> Result<Self, PipewireError> {
        Self::start_with(
            node_id,
            CaptureRequest {
                target_fps,
                target,
                profile: SourceProfile::Camera,
            },
        )
    }

    /// Connect to a node with an explicit source profile.
    ///
    /// # Errors
    ///
    /// As [`PipewireCapturer::start`].
    pub fn start_with(node_id: u32, request: CaptureRequest) -> Result<Self, PipewireError> {
        let (frame_tx, frame_rx) = mpsc::sync_channel(FRAME_QUEUE_DEPTH);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let negotiated = Arc::new(Mutex::new(Negotiated::default()));
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let thread_negotiated = Arc::clone(&negotiated);
        let thread_failed = Arc::clone(&failed);
        std::thread::Builder::new()
            .name(format!("pw-capture-{node_id}"))
            .spawn(move || {
                run_stream(
                    node_id,
                    request,
                    &frame_tx,
                    stop_rx,
                    &thread_negotiated,
                    &thread_failed,
                    &ready_tx,
                );
            })
            .map_err(PipewireError::SpawnThread)?;

        // Surface setup failures to the caller instead of leaving a silent dead thread.
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                frame_rx,
                stop_tx,
                negotiated,
                failed,
            }),
            Ok(Err(reason)) => Err(PipewireError::StreamSetup { reason }),
            Err(e) => Err(PipewireError::StreamSetup {
                reason: format!("stream setup timed out: {e}"),
            }),
        }
    }

    /// The most recent frame, if one is waiting.
    #[must_use]
    pub fn try_recv(&self) -> Option<CapturedFrame> {
        self.frame_rx.try_recv().ok()
    }

    /// Whether the stream has given up.
    ///
    /// Distinct from "no frames yet": a camera can take a moment to start, and a stream
    /// that has errored never will.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Negotiated frame size, or `(0, 0)` before the format callback has run.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.negotiated
            .lock()
            .map_or((0, 0), |n| (n.width, n.height))
    }

    /// Stop the capture thread.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(());
    }
}

impl Drop for PipewireCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Body of the capture thread: build the stream, run the loop until stopped.
#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
/// Tell the caller why capture could not start, and say so in the log if even that fails.
///
/// The second failure is the one worth catching. If the caller has already given up and
/// dropped the receiver, the reason for the *first* failure disappears with it: the caller
/// sees a capture that never started and no explanation exists anywhere. That is a
/// two-line fix for a fault that would otherwise be unexplainable from the outside.
fn report_startup_failure(ready_tx: &std::sync::mpsc::Sender<Result<(), String>>, reason: String) {
    if ready_tx.send(Err(reason.clone())).is_err() {
        tracing::error!(
            reason = %reason,
            "capture failed to start, and the caller was no longer listening for the reason"
        );
    }
}

fn run_stream(
    node_id: u32,
    request: CaptureRequest,
    frame_tx: &mpsc::SyncSender<CapturedFrame>,
    stop_rx: mpsc::Receiver<()>,
    negotiated: &Arc<Mutex<Negotiated>>,
    failed: &Arc<std::sync::atomic::AtomicBool>,
    ready_tx: &mpsc::Sender<Result<(), String>>,
) {
    pipewire::init();

    let setup = || -> Result<_, String> {
        let mainloop = pipewire::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
        let context =
            pipewire::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
        let core = context.connect_rc(None).map_err(|e| e.to_string())?;
        let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
        Ok((mainloop, core, registry))
    };

    let (mainloop, core, registry) = match setup() {
        Ok(v) => v,
        Err(e) => {
            report_startup_failure(ready_tx, e);
            return;
        }
    };

    // Notice the source disappearing, which the stream itself will not tell us.
    //
    // Measured, because the obvious assumptions are both wrong: when a shared window is
    // closed, the stream does *not* error. It goes Streaming -> Paused -> Streaming and
    // then simply never delivers another frame -- which is also exactly what a healthy
    // screencast of a static window does, since a compositor emits on damage. So neither
    // the stream's error state nor a frame-stall timeout can tell "the window is gone"
    // from "nothing has changed on screen", and a timeout would end perfectly good shares
    // of a still document.
    //
    // What does change is the registry: the node is destroyed, and its id is recycled
    // (observed being reused for an unrelated Link object seconds later). That removal is
    // the signal, and it is unambiguous.
    let source_gone = Arc::clone(failed);
    let _registry_listener = registry
        .add_listener_local()
        .global_remove(move |id| {
            if id == node_id {
                tracing::warn!(
                    node_id,
                    "the captured PipeWire node was removed; the shared source is gone"
                );
                source_gone.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .register();

    let props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Video",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Communication",
    };

    let stream = match pipewire::stream::StreamRc::new(core, "elementium-capture", props) {
        Ok(s) => s,
        Err(e) => {
            report_startup_failure(ready_tx, e.to_string());
            return;
        }
    };

    if let Err(e) = attach_and_connect(&stream, node_id, request, negotiated, failed, frame_tx) {
        report_startup_failure(ready_tx, e);
        return;
    }
    // A dropped receiver here is not a fault: the caller may have given up while the
    // portal or the device was still negotiating, and a successful start it no longer wants
    // is torn down by the stop path rather than reported to nobody.
    let _ = ready_tx.send(Ok(()));
    run_until_stopped(&mainloop, stop_rx, node_id);
}

/// Attach the stream callbacks and connect to `node_id`.
///
/// Returns the listener, which must outlive the stream: dropping it silently stops every
/// callback, so the stream would connect and then deliver nothing.
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn attach_and_connect(
    stream: &pipewire::stream::StreamRc,
    node_id: u32,
    request: CaptureRequest,
    negotiated: &Arc<Mutex<Negotiated>>,
    failed: &Arc<std::sync::atomic::AtomicBool>,
    frame_tx: &mpsc::SyncSender<CapturedFrame>,
) -> Result<(), String> {
    let fmt_state = Arc::clone(negotiated);
    let frame_state = Arc::clone(negotiated);
    let tx = frame_tx.clone();
    // Owned by the process callback and reused every frame, so taking a snapshot of the
    // mapped buffer costs no allocation.
    let mut snapshot: Vec<u8> = Vec::new();
    // A dma-buf this process cannot read fails the same way on every frame; the warning is
    // worth exactly once per stream.
    let mut dma_warned = false;
    // How many conversion failures have been logged in full for this stream.
    //
    // These are ~0.5% of buffers on a camera that is otherwise working, which at 30fps is a
    // warning every few seconds, for a fault the user cannot act on and the periodic
    // `capture decode cost` line already counts. The first few carry the diagnostic that
    // matters -- sizes, SOI/EOI, the bytes at each end -- and the hundredth carries nothing
    // the third did not. So the detail is kept and the repetition is not.
    let mut unusable_logged: u32 = 0;
    // Per-stage cost, reported periodically. This runs on every captured frame on every
    // user's machine, so its cost is battery on a laptop and frames stolen from whatever
    // else the machine is doing. Guessing which stage dominates has been wrong before.
    let mut timing = CaptureTiming::default();
    // Shortest gap between frames we will decode. A source may deliver faster than asked --
    // it negotiates a rate from a range, and some ignore the request entirely -- and a
    // frame that arrives early is dropped here, before the decode. That ordering is the
    // whole point: decoding is a third of this path's cost, so decoding a frame nothing
    // will consume is the most expensive possible way to do nothing.
    // With a tolerance, and it matters more than it looks. A camera asked for 30fps
    // delivers frames about 33.3ms apart, but "about" is doing real work: jitter of a
    // fraction of a millisecond puts many of them just inside an exact 1/30s gap, each one
    // is dropped, and the next then falls outside -- so a source running at precisely the
    // requested rate is decimated to roughly half of it. Measured at 17-20fps from a 30fps
    // camera before this allowance.
    //
    // An eighth is comfortably more than any jitter observed and still less than the gap to
    // the next rate a camera would plausibly run at, so a genuinely faster source is still
    // limited.
    let nominal_gap = 1_000_000_000_u64
        .checked_div(u64::from(request.target_fps.max(1)))
        .unwrap_or(0);
    // How early a frame may arrive and still count for its slot.
    //
    // The fixed timeline removes the ratchet, but on its own it still drops a frame that
    // arrives a millisecond before its deadline -- and the next one is then a full interval
    // later, so that slot is lost outright. An eighth of the interval is comfortably more
    // than the jitter observed from a camera and far less than the gap to the next rate one
    // would plausibly run at, so a genuinely faster source is still limited.
    let early_tolerance = std::time::Duration::from_nanos(nominal_gap.checked_div(8).unwrap_or(0));
    let nominal_gap = std::time::Duration::from_nanos(nominal_gap);
    // When the next frame is *due*, on a timeline of its own rather than measured from the
    // last one accepted.
    //
    // Measuring from the last accepted frame ratchets: a frame that arrives late moves the
    // window late with it, so the following one -- arriving on the camera's own cadence --
    // is now early and is dropped, which moves the window again. A 30fps camera measured
    // this way delivered 28.5fps with 19 of 319 buffers dropped as rate-limited, none of
    // which the consumer was too slow for.
    //
    // Advancing a deadline by exactly the nominal gap keeps the reference fixed, so jitter
    // costs nothing as long as the source's average rate is right, while a genuinely faster
    // source is still limited to the target.
    let mut next_due: Option<std::time::Instant> = None;

    let listener = stream
        .add_local_listener::<()>()
        .state_changed({
            let failed = Arc::clone(failed);
            move |_, (), old, new| {
                tracing::info!(?old, ?new, "PipeWire capture stream state");
                if let pipewire::stream::StreamState::Error(reason) = &new {
                    // Warn, not error: one stream failing is a normal step in choosing a
                    // source. `PipeWire` lists several nodes for one camera, and the ones
                    // already in use answer "Device or resource busy" -- so a working
                    // session produces this on the way to a camera that starts. Whether it
                    // mattered is decided by the caller, which tries the next source and
                    // reports at error only once every source and the V4L2 fallback have
                    // been exhausted.
                    // The accepted buffer types are named alongside the failure, because
                    // the message the library gives for the commonest cause is
                    // `error alloc buffers: Invalid argument`, which does not hint at
                    // buffers at all, let alone at a negotiation mismatch. That exact line
                    // cost this project an afternoon: the compositor allocates DMA-BUF and
                    // the client was accepting only mapped memory, and nothing in the log
                    // said either half of that sentence.
                    let accepted = match request.profile {
                        SourceProfile::Camera => "MemPtr|MemFd",
                        SourceProfile::Screencast => "MemPtr|MemFd|DmaBuf",
                    };
                    tracing::warn!(
                        reason = %reason,
                        profile = ?request.profile,
                        accepted_buffer_types = accepted,
                        "PipeWire capture stream failed; trying the next source if there is one"
                    );
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        })
        .param_changed(move |_, (), id, pod| {
            // Only the fixated `Format` matters; the stream also reports Props, Buffers,
            // Meta and friends through this same callback.
            if id != libspa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = pod else { return };
            let mut info = libspa::param::video::VideoInfoRaw::default();
            if info.parse(pod).is_err() {
                tracing::warn!("PipeWire sent a format we could not parse");
                return;
            }
            let size = info.size();
            let subtype = negotiated_subtype(pod);
            let encoding = if subtype == Some(libspa::param::format::MediaSubtype::Mjpg.as_raw()) {
                Encoding::Mjpeg
            } else {
                // Planar first: I420 and NV12 also parse as raw layouts, and treating them
                // as such would route frames through an RGBA conversion they do not need.
                PlanarFormat::from_spa(info.format()).map_or_else(
                    || {
                        SourceFormat::from_spa(info.format())
                            .map_or(Encoding::Unsupported, Encoding::Raw)
                    },
                    Encoding::Planar,
                )
            };
            if encoding == Encoding::Unsupported {
                // Error, and unlike a busy node this one is ours. We advertise the formats
                // we accept in `format_params`, so settling on one we cannot decode means
                // what we offer and what we handle have diverged. The caller still
                // recovers -- no frame arrives, so it moves to the next source -- but the
                // camera it lands on is not the one the policy chose.
                tracing::error!(
                    format = ?info.format(),
                    subtype,
                    "PipeWire negotiated an encoding we cannot decode, so we asked for one \
                     we do not handle; every frame from this stream will be dropped"
                );
            }
            tracing::info!(
                width = size.width,
                height = size.height,
                ?encoding,
                "PipeWire capture format negotiated"
            );
            // Rounded down to even, and this is load-bearing rather than tidy.
            //
            // VP8 refuses odd dimensions outright -- "VP8 requires even dimensions, got
            // 1703x1406" -- and a window is whatever width the user dragged it to, so an
            // odd one is not an edge case here the way it is for a camera. Cropping the
            // last column or row costs a pixel nobody can see; failing to encode costs the
            // whole share, at the point where the picker has already been accepted and
            // everything looks like it is working.
            //
            // Done here, where the negotiated geometry is recorded, so that the size the
            // capture reports, the size its frames are converted at, and the size the
            // encoder is built for cannot disagree. A mismatch between those three reads
            // as a sheared picture rather than as a refusal.
            let even_width = size.width & !1;
            let even_height = size.height & !1;
            if (even_width, even_height) != (size.width, size.height) {
                tracing::info!(
                    from_width = size.width,
                    from_height = size.height,
                    width = even_width,
                    height = even_height,
                    "capture geometry cropped to even dimensions for the encoder"
                );
            }
            if let Ok(mut n) = fmt_state.lock() {
                n.width = even_width;
                n.height = even_height;
                n.encoding = encoding;
            }
        })
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Ok(n) = frame_state.lock().map(|g| *g) else {
                return;
            };
            timing.offered();
            if n.encoding == Encoding::Unsupported {
                timing.dropped(DropReason::NotNegotiated);
                return;
            }
            // Checked before anything is read out of the buffer, let alone decoded.
            let now = std::time::Instant::now();
            match next_due {
                Some(due) if now.checked_add(early_tolerance).is_some_and(|t| t < due) => {
                    timing.dropped(DropReason::RateLimited);
                    return;
                }
                Some(due) => {
                    // Advance by exactly one interval, so the cadence stays anchored. If the
                    // stream stalled long enough that the next slot is already in the past,
                    // resynchronise rather than emitting a burst to catch up -- the frames
                    // for those slots do not exist.
                    let stepped = due.checked_add(nominal_gap).unwrap_or(now);
                    next_due = Some(if stepped > now {
                        stepped
                    } else {
                        now.checked_add(nominal_gap).unwrap_or(now)
                    });
                }
                None => next_due = now.checked_add(nominal_gap),
            }

            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let stride = usize::try_from(data.chunk().stride()).unwrap_or(0);
            let mut size = usize::try_from(data.chunk().size()).unwrap_or(0);
            let chunk_offset = usize::try_from(data.chunk().offset()).unwrap_or(0);
            let max_size = usize::try_from(data.as_raw().maxsize).unwrap_or(0);
            let data_type = data.type_().as_raw();

            // Two ways a buffer can arrive, and only one of them is already mapped.
            //
            // A camera hands over MemPtr/MemFd, which `MAP_BUFFERS` mapped for us on the
            // way in. A screencast on this compositor hands over a DMA-BUF: a DRM PRIME
            // file descriptor that `MAP_BUFFERS` leaves alone, so `data()` is `None` and
            // the frame has to be mapped here. See `DmaBufMapping`.
            let dma_map;
            let mapped: &[u8] = if data.type_().as_raw() == libspa::sys::SPA_DATA_DmaBuf {
                // A DMA-BUF does not describe its own extent the way a mapped buffer does.
                // Measured on this compositor: `chunk.size` and `maxsize` are *both* 1 --
                // a "there is a frame here" marker, not a byte count, because the memory
                // belongs to the GPU and its size is implied by the format. Taking either
                // literally maps a single byte and every frame is then rejected as too
                // short for its geometry, which is exactly what happened before this ran.
                //
                // The extent that is real is the one the format implies: `stride` bytes
                // per row for the negotiated height. That is only true for a linear
                // layout, which is the only one offered (see `video_modifier_property`).
                if size <= 1 {
                    let rows = usize::try_from(n.height).unwrap_or(0);
                    size = stride.checked_mul(rows).unwrap_or(0).max(max_size);
                }
                match DmaBufMapping::map(data.fd(), chunk_offset, size) {
                    Ok(m) => {
                        dma_map = m;
                        dma_map.bytes()
                    }
                    Err(reason) => {
                        timing.dropped(DropReason::Unusable);
                        // Once per stream, not once per frame: this fails identically
                        // thirty times a second, and the first one says everything the
                        // three-hundredth would.
                        if !dma_warned {
                            dma_warned = true;
                            tracing::warn!(
                                %reason,
                                fd = data.fd(),
                                chunk_offset,
                                size,
                                stride,
                                "a screencast dma-buf could not be read on the CPU"
                            );
                        }
                        return;
                    }
                }
            } else {
                let Some(m) = data.data() else { return };
                m
            };

            // Snapshot the buffer before *decoding* it.
            //
            // `data.data()` is memory the camera's driver writes into. Decoding straight
            // out of it means reading, over several milliseconds, from a region another
            // writer may update in the meantime -- so rows decoded before an update come
            // from one frame and rows after it from the next. The result is an image cut
            // into horizontal bands seconds apart, which is the fault this was chasing:
            // reproduced in the app under load, never in a probe that did nothing else.
            //
            // Whether the driver *does* overwrite early is not something a client can
            // establish; reading a buffer another writer owns is a race whether or not it
            // has been observed to lose. The copy is a few hundred KB for MJPEG against a
            // multi-millisecond decode, so it costs nothing worth measuring.
            //
            // The planar layouts are the exception, and deliberately so. Their conversion
            // is itself a single sequential pass that reads each byte once and then owns
            // the result -- exactly what the snapshot does -- so taking a snapshot first
            // would copy a megabyte and a half in order to copy it again, for a race
            // window of precisely the same shape. They read the mapped buffer directly.
            let copy_len = if size > 0 {
                size.min(mapped.len())
            } else {
                mapped.len()
            };
            let source: &[u8] = if matches!(n.encoding, Encoding::Planar(_)) {
                mapped.get(..copy_len).unwrap_or(mapped)
            } else {
                snapshot.clear();
                snapshot.extend_from_slice(mapped.get(..copy_len).unwrap_or(mapped));
                &snapshot
            };
            let bytes: &[u8] = source;

            let width = usize::try_from(n.width).unwrap_or(0);
            let height = usize::try_from(n.height).unwrap_or(0);

            // Every frame carries the geometry it was actually decoded at, not the
            // geometry the stream negotiated, because a mismatch between the two is
            // exactly the fault this used to have: the buffer was labelled with the
            // negotiated size regardless of what came out of the decoder.
            // Handed on undecoded when the negotiated encoder decodes JPEG in hardware.
            // Decoding here would be the single largest cost on this path and would be
            // thrown away immediately: the GPU decodes it again, properly, from the
            // compressed bytes.
            if n.encoding == Encoding::Mjpeg && request.target.gpu_jpeg_decode {
                let frame = CapturedFrame::Mjpeg {
                    data: bytes.to_vec(),
                    width: n.width,
                    height: n.height,
                };
                timing.record(std::time::Duration::ZERO, copy_len);
                if tx.try_send(frame).is_err() {
                    timing.dropped(DropReason::QueueFull);
                }
                return;
            }

            let decode_started = std::time::Instant::now();
            let converted = match n.encoding {
                Encoding::Mjpeg => decode_mjpeg_to_i420(bytes),
                Encoding::Planar(format) => {
                    let stride = if stride == 0 { width } else { stride };
                    planar_to_i420(bytes, width, height, stride, format)
                }
                Encoding::Raw(format) => {
                    let stride = if stride == 0 {
                        width.saturating_mul(format.bytes_per_pixel())
                    } else {
                        stride
                    };
                    // Raw sources are the minority path (a camera offering uncompressed
                    // frames, and the screen-capture portal). They convert via RGBA
                    // because that is what the layout converters produce; the cost is
                    // paid only where MJPEG is not on offer.
                    to_rgba(bytes, width, height, stride, format).map(|rgba| {
                        elementium_codec::rgba_to_i420(n.width, n.height, rgba.as_slice())
                    })
                }
                Encoding::Unsupported => None,
            };

            timing.record(decode_started.elapsed(), copy_len);

            if let Some(frame) = converted {
                let (frame_width, frame_height) = (frame.width(), frame.height());
                if frame_width != n.width || frame_height != n.height {
                    // Once per occurrence rather than per frame: a camera that does this
                    // does it every frame, and the interesting fact is that it happens at
                    // all, not how often.
                    tracing::warn!(
                        negotiated_width = n.width,
                        negotiated_height = n.height,
                        frame_width,
                        frame_height,
                        "camera delivered a frame of a different size than negotiated"
                    );
                }
                // Newest frame dropped when full: blocking here would stall the PipeWire
                // graph, which is far worse than losing a frame.
                if tx.try_send(CapturedFrame::Planar(frame)).is_err() {
                    timing.dropped(DropReason::QueueFull);
                }
            } else {
                timing.dropped(DropReason::Unusable);
                // Distinguishes driver truncation from a race in our own read, for MJPEG
                // specifically: a buffer cut off mid-write by the driver still starts with a
                // valid SOI (the first bytes written) and simply stops before EOI. A buffer
                // we read while another writer was rewriting it would more likely show a
                // *corrupt* header -- we would have raced the same write that lays down the
                // marker -- not a clean start with a missing tail. Checked only on the
                // failure path, which is already rare enough (~0.5%) that this costs nothing
                // worth measuring; it does no work on the 99.5% that decode.
                let (soi_ok, eoi_ok, head_hex, tail_hex) = if n.encoding == Encoding::Mjpeg {
                    let soi = matches!(bytes, [0xFF, 0xD8, ..]);
                    let eoi = matches!(bytes, [.., 0xFF, 0xD9]);
                    // A handful of bytes at each end, not the whole buffer: enough to tell a
                    // zeroed allocation (never written) from live garbage (overwritten mid-read)
                    // from an ordinary JPEG that is simply short, without logging kilobytes on
                    // a path that already runs on every one of these rare failures.
                    let head = bytes.get(..bytes.len().min(8)).unwrap_or(bytes);
                    let tail_start = bytes.len().saturating_sub(8);
                    let tail = bytes.get(tail_start..).unwrap_or(bytes);
                    let hex = |b: &[u8]| {
                        b.iter().fold(String::new(), |mut acc, x| {
                            let _ = std::fmt::Write::write_fmt(&mut acc, format_args!("{x:02x}"));
                            acc
                        })
                    };
                    (Some(soi), Some(eoi), Some(hex(head)), Some(hex(tail)))
                } else {
                    (None, None, None, None)
                };
                unusable_logged = unusable_logged.saturating_add(1);
                if unusable_logged > UNUSABLE_LOG_LIMIT {
                    // Counted, not logged. `unusable` in the periodic report is the number
                    // that matters once the shape of the failure is known.
                    return;
                }
                if unusable_logged == UNUSABLE_LOG_LIMIT {
                    tracing::warn!(
                        limit = UNUSABLE_LOG_LIMIT,
                        "further unconvertible frames on this stream will be counted in \
                         `capture decode cost` rather than logged individually"
                    );
                }
                tracing::warn!(
                    len = bytes.len(),
                    // The buffer's own description of itself, because "too short" says
                    // nothing about which of the three lengths involved was wrong.
                    data_type, max_size, chunk_size = size, chunk_offset,
                    width, height, stride,
                    encoding = ?n.encoding,
                    reason = unusable_reason(n.encoding),
                    soi_ok, eoi_ok, head_hex, tail_hex,
                    "PipeWire frame could not be converted; frame dropped"
                );
            }
        })
        .register();

    let mut params = format_params(request);
    // Appended after the formats: the source reads both when the stream connects, and the
    // buffer constraint applies whichever format it settles on.
    params.push(buffers_param(request.profile));
    let mut param_refs: Vec<&libspa::pod::Pod> = params
        .iter_mut()
        .filter_map(|p| libspa::pod::Pod::from_bytes(p))
        .collect();
    if param_refs.is_empty() {
        // Connecting with no constraint lets the source pick anything at all, and there
        // would be no way to tell that had happened from the frames alone.
        return Err("could not build the video format parameters".to_owned());
    }

    stream
        .connect(
            libspa::utils::Direction::Input,
            Some(node_id),
            pipewire::stream::StreamFlags::AUTOCONNECT | pipewire::stream::StreamFlags::MAP_BUFFERS,
            &mut param_refs,
        )
        .map_err(|e| e.to_string())?;

    // Deliberately leaked: the listener must live as long as the stream, and the stream
    // outlives this function. Dropping it would leave a connected stream with no callbacks,
    // which looks exactly like a camera that produces nothing.
    std::mem::forget(listener);
    Ok(())
}

/// Run the main loop until a stop is requested.
fn run_until_stopped(
    mainloop: &pipewire::main_loop::MainLoopRc,
    stop_rx: mpsc::Receiver<()>,
    node_id: u32,
) {
    // Poll the stop channel from inside the loop rather than blocking on it: the PipeWire
    // main loop owns this thread once running.
    let quit_loop = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_rx.try_recv().is_ok() {
            quit_loop.quit();
        }
    });
    let _ = timer.update_timer(
        Some(std::time::Duration::from_millis(100)),
        Some(std::time::Duration::from_millis(100)),
    );

    tracing::info!(node_id, "PipeWire capture loop running");
    mainloop.run();
    tracing::info!(node_id, "PipeWire capture loop stopped");
}

/// The `libspa` video format a capture format is offered as, if it is a raw layout.
///
/// `None` for MJPEG, which is not a raw layout at all — it is a different media subtype,
/// and offering it means building a different kind of parameter.
const fn spa_video_format(
    format: elementium_codec::CaptureFormat,
) -> Option<libspa::param::video::VideoFormat> {
    use elementium_codec::CaptureFormat as C;
    use libspa::param::video::VideoFormat as F;
    match format {
        C::Mjpeg => None,
        C::I420 => Some(F::I420),
        C::Nv12 => Some(F::NV12),
        C::Yuy2 => Some(F::YUY2),
        C::Rgb => Some(F::RGB),
        C::Bgr => Some(F::BGR),
        C::Rgbx => Some(F::RGBx),
        C::Bgrx => Some(F::BGRx),
    }
}

/// Every format we will accept, ordered for the encoder the frames are headed to.
///
/// Two parameters, not one per format. The raw layouts go in a single parameter as an
/// enumerated choice with our preferred one first, and MJPEG goes in its own because it is
/// a different media subtype; which of the two comes first is what the policy decided.
///
/// One parameter per format was tried against a real camera and is wrong. It expresses the
/// preference exactly — `PipeWire` takes the first parameter the source can satisfy — but
/// "can satisfy" is judged against what the node advertises rather than what the device
/// will do, and an OBSBOT Tiny 2 advertising a layout its driver then refuses failed the
/// whole connection with `error set output format: -22 (Invalid argument)`. No frames at
/// all, from a camera that works. Inside one parameter the source negotiates the
/// intersection itself and settles on something that exists.
///
/// The proper fix is to read the node's own `EnumFormat` first and rank what it actually
/// offers. That is worth doing and is not this.
fn format_params(request: CaptureRequest) -> Vec<Vec<u8>> {
    let preference = elementium_codec::capture_format::preference(request.target);
    let raw: Vec<libspa::param::video::VideoFormat> = preference
        .iter()
        .copied()
        .filter_map(spa_video_format)
        .collect();
    // Where MJPEG sits relative to every raw layout is the whole decision: first when the
    // GPU decodes it, last when the CPU has to.
    let mjpeg_first = preference
        .first()
        .is_some_and(|f| *f == elementium_codec::CaptureFormat::Mjpeg);

    let mut params = Vec::with_capacity(2);
    if mjpeg_first {
        params.push(mjpeg_format_param(request));
    }
    if !raw.is_empty() {
        params.push(raw_format_param(request, &raw));
    }
    if !mjpeg_first {
        params.push(mjpeg_format_param(request));
    }
    params
}

/// The buffer types we can actually read.
///
/// Without this the source picks, and a source that picks DMA-BUF hands back a buffer whose
/// memory is a GPU handle rather than something `MAP_BUFFERS` can map -- which the client
/// then rejects with `error use input buffers: -22 (Invalid argument)` and delivers nothing
/// at all. The camera works, the format negotiates, and every frame is silently absent.
///
/// So the two types this capture path can read are declared: a plain pointer, and a memfd
/// the client maps. A source that can only offer DMA-BUF will now fail to negotiate rather
/// than agreeing and then failing, which is the better of the two failures because it says
/// so before a call starts.
///
/// A screencast is the exception, and has to be: measured against a real node, this
/// compositor allocates DMA-BUF whatever else is offered, and a client that declines it
/// gets `error alloc buffers: Invalid argument` and no frames at all (research R10). So
/// `DmaBuf` is added to the mask for that profile only, and `read_dma_buf` below does the
/// import by hand. The camera keeps the narrower mask: it has a working mappable path, and
/// widening it would let a camera pick a route that exists for a different reason.
fn buffers_param(profile: SourceProfile) -> Vec<u8> {
    use libspa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use libspa::utils::{Choice, ChoiceEnum, ChoiceFlags};

    const MAPPABLE: i32 = (1 << libspa::sys::SPA_DATA_MemPtr) | (1 << libspa::sys::SPA_DATA_MemFd);
    const WITH_DMA_BUF: i32 = MAPPABLE | (1 << libspa::sys::SPA_DATA_DmaBuf);

    let accepted = match profile {
        SourceProfile::Camera => MAPPABLE,
        SourceProfile::Screencast => WITH_DMA_BUF,
    };

    let obj = libspa::pod::Object {
        type_: libspa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: libspa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: libspa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            // A flags choice rather than a single value: this is a mask of what we accept,
            // and the source picks one from it.
            value: Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: accepted,
                    flags: vec![accepted],
                },
            ))),
        }],
    };
    serialize(obj)
}

/// A `VideoFormat` property offering several layouts, the first being preferred.
///
/// Built by hand rather than with `property!`, which takes its alternatives as literals and
/// so cannot express a list decided at run time — which is the point here, since the order
/// comes from whichever encoder was negotiated.
fn format_choice(formats: &[libspa::param::video::VideoFormat]) -> libspa::pod::Property {
    use libspa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use libspa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id};

    let default = formats
        .first()
        .copied()
        .unwrap_or(libspa::param::video::VideoFormat::YUY2);
    Property {
        key: libspa::param::format::FormatProperties::VideoFormat.as_raw(),
        flags: PropertyFlags::empty(),
        value: Value::Choice(ChoiceValue::Id(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Enum {
                default: Id(default.as_raw()),
                alternatives: formats.iter().map(|f| Id(f.as_raw())).collect(),
            },
        ))),
    }
}

/// One captured DMA-BUF, mapped into this process for the length of a single callback.
///
/// A DMA-BUF is a file descriptor naming memory another device owns, not memory this
/// process has. Reading it means three steps, all of which have to happen and in this
/// order: map it, tell the exporting driver a CPU read is starting so it can flush or
/// invalidate whatever caches it needs to, and tell it the read has finished so it can
/// take ownership back. Skipping the sync ioctls is the failure that produces a picture
/// which is *mostly* right -- stale cache lines show up as patches of a previous frame,
/// intermittently, on some GPUs and not others -- which is far worse to diagnose than not
/// working at all. The `Drop` impl is what guarantees the closing half runs even on the
/// early returns above it.
///
/// Only sound for a linear layout, which is why `video_modifier_property` offers exactly
/// `MOD_LINEAR` and `MOD_INVALID` and no vendor tiling: those bytes are read as rows, and
/// a tiled buffer's bytes are not in row order. A tiled buffer would need a GPU-side
/// import (EGL/vaapi) rather than a CPU mapping, and would come out as visible noise here.
struct DmaBufMapping {
    ptr: *mut libc::c_void,
    /// The whole mapping, from offset 0 -- `mmap`'s own offset must be page-aligned and the
    /// chunk's need not be, so the chunk offset is applied when slicing instead.
    len: usize,
    fd: std::os::fd::RawFd,
    offset: usize,
    size: usize,
}

/// `DMA_BUF_IOCTL_SYNC`, i.e. `_IOW('b', 0, struct dma_buf_sync)`, from `linux/dma-buf.h`.
/// Spelled out rather than computed because the `_IOW` macro is C and the value is stable
/// ABI: `0x4000_0000 | (8 << 16) | (0x62 << 8)`.
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

/// The `struct dma_buf_sync` the ioctl above takes.
#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

impl DmaBufMapping {
    /// Map a DMA-BUF fd for reading, or return `None` if it cannot be read on the CPU.
    ///
    /// `None` rather than an error because the caller's only useful response is to drop the
    /// frame and count it: a buffer this process cannot map is not a fault it can fix
    /// mid-stream, and one that logged per frame would log thirty times a second.
    fn map(fd: std::os::fd::RawFd, offset: usize, size: usize) -> Result<Self, String> {
        if fd < 0 {
            return Err("the buffer carried no dma-buf descriptor".to_owned());
        }
        if size == 0 {
            return Err("the chunk reported a size of zero".to_owned());
        }
        let Some(len) = offset.checked_add(size) else {
            return Err(format!("chunk offset {offset} plus size {size} overflows"));
        };

        // SAFETY: `fd` is a DMA-BUF descriptor owned by the buffer PipeWire just handed us
        // and valid for this callback; a null address asks the kernel to choose one; and a
        // failed mapping is reported as MAP_FAILED rather than by trapping.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if std::ptr::eq(ptr, libc::MAP_FAILED) {
            let err = std::io::Error::last_os_error();
            return Err(format!("mmap of {len} bytes from the dma-buf fd failed: {err}"));
        }

        let mapping = Self { ptr, len, fd, offset, size };
        mapping.sync(DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
        Ok(mapping)
    }

    /// Announce the start or end of a CPU read to the exporting driver.
    ///
    /// A failure is deliberately not fatal: the ioctl is unsupported on some exporters, and
    /// on those the mapping is coherent without it. Refusing the frame here would turn a
    /// working configuration into a black screen for the sake of a call that had nothing to
    /// do.
    fn sync(&self, flags: u64) {
        let arg = DmaBufSync { flags };
        // SAFETY: `self.fd` is the descriptor this mapping was made from, and the ioctl
        // reads one `dma_buf_sync` through the pointer, which is what is passed.
        let rc = unsafe { libc::ioctl(self.fd, DMA_BUF_IOCTL_SYNC, std::ptr::from_ref(&arg)) };
        if rc != 0 {
            tracing::trace!(flags, "dma-buf sync ioctl was refused; reading the mapping anyway");
        }
    }

    /// The frame's bytes: the chunk's own window into the mapping, not the whole thing.
    const fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping covers `offset + size` bytes (checked in `map`), is read-only
        // and lives until this value is dropped, so the slice is in bounds and outlived by
        // its backing memory.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>().add(self.offset), self.size) }
    }
}

impl Drop for DmaBufMapping {
    fn drop(&mut self) {
        self.sync(DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
        // SAFETY: `ptr`/`len` are exactly what `mmap` returned and was asked for, and no
        // slice handed out by `bytes` can outlive `self`.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// `DRM_FORMAT_MOD_LINEAR`: the DRM modifier for a plain row-major buffer with no tiling or
/// compression. One of the two non-hardware-specific modifiers a compositor can offer.
const DRM_FORMAT_MOD_LINEAR: i64 = 0;

/// `DRM_FORMAT_MOD_INVALID` (`0x00FF_FFFF_FFFF_FFFF`, i.e. `(1 << 56) - 1`): the DRM
/// convention for "no explicit modifier" rather than a real tiling/compression layout. In
/// `PipeWire`'s screencast usage it is the marker for a buffer with an *implicit* layout --
/// which in practice means a plain mapped buffer (shm or a `MemPtr`/`MemFd` allocation)
/// rather than a DMA-BUF carrying a GPU-specific tiling scheme. Offering it is how a client
/// says "I can take whatever layout you'd use for a non-GPU buffer" without claiming to
/// understand any vendor's real modifier.
const DRM_FORMAT_MOD_INVALID: i64 = 0x00FF_FFFF_FFFF_FFFF;

/// A `VideoModifier` property offering only the non-hardware DRM modifiers, for screencasts.
///
/// Measured against a real node (`pw-cli enum-params <node> EnumFormat` on a window shared
/// from niri via `xdg-desktop-portal-gnome`): the node's `EnumFormat` carries a `modifier`
/// property flagged `SPA_POD_PROP_FLAG_MANDATORY`. That flag means a client whose own
/// `EnumFormat` omits `modifier` cannot match the format at all -- not "gets a default", but
/// outright fails negotiation with `no more input formats`. A camera never does this, which
/// is why this property is confined to the screencast profile: adding it unconditionally
/// would risk changing what a working camera negotiates for no reason.
///
/// `MOD_INVALID` and `MOD_LINEAR` are exactly the two modifiers on that node's list that
/// name a plain buffer rather than a GPU-specific tiling layout, so they are the only two
/// offered here. This client's buffer path (`buffers_param`) only accepts `MemPtr`/`MemFd`,
/// which cannot map a real DMA-BUF tiling layout regardless of what the format negotiates --
/// so offering hardware modifiers would be worse than useless, it would let the stream
/// fixate on a layout this client then cannot read.
///
/// The `DONT_FIXATE` flag is set because that is what the protocol expects on a modifier
/// choice: `libspa`'s `PropertyFlags::DONT_FIXATE` is gated behind a Cargo feature this
/// workspace does not enable (`v0_3_33`), but the underlying bit is a plain `u32` constant
/// from `libspa::sys` that exists unconditionally, so it is set via `from_bits_retain`
/// rather than the named flag.
fn video_modifier_property() -> libspa::pod::Property {
    use libspa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use libspa::utils::{Choice, ChoiceEnum, ChoiceFlags};

    Property {
        key: libspa::param::format::FormatProperties::VideoModifier.as_raw(),
        flags: PropertyFlags::from_bits_retain(libspa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE),
        value: Value::Choice(ChoiceValue::Long(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Enum {
                default: DRM_FORMAT_MOD_INVALID,
                alternatives: vec![DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR],
            },
        ))),
    }
}

/// Ask for any of these raw pixel layouts, preferring the first.
fn raw_format_param(
    request: CaptureRequest,
    formats: &[libspa::param::video::VideoFormat],
) -> Vec<u8> {
    use libspa::pod::{object, property};
    let target_fps = request.target_fps;
    let max_size = request.profile.max_size();
    let min_framerate = request.profile.min_framerate();
    let mut obj = object! {
        libspa::utils::SpaTypes::ObjectParamFormat,
        libspa::param::ParamType::EnumFormat,
        property!(
            libspa::param::format::FormatProperties::MediaType,
            Id,
            libspa::param::format::MediaType::Video
        ),
        property!(
            libspa::param::format::FormatProperties::MediaSubtype,
            Id,
            libspa::param::format::MediaSubtype::Raw
        ),
        // A choice rather than a single value: the source picks from these, which is what
        // lets it settle on one the device will actually accept. The first is the default
        // and is taken when the device can manage it.
        format_choice(formats),
        // Size and framerate are not optional in practice: without them the source
        // fixates a format whose pixel layout comes back as `Unknown`, and every frame is
        // then dropped for want of a conversion. Offered as wide ranges so the source
        // picks whatever it natively supports.
        //
        // They also do the work of ruling out formats the link cannot carry. An
        // uncompressed 720p60 stream is 110 MB/s and will not fit down USB 2.0; a camera
        // advertises each format only at the rates it can sustain, so an infeasible one
        // simply fails to match here rather than needing to be predicted.
        property!(
            libspa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            libspa::utils::Rectangle { width: 1280, height: 720 },
            libspa::utils::Rectangle { width: 160, height: 120 },
            max_size
        ),
        property!(
            libspa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            libspa::utils::Fraction { num: target_fps, denom: 1 },
            min_framerate,
            libspa::utils::Fraction { num: target_fps, denom: 1 }
        ),
    };
    // Screencast-only: see `video_modifier_property` for why a camera must never get this
    // and a screencast node can require it to negotiate at all.
    if request.profile == SourceProfile::Screencast {
        obj.properties.push(video_modifier_property());
    }
    serialize(obj)
}

/// Ask for MJPEG, which is a media subtype rather than a pixel layout.
///
/// No `VideoFormat` property: there is no pixel layout to state, and including one makes
/// the parameter fail to match a camera that would otherwise have satisfied it.
fn mjpeg_format_param(request: CaptureRequest) -> Vec<u8> {
    use libspa::pod::{object, property};
    let target_fps = request.target_fps;
    let max_size = request.profile.max_size();
    let min_framerate = request.profile.min_framerate();
    let obj = object! {
        libspa::utils::SpaTypes::ObjectParamFormat,
        libspa::param::ParamType::EnumFormat,
        property!(
            libspa::param::format::FormatProperties::MediaType,
            Id,
            libspa::param::format::MediaType::Video
        ),
        property!(
            libspa::param::format::FormatProperties::MediaSubtype,
            Id,
            libspa::param::format::MediaSubtype::Mjpg
        ),
        property!(
            libspa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            libspa::utils::Rectangle { width: 1280, height: 720 },
            libspa::utils::Rectangle { width: 160, height: 120 },
            max_size
        ),
        property!(
            libspa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            libspa::utils::Fraction { num: target_fps, denom: 1 },
            min_framerate,
            libspa::utils::Fraction { num: target_fps, denom: 1 }
        ),
    };
    serialize(obj)
}

/// Serialise a format object into the bytes `PipeWire` takes, empty if it cannot be built.
fn serialize(obj: libspa::pod::Object) -> Vec<u8> {
    libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &libspa::pod::Value::Object(obj),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::panic
)]
mod tests {
    use super::{PlanarFormat, SourceFormat, planar_to_i420, to_rgba};

    /// Build a padded planar buffer whose every sample is identifiable, so a plane landing
    /// in the wrong place is visible rather than merely plausible.
    ///
    /// Luma counts up from 0, U is 200-something and V is 100-something. Swapping the
    /// chroma planes -- the classic fault here, and one that survives every size check --
    /// changes the picture's colour and nothing else, so the values have to distinguish
    /// them.
    fn planar_source(width: usize, height: usize, stride: usize, format: PlanarFormat) -> Vec<u8> {
        let mut buffer = vec![0_u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                buffer[y * stride + x] = u8::try_from((y * width + x) % 100).unwrap_or(0);
            }
        }
        let (cw, ch) = (width / 2, height / 2);
        match format {
            PlanarFormat::I420 => {
                let uv_stride = stride / 2;
                let mut u = vec![0_u8; uv_stride * ch];
                let mut v = vec![0_u8; uv_stride * ch];
                for y in 0..ch {
                    for x in 0..cw {
                        u[y * uv_stride + x] = u8::try_from(200 + (y * cw + x) % 50).unwrap_or(0);
                        v[y * uv_stride + x] = u8::try_from(100 + (y * cw + x) % 50).unwrap_or(0);
                    }
                }
                buffer.extend_from_slice(&u);
                buffer.extend_from_slice(&v);
            }
            PlanarFormat::Nv12 => {
                let mut uv = vec![0_u8; stride * ch];
                for y in 0..ch {
                    for x in 0..cw {
                        uv[y * stride + x * 2] = u8::try_from(200 + (y * cw + x) % 50).unwrap_or(0);
                        uv[y * stride + x * 2 + 1] =
                            u8::try_from(100 + (y * cw + x) % 50).unwrap_or(0);
                    }
                }
                buffer.extend_from_slice(&uv);
            }
        }
        buffer
    }

    /// I420 from the camera is already what the encoder reads, so the only work is getting
    /// it out of the driver's buffer -- no colour conversion, and one copy.
    ///
    /// The source is padded (stride 16 for 8 pixels) and the result is not, which is the
    /// case a converter assuming a tight source gets wrong: it reads across row boundaries
    /// and shears the picture progressively down the frame.
    #[test]
    fn i420_arrives_without_conversion() {
        let (w, h, stride) = (8_usize, 4_usize, 16_usize);
        let src = planar_source(w, h, stride, PlanarFormat::I420);
        let frame = planar_to_i420(&src, w, h, stride, PlanarFormat::I420).expect("converts");

        assert_eq!((frame.width(), frame.height()), (8, 4));
        assert_eq!(frame.y_stride(), w, "the output is tightly packed");
        assert_eq!(frame.uv_stride(), w / 2);
        assert_eq!(frame.y()[w], 8, "second luma row misplaced");
        assert_eq!(frame.u()[0], 200);
        assert_eq!(frame.v()[0], 100, "chroma planes are the wrong way round");
    }

    /// NV12 needs its interleaved chroma split, and nothing else: the luma plane is
    /// identical in both layouts and must not be touched.
    #[test]
    fn nv12_chroma_is_separated_and_luma_is_left_alone() {
        let (w, h, stride) = (8_usize, 4_usize, 16_usize);
        let src = planar_source(w, h, stride, PlanarFormat::Nv12);
        let frame = planar_to_i420(&src, w, h, stride, PlanarFormat::Nv12).expect("converts");

        assert_eq!(frame.y()[..w], src[..w], "luma must pass through untouched");
        assert_eq!(frame.y()[w], 8, "second luma row misplaced");
        for x in 0..w / 2 {
            assert_eq!(frame.u()[x], u8::try_from(200 + x).unwrap_or(0));
            assert_eq!(frame.v()[x], u8::try_from(100 + x).unwrap_or(0));
        }
    }

    /// A buffer too small for its claimed geometry must be refused. Accepting it would
    /// produce a frame whose lower rows are whatever happened to be in memory.
    #[test]
    fn a_short_planar_buffer_is_refused() {
        let (w, h, stride) = (8_usize, 4_usize, 16_usize);
        for format in [PlanarFormat::I420, PlanarFormat::Nv12] {
            let src = planar_source(w, h, stride, format);
            // A whole row short, so real samples are missing rather than only the padding
            // that follows the last one -- which a driver is free not to send.
            let truncated = &src[..src.len() - stride];
            assert!(
                planar_to_i420(truncated, w, h, stride, format).is_none(),
                "{format:?}: a short buffer produced a frame"
            );
        }
    }

    /// The layouts are distinguished by what they mean, not only by their names: reading
    /// NV12 as I420 finds interleaved chroma where a whole U plane is expected, which
    /// tints the picture and passes every size check.
    #[test]
    fn the_two_planar_layouts_are_not_interchangeable() {
        let (w, h, stride) = (8_usize, 4_usize, 16_usize);
        let src = planar_source(w, h, stride, PlanarFormat::Nv12);
        let correct = planar_to_i420(&src, w, h, stride, PlanarFormat::Nv12).expect("converts");
        let misread = planar_to_i420(&src, w, h, stride, PlanarFormat::I420).expect("converts");
        assert_ne!(
            correct.u(),
            misread.u(),
            "the layouts must not be conflated"
        );
    }

    #[test]
    fn bytes_per_pixel_matches_the_layout() {
        assert_eq!(SourceFormat::Rgbx.bytes_per_pixel(), 4);
        assert_eq!(SourceFormat::Bgrx.bytes_per_pixel(), 4);
        assert_eq!(SourceFormat::Rgb.bytes_per_pixel(), 3);
        assert_eq!(SourceFormat::Bgr.bytes_per_pixel(), 3);
    }

    #[test]
    fn converts_bgrx_to_rgba_swapping_the_channels() {
        // One pixel: B=1 G=2 R=3 X=4
        let out = to_rgba(&[1, 2, 3, 4], 1, 1, 4, SourceFormat::Bgrx).expect("converts");
        assert_eq!(out, vec![3, 2, 1, 255]);
    }

    #[test]
    fn converts_rgb_to_rgba_adding_opaque_alpha() {
        let out = to_rgba(&[9, 8, 7], 1, 1, 3, SourceFormat::Rgb).expect("converts");
        assert_eq!(out, vec![9, 8, 7, 255]);
    }

    /// Row padding is the failure that looks like a working camera: ignoring the stride
    /// shears the image progressively down the frame rather than producing obvious garbage.
    #[test]
    fn honours_a_stride_larger_than_the_row() {
        // 2x2 RGB with 2 bytes of padding per row.
        let src = vec![
            1, 1, 1, 2, 2, 2, 0, 0, // row 0 + padding
            3, 3, 3, 4, 4, 4, 0, 0, // row 1 + padding
        ];
        let out = to_rgba(&src, 2, 2, 8, SourceFormat::Rgb).expect("converts");
        assert_eq!(
            out,
            vec![1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255, 4, 4, 4, 255],
            "padding must be skipped, not folded into the image"
        );
    }

    #[test]
    fn output_is_tightly_packed_rgba() {
        let src = vec![0u8; 16 * 8 * 4];
        let out = to_rgba(&src, 16, 8, 16 * 4, SourceFormat::Bgrx).expect("converts");
        assert_eq!(out.len(), 16 * 8 * 4);
    }

    /// A short buffer must be refused rather than converted into a partly-garbage frame:
    /// half an image with stale memory after it looks like a decoder bug, not a truncated
    /// read.
    #[test]
    fn refuses_a_buffer_too_small_for_the_geometry() {
        let src = vec![0u8; 10];
        assert!(to_rgba(&src, 4, 4, 16, SourceFormat::Bgrx).is_none());
    }

    #[test]
    fn refuses_a_stride_narrower_than_a_row() {
        let src = vec![0u8; 64];
        assert!(to_rgba(&src, 4, 4, 8, SourceFormat::Bgrx).is_none());
    }

    /// The last row is exactly `row_bytes` long, not a full stride: requiring a whole
    /// trailing stride would reject valid buffers from sources that do not pad the end.
    #[test]
    fn accepts_a_buffer_whose_last_row_is_not_padded() {
        let width = 3;
        let height = 2;
        let stride = 16;
        let src = vec![7u8; stride * (height - 1) + width * 4];
        assert!(to_rgba(&src, width, height, stride, SourceFormat::Bgrx).is_some());
    }

    /// A JPEG the decoder cannot read must be refused, not turned into a partial image:
    /// cameras do occasionally emit a truncated frame, and half an image followed by stale
    /// pixels reads as a decoder bug rather than a dropped frame.
    #[test]
    fn refuses_a_malformed_mjpeg_frame() {
        assert!(super::decode_mjpeg_to_i420(&[0xFF, 0xD8, 0x00, 0x01]).is_none());
        assert!(super::decode_mjpeg_to_i420(&[]).is_none());
    }

    /// Encode a solid-colour JPEG, at a chosen chroma subsampling, to decode back.
    fn jpeg_fixture(
        width: u16,
        height: u16,
        rgb: [u8; 3],
        sampling: jpeg_encoder::SamplingFactor,
    ) -> Vec<u8> {
        let pixels: Vec<u8> = rgb
            .iter()
            .copied()
            .cycle()
            .take(usize::from(width) * usize::from(height) * 3)
            .collect();
        let mut out = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut out, 90);
        encoder.set_sampling_factor(sampling);
        encoder
            .encode(&pixels, width, height, jpeg_encoder::ColorType::Rgb)
            .expect("encode fixture");
        out
    }

    /// A decoded frame's planes must match the geometry it reports.
    ///
    /// This is the invariant whose violation shears a picture into sliding horizontal
    /// bands: everything downstream reads the planes using the frame's own width, so a
    /// frame whose planes disagree with it is read at the wrong stride. It was previously
    /// left as a documented gap because there was no way to make a JPEG here; there is
    /// now.
    #[test]
    fn a_decoded_frame_is_self_consistent() {
        let frame = super::decode_mjpeg_to_i420(&jpeg_fixture(
            64,
            32,
            [200, 100, 50],
            jpeg_encoder::SamplingFactor::F_2_2,
        ))
        .expect("decodes");

        assert_eq!((frame.width(), frame.height()), (64, 32));
        assert_eq!(frame.y().len(), 64 * 32, "luma plane matches the geometry");
        assert_eq!(frame.u().len(), 32 * 16, "chroma is half in each axis");
        assert_eq!(frame.v().len(), 32 * 16);
    }

    /// The decode must produce the colour that went in, not a channel-swapped one.
    ///
    /// A YUV plane order or channel swap is invisible in the sizes and obvious on screen,
    /// so the sizes alone are not enough of a check.
    #[test]
    fn a_decoded_frame_keeps_its_colour() {
        let frame = super::decode_mjpeg_to_i420(&jpeg_fixture(
            64,
            32,
            [200, 60, 60],
            jpeg_encoder::SamplingFactor::F_2_2,
        ))
        .expect("decodes");
        let rgba = elementium_codec::i420_to_rgba(&frame);

        // JPEG is lossy and the conversion is approximate, so this asserts the character
        // of the colour -- strongly red -- rather than exact values.
        let (r, g, b) = (rgba.data[0], rgba.data[1], rgba.data[2]);
        assert!(
            r > 150 && g < 110 && b < 110,
            "expected a red pixel, got r={r} g={g} b={b}"
        );
    }

    /// The chroma subsampling belongs to the JPEG, not to us.
    ///
    /// UVC webcams commonly emit 4:2:2 MJPEG and some emit 4:4:4, while I420 is 4:2:0.
    /// Reading one as another puts chroma where luma is expected, which decoded a solid
    /// red image to a murky green until this was handled -- a fault that is invisible in
    /// the plane sizes and obvious on screen.
    #[test]
    fn every_chroma_subsampling_decodes_to_i420() {
        for (name, sampling) in [
            ("4:4:4", jpeg_encoder::SamplingFactor::F_1_1),
            ("4:2:2", jpeg_encoder::SamplingFactor::F_2_1),
            ("4:2:0", jpeg_encoder::SamplingFactor::F_2_2),
        ] {
            let frame = super::decode_mjpeg_to_i420(&jpeg_fixture(64, 32, [200, 60, 60], sampling))
                .unwrap_or_else(|| panic!("{name} must decode"));

            assert_eq!(frame.y().len(), 64 * 32, "{name}: luma plane");
            assert_eq!(frame.u().len(), 32 * 16, "{name}: chroma must end up 4:2:0");
            assert_eq!(frame.v().len(), 32 * 16, "{name}: chroma must end up 4:2:0");

            let rgba = elementium_codec::i420_to_rgba(&frame);
            let (r, g, b) = (rgba.data[0], rgba.data[1], rgba.data[2]);
            assert!(
                r > 150 && g < 110 && b < 110,
                "{name}: expected red, got r={r} g={g} b={b}"
            );
        }
    }

    #[test]
    fn yuy2_shares_chroma_between_a_pixel_pair() {
        // One YUY2 quad is two pixels; a mid-grey Y with neutral chroma must give two equal
        // near-grey RGBA pixels rather than one pixel and padding.
        let out = to_rgba(&[128, 128, 128, 128], 2, 1, 4, SourceFormat::Yuy2).expect("converts");
        assert_eq!(out.len(), 2 * 4, "two pixels out of one quad");
        assert_eq!(out[0..4], out[4..8], "a pair shares chroma");
        assert_eq!(out[3], 255);
    }

    #[test]
    fn yuy2_reports_two_bytes_per_pixel_for_stride_maths() {
        assert_eq!(SourceFormat::Yuy2.bytes_per_pixel(), 2);
    }
}
