//! Does the STAP-A we put on the wire survive Chromium's own validation of it?
//!
//! Chromium, subscribing to our encrypted H.264, assembles no frames at all and says why
//! in its logs and nowhere else:
//!
//! ```text
//! video_rtp_depacketizer_h264.cc:86] Incorrect StapA packet.
//! ... Failed to parse payload for ssrc: 3620057542
//! h264_sps_pps_tracker.cc:75] No PPS with id 1 received
//! ```
//!
//! The STAP-A is the packet carrying the SPS and the PPS. Rejected, the parameter sets
//! never arrive, so every IDR after it is dropped for want of a PPS, the receiver asks for
//! a keyframe forever, and `framesReceived` stays at zero. The same stream unencrypted is
//! decoded without a single dropped frame, which is what makes this measurable at all.
//!
//! A STAP-A is a length-prefixed list: `[hdr][u16 size][NAL]...`, and the sizes must tile
//! the payload exactly. This checks that property directly, against packets from the same
//! packetiser the publisher uses, so the fault can be seen without a browser.

// Assertions and fixed test indices; the workspace's bans are aimed at shipping paths.
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use elementium_e2ee::{E2eeContext, E2eeOptions, MediaKind};
use elementium_types::PlaintextMedia;
use str0m::unversioned::{H264Packetizer, Packetizer};

/// RFC 6184's aggregation packet type.
const STAP_A: u8 = 24;
/// A real path MTU, so fragmentation happens where it happens in production.
const MTU: usize = 1200;

/// `ParseStapAStartOffsets` from `video_rtp_depacketizer_h264.cc`, which is the thing that
/// logged "Incorrect `StapA` packet": walk the length prefixes and require that they land
/// exactly on the end of the payload.
fn stap_a_tiles_exactly(payload: &[u8]) -> Result<usize, String> {
    // Past the STAP-A NAL header byte.
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

/// An Annex B keyframe: SPS, PPS, then an IDR slice, as our encoder emits.
///
/// The slice filler is never 0 or 1, so it cannot manufacture a start code by accident --
/// a fixture that does that measures itself, which has already produced one false result
/// in this investigation.
fn annexb_keyframe(slice_payload_len: usize) -> Vec<u8> {
    let start_code = [0x00, 0x00, 0x00, 0x01];
    let mut frame = Vec::new();
    frame.extend_from_slice(&start_code);
    frame.extend_from_slice(&[0x67, 0x42, 0xc0, 0x1f, 0x1a, 0x32, 0x35, 0x01, 0x40, 0x7a]);
    frame.extend_from_slice(&start_code);
    frame.extend_from_slice(&[0x68, 0xce, 0x3c, 0x80]);
    frame.extend_from_slice(&start_code);
    frame.push(0x65);
    for i in 0..slice_payload_len {
        frame.push(u8::try_from(i % 250).unwrap_or(7).saturating_add(2));
    }
    frame
}

fn context() -> E2eeContext {
    let ctx = E2eeContext::new(E2eeOptions::default());
    ctx.set_local_identity("tester");
    ctx.set_key("tester", 0, &[7_u8; 16]);
    ctx
}

/// Check every STAP-A the packetiser produced for one frame.
fn check_all_stap_a(label: &str, frame: &[u8]) -> Vec<String> {
    let packets = H264Packetizer::default()
        .packetize(MTU, frame)
        .expect("a well-formed Annex B frame must packetise");
    let mut faults = Vec::new();
    for (i, packet) in packets.iter().enumerate() {
        let Some(&first) = packet.first() else {
            faults.push(format!("{label}: packet {i} is empty"));
            continue;
        };
        if first & 0x1F != STAP_A {
            continue;
        }
        if let Err(why) = stap_a_tiles_exactly(packet) {
            faults.push(format!(
                "{label}: packet {i} ({} bytes) is not a valid STAP-A: {why}",
                packet.len()
            ));
        }
    }
    faults
}

/// The plaintext case, which Chromium decodes without dropping a frame. If this ever
/// fails, the comparison below means nothing.
#[test]
fn plain_keyframes_produce_well_formed_stap_a() {
    for slice_len in [40_usize, 400, 1550, 4000] {
        let faults = check_all_stap_a("plain", &annexb_keyframe(slice_len));
        assert!(faults.is_empty(), "slice_len {slice_len}: {faults:?}");
    }
}

/// The failing case: the same frames, encrypted exactly as the publisher encrypts them.
#[test]
fn encrypted_keyframes_produce_well_formed_stap_a() {
    let ctx = context();
    let mut faults = Vec::new();
    for slice_len in [40_usize, 400, 1550, 4000] {
        let plain = annexb_keyframe(slice_len);
        let encrypted = ctx
            .encrypt_frame(&PlaintextMedia::from_encoder(plain), MediaKind::VideoH264)
            .expect("a frame with a key installed must encrypt");
        faults.extend(check_all_stap_a(
            &format!("encrypted slice_len {slice_len}"),
            encrypted.as_bytes(),
        ));
    }
    assert!(
        faults.is_empty(),
        "Chromium rejects a STAP-A whose length prefixes do not tile its payload, and \
         rejecting it costs the receiver the SPS and PPS -- after which every IDR is \
         dropped for want of a PPS and no frame is ever assembled: {faults:?}"
    );
}

/// The aggregated NAL count, which is the other way a STAP-A can be wrong while still
/// tiling: carrying the wrong units, or fewer of them than the frame had.
#[test]
fn encryption_does_not_change_what_the_stap_a_aggregates() {
    let ctx = context();
    let plain = annexb_keyframe(1550);
    let encrypted = ctx
        .encrypt_frame(&PlaintextMedia::from_encoder(plain.clone()), MediaKind::VideoH264)
        .expect("encrypts");

    let count = |frame: &[u8]| -> Vec<usize> {
        H264Packetizer::default()
            .packetize(MTU, frame)
            .expect("packetises")
            .iter()
            .filter(|p| p.first().is_some_and(|b| b & 0x1F == STAP_A))
            .map(|p| stap_a_tiles_exactly(p).unwrap_or(0))
            .collect()
    };

    assert_eq!(
        count(&plain),
        count(encrypted.as_bytes()),
        "encryption changed how many NAL units the aggregation packets carry"
    );
}
