//! The same STAP-A question as `h264_stap_a_validity`, but on frames the encoder actually
//! produced.
//!
//! The synthetic version of this passes, and Chromium still rejects the real stream:
//!
//! ```text
//! video_rtp_depacketizer_h264.cc:86] Incorrect StapA packet.
//! ```
//!
//! So whatever malforms the aggregation packet is a property of a real encoded keyframe --
//! its NAL inventory, or the sizes involved -- that a hand-built three-NAL fixture does not
//! have. This runs the real encoder, encrypts exactly as the publisher does, packetises at
//! a real MTU, and reports what it finds, so the difference can be seen without a browser
//! or an SFU in the way.
//!
//! `#[ignore]` because it needs a working VAAPI device: it is a diagnosis, and a machine
//! without a GPU should not fail the suite over one.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::doc_markdown,
    clippy::as_conversions
)]

use elementium_codec::{EncoderConfig, NegotiatedEncoder, VideoCodec, VideoEncoder};
use elementium_e2ee::{E2eeContext, E2eeOptions, MediaKind};
use str0m::unversioned::{H264Packetizer, Packetizer};

const STAP_A: u8 = 24;
const FU_A: u8 = 28;
const MTU: usize = 1200;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

/// `ParseStapAStartOffsets` from `video_rtp_depacketizer_h264.cc`.
fn stap_a_tiles_exactly(payload: &[u8]) -> Result<usize, String> {
    let mut rest = &payload[1..];
    let mut nalus = 0_usize;
    while !rest.is_empty() {
        if rest.len() < 2 {
            return Err(format!("{} trailing byte(s), too few for a size", rest.len()));
        }
        let size = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
        rest = &rest[2..];
        if size > rest.len() {
            return Err(format!(
                "a NAL declares {size} bytes but only {} remain",
                rest.len()
            ));
        }
        rest = &rest[size..];
        nalus = nalus.saturating_add(1);
    }
    Ok(nalus)
}

/// The NAL units an Annex B frame contains, as `(type, length)` pairs.
fn annexb_nalus(frame: &[u8]) -> Vec<(u8, usize)> {
    let mut starts = Vec::new();
    let mut i = 0_usize;
    while i.saturating_add(3) <= frame.len() {
        if frame[i] == 0 && frame[i + 1] == 0 && frame[i + 2] == 1 {
            starts.push(i.saturating_add(3));
            i = i.saturating_add(3);
        } else {
            i = i.saturating_add(1);
        }
    }
    let mut out = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let end = starts
            .get(n.saturating_add(1))
            .map_or(frame.len(), |next| next.saturating_sub(4).max(start));
        out.push((frame[start] & 0x1F, end.saturating_sub(start)));
    }
    out
}

/// A one-line description of each packet: aggregation, fragment, or a whole NAL.
fn describe(packets: &[Vec<u8>]) -> Vec<String> {
    packets
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let Some(&first) = p.first() else {
                return format!("{i}: empty");
            };
            let kind = first & 0x1F;
            let len = p.len();
            match kind {
                STAP_A => match stap_a_tiles_exactly(p) {
                    Ok(n) => format!("{i}: STAP-A, {len} bytes, {n} NALs, valid"),
                    Err(why) => format!("{i}: STAP-A, {len} bytes, INVALID: {why}"),
                },
                FU_A => format!("{i}: FU-A, {len} bytes"),
                other => format!("{i}: single NAL type {other}, {len} bytes"),
            }
        })
        .collect()
}

/// A checkerboard in I420, the same source the publisher encodes.
fn checkerboard(width: u32, height: u32, step: u32) -> elementium_types::I420Frame {
    let (w, h) = (width as usize, height as usize);
    let mut y = vec![0_u8; w.saturating_mul(h)];
    for row in 0..h {
        for col in 0..w {
            let cell = (row.saturating_add(step as usize) / 16).saturating_add(col / 16);
            y[row.saturating_mul(w).saturating_add(col)] = if cell % 2 == 0 { 235 } else { 16 };
        }
    }
    let chroma = w.div_ceil(2).saturating_mul(h.div_ceil(2));
    elementium_types::I420Frame::from_planes(
        width,
        height,
        &y,
        &vec![100_u8; chroma],
        &vec![150_u8; chroma],
        0,
    )
    .expect("well-formed planes")
}

fn context() -> E2eeContext {
    let ctx = E2eeContext::new(E2eeOptions::default());
    ctx.set_local_identity("tester");
    ctx.set_key("tester", 0, &[7_u8; 16]);
    ctx
}

/// Encode real frames, encrypt them, and require every aggregation packet to be one
/// Chromium will accept.
#[test]
#[ignore = "needs a VAAPI device; run with --ignored"]
fn real_encrypted_keyframes_produce_well_formed_stap_a() {
    let mut encoder = NegotiatedEncoder::new(
        VideoCodec::H264,
        EncoderConfig {
            width: WIDTH,
            height: HEIGHT,
            bitrate_kbps: 1_000,
            max_framerate: 25,
        },
    )
    .expect("an H.264 encoder");
    let ctx = context();

    let mut faults = Vec::new();
    let mut described = 0_usize;
    for step in 0..8_u32 {
        let packets = VideoEncoder::encode(&mut encoder, &checkerboard(WIDTH, HEIGHT, step))
            .expect("the encoder must produce a frame");
        for packet in packets {
            let encrypted = ctx
                .encrypt_frame(&packet.data, MediaKind::VideoH264)
                .expect("encrypts");
            let plain: &[u8] = packet.data.as_bytes();
            let plain_nalus = annexb_nalus(plain);
            let encrypted_nalus = annexb_nalus(encrypted.as_bytes());

            let plain_packets = H264Packetizer::default()
                .packetize(MTU, plain)
                .expect("plaintext packetises");
            let encrypted_packets = H264Packetizer::default()
                .packetize(MTU, encrypted.as_bytes())
                .expect("ciphertext packetises");

            // Printed for the first few frames whatever the outcome: the inventory is the
            // thing the synthetic fixture got wrong, so it is worth seeing even on a pass.
            if described < 3 {
                described = described.saturating_add(1);
                let head = plain.iter().take(40).fold(String::new(), |mut acc, b| {
                    use std::fmt::Write as _;
                    let _ = write!(&mut acc, "{b:02x}");
                    acc
                });
                println!("frame {step}: {} bytes plain, NALs {plain_nalus:?}, head {head}", plain.len());
                println!("  plain packets:     {:?}", describe(&plain_packets));
                println!(
                    "frame {step}: {} bytes encrypted, NALs {encrypted_nalus:?}",
                    encrypted.as_bytes().len()
                );
                println!("  encrypted packets: {:?}", describe(&encrypted_packets));
            }

            for (i, p) in encrypted_packets.iter().enumerate() {
                if p.first().is_some_and(|b| b & 0x1F == STAP_A)
                    && let Err(why) = stap_a_tiles_exactly(p)
                {
                    faults.push(format!("frame {step}, packet {i}: {why}"));
                }
            }
        }
    }

    assert!(
        faults.is_empty(),
        "Chromium rejects an aggregation packet whose length prefixes do not tile it, and \
         with it the SPS and PPS the rest of the stream depends on: {faults:?}"
    );
}
