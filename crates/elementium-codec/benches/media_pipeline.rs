//! Per-stage cost of the camera-to-wire path.
//!
//! This code runs on every captured frame for the whole of every call, so its cost is
//! battery on a laptop and CPU taken from whatever else the machine is doing — which for
//! this application is frequently a game. That makes the hot path a product requirement,
//! not a tuning exercise, and requirements need numbers rather than intuition.
//!
//! ```bash
//! cargo bench -p elementium-codec
//! cargo bench -p elementium-codec -- decode   # one stage
//! ```
//!
//! Content matters as much as resolution: a flat test pattern compresses to almost nothing
//! and encodes several times faster than a real camera image, so the fixtures here are
//! synthesised to have realistic local detail rather than being solid colour.

#![allow(
    clippy::expect_used,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::missing_docs_in_private_items,
    clippy::print_stdout,
    clippy::suboptimal_flops
)]

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use elementium_codec::{Vp8Encoder, halve_i420, halve_rgba, i420_to_rgba, rgba_to_i420};
use elementium_types::I420Frame;

/// 720p: what the camera negotiates here, and the common video-call resolution.
const W: u32 = 1280;
const H: u32 = 720;

/// An RGBA image with the frequency content of a photograph.
///
/// This matters more than it looks. An image of per-pixel noise is almost incompressible,
/// so its JPEG is enormous and decoding it is dominated by Huffman work that a real camera
/// frame never produces -- benchmarking against one hides exactly the differences between
/// decoders that matter here. A webcam frame is mostly smooth: broad gradients, a few
/// large shapes, edges at object boundaries, and only mild sensor noise.
///
/// The check that this is representative is the resulting JPEG's size, reported by
/// `fixture_report`: a 720p webcam at quality 85 produces roughly 100-250KB.
fn sample_rgba(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0_u8; w * h * 4];
    let mut seed = 0x9e37_79b9_u32;
    for y in 0..h {
        for x in 0..w {
            // Mild, film-grain-scale noise only.
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((seed >> 26) as u8) as i16 - 32;

            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;

            // Broad lighting gradient, a couple of large soft shapes, and one hard edge --
            // the structure a portrait in a room actually has.
            let lighting = 120.0 + 90.0 * (1.0 - fy) * (0.35 + 0.65 * fx);
            let blob = {
                let (dx, dy) = (fx - 0.45, fy - 0.55);
                let d = (dx * dx * 1.6 + dy * dy).sqrt();
                (60.0 * (0.35 - d).max(0.0) / 0.35).max(0.0)
            };
            let edge = if fx > 0.78 { -35.0 } else { 0.0 };

            let base = lighting + blob + edge;
            let px = |v: f32| (v + f32::from(noise)).clamp(0.0, 255.0) as u8;

            let i = (y * w + x) * 4;
            out[i] = px(base * 1.05);
            out[i + 1] = px(base * 0.95);
            out[i + 2] = px(base * 0.88);
            out[i + 3] = 255;
        }
    }
    out
}

/// A short loop of frames that differ from each other, as a camera's output does.
///
/// Encoding one frame over and over measures an encoder finding nothing to do.
fn moving_sequence(count: usize) -> Vec<I420Frame> {
    (0..count)
        .map(|i| {
            let mut rgba = sample_rgba(W, H);
            // Shift a band of the image so successive frames need real motion vectors.
            let row = (W as usize) * 4;
            let offset = i * 16 * row;
            if offset + row * 64 < rgba.len() {
                rgba.copy_within(offset..offset + row * 64, 0);
            }
            rgba_to_i420(W, H, &rgba)
        })
        .collect()
}

fn sample_i420(width: u32, height: u32) -> I420Frame {
    rgba_to_i420(width, height, &sample_rgba(width, height))
}

/// A JPEG of the sample image, standing in for one MJPEG buffer from the camera.
fn sample_jpeg(width: u32, height: u32) -> Vec<u8> {
    let rgba = sample_rgba(width, height);
    let rgb: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect();
    let mut out = Vec::new();
    // Quality 85: what UVC cameras typically emit for MJPEG.
    jpeg_encoder::Encoder::new(&mut out, 85)
        .encode(
            &rgb,
            width as u16,
            height as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .expect("encode fixture");
    out
}

/// Every stage the camera path runs, individually.
fn stages(c: &mut Criterion) {
    fixture_report();
    let rgba = sample_rgba(W, H);
    let i420 = sample_i420(W, H);
    let jpeg = sample_jpeg(W, H);

    let mut group = c.benchmark_group("stage");
    group.throughput(Throughput::Elements(1));

    // What the camera hands us: one MJPEG buffer per frame.
    group.bench_function("mjpeg_decode_to_rgba", |b| {
        b.iter(|| elementium_media_decode(&jpeg));
    });

    // The same decode without the colour conversion. JPEG is stored as YCbCr, which is
    // what the video encoder wants, so converting to RGB on the way out and back to YUV
    // on the way in is work done twice to arrive where we started. Measured rather than
    // assumed, because the saving is only worth having if the decoder actually skips the
    // conversion when asked for its native colourspace.
    group.bench_function("mjpeg_decode_to_ycbcr", |b| {
        b.iter(|| decode_jpeg_as(&jpeg, zune_core::colorspace::ColorSpace::YCbCr));
    });

    // libjpeg-turbo, decoding to RGBA -- a like-for-like comparison against zune.
    group.bench_function("turbo_decode_to_rgba", |b| {
        b.iter(|| turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::RGBA).expect("turbo rgba"));
    });

    // libjpeg-turbo straight to I420 planes: the encoder's input format, and the JPEG's
    // own storage format, so no colour conversion happens in either direction.
    group.bench_function("turbo_decode_to_i420", |b| {
        b.iter(|| turbojpeg::decompress_to_yuv(&jpeg).expect("turbo yuv"));
    });

    // Half-size decode. JPEG can be decoded at 1/2 scale for far less than full size,
    // which is what the self-view needs -- replacing a full decode followed by a separate
    // downscale.
    group.bench_function("turbo_decode_half_scale", |b| {
        b.iter(|| {
            let mut d = turbojpeg::Decompressor::new().expect("decompressor");
            // Checked, not ignored: a refused scaling factor would silently decode at
            // full size and report the result as the half-scale cost.
            d.set_scaling_factor(turbojpeg::ScalingFactor::ONE_HALF)
                .expect("half-scale decode is supported");
            let header = d.read_header(&jpeg).expect("header");
            let (w, h) = (
                turbojpeg::ScalingFactor::ONE_HALF.scale(header.width),
                turbojpeg::ScalingFactor::ONE_HALF.scale(header.height),
            );
            let mut img = turbojpeg::Image {
                pixels: vec![0_u8; w * h * 4],
                width: w,
                pitch: w * 4,
                height: h,
                format: turbojpeg::PixelFormat::RGBA,
            };
            d.decompress(&jpeg, img.as_deref_mut())
                .expect("half decode");
            img
        });
    });

    // Camera RGBA to the encoder's input format.
    group.bench_function("rgba_to_i420", |b| {
        b.iter(|| rgba_to_i420(W, H, &rgba));
    });

    // Remote frames, for display.
    group.bench_function("i420_to_rgba", |b| {
        b.iter(|| i420_to_rgba(&i420));
    });

    // The self-view downscale, once per captured frame. Both orders are measured because
    // the choice between them is the whole saving: I420 is 1.5 bytes per pixel against
    // RGBA's 4, so halving first touches far less memory and leaves the conversion running
    // on a quarter of the pixels.
    group.bench_function("halve_rgba", |b| {
        b.iter(|| halve_rgba(W, H, &rgba));
    });

    group.bench_function("preview_via_rgba_then_halve", |b| {
        b.iter(|| {
            let full = i420_to_rgba(&i420);
            halve_rgba(full.width, full.height, &full.data)
        });
    });

    group.bench_function("preview_via_halve_then_rgba", |b| {
        b.iter(|| halve_i420(&i420).as_ref().map(i420_to_rgba));
    });

    // VP8 keyframe: a fresh encoder always emits one. Measured separately because it is
    // several times the cost of an interframe and happens rarely -- roughly once every
    // three seconds, or when a receiver asks. Averaging it into the per-frame figure
    // overstates the steady-state cost by an order of magnitude, which is exactly the
    // mistake the first version of this benchmark made.
    group.bench_function("vp8_encode_keyframe", |b| {
        b.iter_batched_ref(
            || Vp8Encoder::new(W, H, 2764, 30).expect("encoder"),
            |enc| enc.encode(&i420),
            BatchSize::PerIteration,
        );
    });

    // VP8 interframe: what 29 frames in every 30 actually cost. The encoder is primed
    // first so its rate control and reference frames are in the state they would be in
    // during a call, and the content changes between iterations because encoding the
    // identical frame repeatedly is trivially cheap and measures nothing real.
    let frames = moving_sequence(8);
    group.bench_function("vp8_encode_interframe", |b| {
        let mut enc = Vp8Encoder::new(W, H, 2764, 30).expect("encoder");
        for f in &frames {
            let _ = enc.encode(f);
        }
        let mut i = 0_usize;
        b.iter(|| {
            i = (i + 1) % frames.len();
            enc.encode(&frames[i])
        });
    });

    group.finish();
}

/// One captured frame, all the way from the camera's buffer to an encoded packet.
///
/// The sum of the parts is what a call actually costs per frame, and it is the number to
/// beat: at 30fps, every millisecond here is 3% of a core held for the length of the call.
fn whole_frame(c: &mut Criterion) {
    let jpeg = sample_jpeg(W, H);

    let mut group = c.benchmark_group("pipeline");
    group.throughput(Throughput::Elements(1));

    // A primed encoder, so this is the steady-state per-frame cost of a call rather than
    // the cost of starting one.
    // What the pipeline used to do: decode to RGBA, downscale in RGBA for the preview,
    // then convert back to YUV for the encoder.
    group.bench_function("capture_to_encoded_frame_via_rgba", |b| {
        let mut enc = Vp8Encoder::new(W, H, 2764, 30).expect("encoder");
        for f in &moving_sequence(4) {
            let _ = enc.encode(f);
        }
        b.iter(|| {
            let (rgba, w, h) = elementium_media_decode(&jpeg).expect("decode");
            let _preview = halve_rgba(w, h, &rgba);
            let i420 = rgba_to_i420(w, h, &rgba);
            enc.encode(&i420)
        });
    });

    // What it does now: decode straight to the encoder's input format, and derive the
    // preview from that. No colour conversion happens in either direction.
    group.bench_function("capture_to_encoded_frame_via_i420", |b| {
        let mut enc = Vp8Encoder::new(W, H, 2764, 30).expect("encoder");
        for f in &moving_sequence(4) {
            let _ = enc.encode(f);
        }
        b.iter(|| {
            let yuv = turbojpeg::decompress_to_yuv(&jpeg).expect("decode");
            // Adopted, not repacked: the decoder's buffer becomes the frame.
            let (w, h) = (yuv.width as u32, yuv.height as u32);
            let (y_stride, uv_stride) = (yuv.y_size().0, yuv.uv_size().0);
            let i420 = I420Frame::from_padded(w, h, yuv.pixels, y_stride, uv_stride, 0)
                .expect("adopt the decoder buffer");
            let _preview = halve_i420(&i420).as_ref().map(i420_to_rgba);
            enc.encode(&i420)
        });
    });

    group.finish();
}

/// Print the fixture's compressed size, so an unrepresentative fixture is visible.
///
/// A 720p webcam at quality 85 emits roughly 100-250KB per frame. This fixture lands a
/// little above that, which is the safe direction to be wrong in: it is slightly harder to
/// decode than a real frame, so any saving measured here is a lower bound. A fixture far
/// *below* the range would flatter every decoder and hide the differences between them.
fn fixture_report() {
    let jpeg = sample_jpeg(W, H);
    println!(
        "fixture: {W}x{H} JPEG is {}KB (a 720p webcam emits ~100-250KB; \
         slightly harder is deliberate)",
        jpeg.len() / 1024
    );
}

/// Decode a JPEG into a chosen colourspace, reporting what came out.
fn decode_jpeg_as(
    jpeg: &[u8],
    space: zune_core::colorspace::ColorSpace,
) -> Option<(Vec<u8>, u32, u32)> {
    let options = zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(space);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(jpeg), options);
    let pixels = decoder.decode().ok()?;
    let (width, height) = decoder.dimensions()?;
    Some((
        pixels,
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    ))
}

/// Decode a JPEG the way the capture path does.
///
/// Duplicated from `elementium-media` rather than depended on: benchmarking the codec
/// crate must not require the whole `PipeWire` stack to build.
fn elementium_media_decode(jpeg: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let options = zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(jpeg), options);
    let pixels = decoder.decode().ok()?;
    let (width, height) = decoder.dimensions()?;
    Some((
        pixels,
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    ))
}

criterion_group!(benches, stages, whole_frame);
criterion_main!(benches);
