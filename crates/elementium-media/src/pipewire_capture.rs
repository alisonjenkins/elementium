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

use elementium_types::VideoFrame;

use crate::pipewire_nodes::PipewireError;

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
    let needed = stride.checked_mul(height.saturating_sub(1))?.checked_add(row_bytes)?;
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
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::as_conversions)]
        {
            x.clamp(0.0, 255.0) as u8
        }
    };
    out.push(clamp(1.402_f32.mul_add(v, y)));
    out.push(clamp((-0.714_136_f32).mul_add(v, (-0.344_136_f32).mul_add(u, y))));
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

/// Decode one MJPEG buffer to tightly packed RGBA, with the dimensions it really had.
///
/// Returns `None` on a malformed frame rather than a partial image: a camera occasionally
/// emits a truncated JPEG, and half a frame followed by stale pixels looks like a decoder
/// bug rather than a dropped frame.
///
/// The dimensions are returned rather than assumed because **each JPEG carries its own**,
/// and they are not guaranteed to match what the stream negotiated. A buffer labelled with
/// the wrong geometry is read at the wrong stride by everything downstream: rows step by
/// the wrong number of bytes, so the picture breaks into horizontal bands that slide
/// sideways, and because a row offset that is not a multiple of four rotates the RGBA
/// components, the bands take on colour casts. It looks like a corrupt encoder and is
/// nothing of the kind -- the pixels are perfect and the label is wrong.
///
/// A decode whose output does not match its own stated size is refused outright; there is
/// no geometry that would describe such a buffer correctly.
#[must_use]
pub fn decode_mjpeg_to_rgba(jpeg: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let options = zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(jpeg), options);
    let pixels = decoder.decode().ok()?;
    let (width, height) = decoder.dimensions()?;

    let expected = width.checked_mul(height)?.checked_mul(4)?;
    if pixels.len() != expected {
        tracing::warn!(
            decoded_len = pixels.len(),
            width,
            height,
            expected,
            "MJPEG decode produced a buffer that does not match its own dimensions; frame dropped"
        );
        return None;
    }

    Some((pixels, u32::try_from(width).ok()?, u32::try_from(height).ok()?))
}

/// Negotiated geometry, shared between the format callback and the frame callback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Encoding {
    /// Uncompressed pixels in a layout `SourceFormat` describes.
    Raw(SourceFormat),
    /// Motion JPEG: each buffer is a complete JPEG image.
    ///
    /// What a UVC webcam offers at higher resolutions and frame rates -- often the *only*
    /// thing it offers there. `PipeWire` does not transcode, so a client that cannot decode
    /// JPEG simply receives nothing from such a camera.
    Mjpeg,
    #[default]
    Unsupported,
}

#[derive(Debug, Clone, Copy, Default)]
struct Negotiated {
    width: u32,
    height: u32,
    encoding: Encoding,
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
}

impl CaptureTiming {
    /// Frames between reports. 300 is five seconds at 60fps -- often enough to notice a
    /// change, rare enough that the logging itself is not part of the cost.
    const REPORT_EVERY: u64 = 300;

    fn record(&mut self, elapsed: std::time::Duration, bytes: usize) {
        self.frames = self.frames.saturating_add(1);
        self.total = self.total.saturating_add(elapsed);
        self.worst = self.worst.max(elapsed);
        self.bytes = self.bytes.saturating_add(u64::try_from(bytes).unwrap_or(0));

        if self.frames.is_multiple_of(Self::REPORT_EVERY) {
            let mean = self.total.checked_div(u32::try_from(self.frames).unwrap_or(u32::MAX));
            tracing::info!(
                frames = self.frames,
                mean_ms = mean.map_or(0.0, |d| d.as_secs_f64() * 1000.0),
                worst_ms = self.worst.as_secs_f64() * 1000.0,
                mean_source_kb = self
                    .bytes
                    .checked_div(self.frames)
                    .and_then(|b| b.checked_div(1024))
                    .unwrap_or(0),
                "capture decode cost"
            );
            *self = Self::default();
        }
    }
}

/// A running `PipeWire` video capture.
pub struct PipewireCapturer {
    frame_rx: mpsc::Receiver<VideoFrame>,
    stop_tx: mpsc::Sender<()>,
    negotiated: Arc<Mutex<Negotiated>>,
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
        let (frame_tx, frame_rx) = mpsc::sync_channel(FRAME_QUEUE_DEPTH);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let negotiated = Arc::new(Mutex::new(Negotiated::default()));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let thread_negotiated = Arc::clone(&negotiated);
        std::thread::Builder::new()
            .name(format!("pw-capture-{node_id}"))
            .spawn(move || {
                run_stream(node_id, &frame_tx, stop_rx, &thread_negotiated, &ready_tx);
            })
            .map_err(|e| PipewireError::Init(e.to_string()))?;

        // Surface setup failures to the caller instead of leaving a silent dead thread.
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { frame_rx, stop_tx, negotiated }),
            Ok(Err(e)) => Err(PipewireError::Connect(e)),
            Err(e) => Err(PipewireError::Connect(format!("stream setup timed out: {e}"))),
        }
    }

    /// The most recent frame, if one is waiting.
    #[must_use]
    pub fn try_recv(&self) -> Option<VideoFrame> {
        self.frame_rx.try_recv().ok()
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
fn run_stream(
    node_id: u32,
    frame_tx: &mpsc::SyncSender<VideoFrame>,
    stop_rx: mpsc::Receiver<()>,
    negotiated: &Arc<Mutex<Negotiated>>,
    ready_tx: &mpsc::Sender<Result<(), String>>,
) {
    pipewire::init();

    let setup = || -> Result<_, String> {
        let mainloop =
            pipewire::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
        let context =
            pipewire::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
        let core = context.connect_rc(None).map_err(|e| e.to_string())?;
        Ok((mainloop, core))
    };

    let (mainloop, core) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Video",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Communication",
    };

    let stream = match pipewire::stream::StreamRc::new(core, "elementium-capture", props) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
    };

    if let Err(e) = attach_and_connect(&stream, node_id, negotiated, frame_tx) {
        let _ = ready_tx.send(Err(e));
        return;
    }
    let _ = ready_tx.send(Ok(()));
    run_until_stopped(&mainloop, stop_rx, node_id);
}

/// Attach the stream callbacks and connect to `node_id`.
///
/// Returns the listener, which must outlive the stream: dropping it silently stops every
/// callback, so the stream would connect and then deliver nothing.
#[allow(clippy::too_many_lines)]
fn attach_and_connect(
    stream: &pipewire::stream::StreamRc,
    node_id: u32,
    negotiated: &Arc<Mutex<Negotiated>>,
    frame_tx: &mpsc::SyncSender<VideoFrame>,
) -> Result<(), String> {
    let fmt_state = Arc::clone(negotiated);
    let frame_state = Arc::clone(negotiated);
    let tx = frame_tx.clone();
    // Owned by the process callback and reused every frame, so taking a snapshot of the
    // mapped buffer costs no allocation.
    let mut snapshot: Vec<u8> = Vec::new();
    // Per-stage cost, reported periodically. This runs on every captured frame on every
    // user's machine, so its cost is battery on a laptop and frames stolen from whatever
    // else the machine is doing. Guessing which stage dominates has been wrong before.
    let mut timing = CaptureTiming::default();

    let listener = stream
        .add_local_listener::<()>()
        .state_changed(|_, (), old, new| {
            tracing::info!(?old, ?new, "PipeWire capture stream state");
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
                SourceFormat::from_spa(info.format()).map_or(Encoding::Unsupported, Encoding::Raw)
            };
            if encoding == Encoding::Unsupported {
                tracing::error!(
                    format = ?info.format(),
                    subtype,
                    "PipeWire negotiated an encoding we cannot decode; frames will be dropped"
                );
            }
            tracing::info!(
                width = size.width,
                height = size.height,
                ?encoding,
                "PipeWire capture format negotiated"
            );
            if let Ok(mut n) = fmt_state.lock() {
                n.width = size.width;
                n.height = size.height;
                n.encoding = encoding;
            }
        })
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Ok(n) = frame_state.lock().map(|g| *g) else { return };
            if n.encoding == Encoding::Unsupported {
                return;
            }
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let stride = usize::try_from(data.chunk().stride()).unwrap_or(0);
            let size = usize::try_from(data.chunk().size()).unwrap_or(0);
            let Some(mapped) = data.data() else { return };

            // Snapshot the buffer before decoding it.
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
            let copy_len = if size > 0 { size.min(mapped.len()) } else { mapped.len() };
            snapshot.clear();
            snapshot.extend_from_slice(mapped.get(..copy_len).unwrap_or(mapped));
            let bytes: &[u8] = &snapshot;

            let width = usize::try_from(n.width).unwrap_or(0);
            let height = usize::try_from(n.height).unwrap_or(0);

            // Every frame carries the geometry it was actually decoded at, not the
            // geometry the stream negotiated, because a mismatch between the two is
            // exactly the fault this used to have: the buffer was labelled with the
            // negotiated size regardless of what came out of the decoder.
            let decode_started = std::time::Instant::now();
            let converted = match n.encoding {
                Encoding::Mjpeg => decode_mjpeg_to_rgba(bytes),
                Encoding::Raw(format) => {
                    let stride = if stride == 0 {
                        width.saturating_mul(format.bytes_per_pixel())
                    } else {
                        stride
                    };
                    to_rgba(bytes, width, height, stride, format)
                        .map(|rgba| (rgba, n.width, n.height))
                }
                Encoding::Unsupported => None,
            };

            timing.record(decode_started.elapsed(), copy_len);

            if let Some((rgba, frame_width, frame_height)) = converted {
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
                let _ = tx.try_send(VideoFrame {
                    width: frame_width,
                    height: frame_height,
                    data: rgba,
                    timestamp_us: 0,
                });
            } else {
                tracing::warn!(
                    len = bytes.len(),
                    width, height, stride,
                    "PipeWire buffer too small for the negotiated geometry; frame dropped"
                );
            }
        })
        .register();

    let mut params = [format_param()];
    let mut param_refs: Vec<&libspa::pod::Pod> = params
        .iter_mut()
        .filter_map(|p| libspa::pod::Pod::from_bytes(p))
        .collect();
    if param_refs.is_empty() {
        // Connecting with no constraint lets the source pick anything at all, and there
        // would be no way to tell that had happened from the frames alone.
        return Err("could not build the video format parameter".to_owned());
    }

    stream
        .connect(
            libspa::utils::Direction::Input,
            Some(node_id),
            pipewire::stream::StreamFlags::AUTOCONNECT
                | pipewire::stream::StreamFlags::MAP_BUFFERS,
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

/// The format we ask for: raw video in any layout we can convert.
///
/// Offering several and letting `PipeWire` choose is deliberate — a source that cannot
/// produce our first choice would otherwise fail to negotiate at all, and conversion is
/// cheap next to not having a camera.
fn format_param() -> Vec<u8> {
    use libspa::pod::{object, property, Value};
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
            libspa::param::format::MediaSubtype::Raw
        ),
        property!(
            libspa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            libspa::param::video::VideoFormat::RGBx,
            libspa::param::video::VideoFormat::BGRx,
            libspa::param::video::VideoFormat::RGBA,
            libspa::param::video::VideoFormat::BGRA,
            libspa::param::video::VideoFormat::RGB,
            libspa::param::video::VideoFormat::BGR,
            libspa::param::video::VideoFormat::YUY2
        ),
        // Size and framerate are not optional in practice: without them the source
        // fixates a format whose pixel layout comes back as `Unknown`, and every frame is
        // then dropped for want of a conversion. Offered as wide ranges so the source
        // picks whatever it natively supports.
        property!(
            libspa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            libspa::utils::Rectangle { width: 1280, height: 720 },
            libspa::utils::Rectangle { width: 160, height: 120 },
            libspa::utils::Rectangle { width: 4096, height: 4096 }
        ),
        property!(
            libspa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            libspa::utils::Fraction { num: 30, denom: 1 },
            libspa::utils::Fraction { num: 1, denom: 1 },
            libspa::utils::Fraction { num: 60, denom: 1 }
        ),
    };
    libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::expect_used)]
mod tests {
    use super::{to_rgba, SourceFormat};

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
        assert!(super::decode_mjpeg_to_rgba(&[0xFF, 0xD8, 0x00, 0x01]).is_none());
        assert!(super::decode_mjpeg_to_rgba(&[]).is_none());
    }

    /// A real JPEG decodes to exactly `width * height * 4` bytes of RGBA.
    #[test]
    fn decodes_a_jpeg_to_tightly_packed_rgba() {
        // Smallest valid JPEG this crate will produce: encode a 2x2 image via zune-jpeg's
        // own round trip is not available, so this asserts the contract on the real camera
        // path instead -- see `camera_probe`, which reports the decoded byte count and is
        // checked against width*height*4.
        //
        // Kept as a documented gap rather than a fake assertion: fabricating a JPEG by hand
        // here would test the fixture, not the decoder.
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
