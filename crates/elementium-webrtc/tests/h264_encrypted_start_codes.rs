//! Does an encrypted H.264 frame still contain exactly the start codes it should?
//!
//! The fault being chased: our encrypted H.264 reaches Chrome as packets it counts and
//! frames it never assembles, while plain H.264 works, VP8 with the same encryption works,
//! and our own client round-trips the encrypted stream fine.
//!
//! str0m's packetiser splits its input on Annex B start codes. So a start code appearing
//! *inside* what should be one NAL does not corrupt anything visibly here — it makes str0m
//! emit two NALs, the second with a header byte taken from ciphertext. Chrome sees a NAL
//! type that means nothing and discards it; our own depacketiser, being str0m's symmetric
//! counterpart, concatenates the pieces back and decodes. That shape fits every observation
//! this bug has.
//!
//! Ciphertext is uniformly random, so it produces `00 00 01` by chance, and the encryption
//! path escapes for exactly that reason. The question these tests ask is whether the
//! escaping covers everything that ends up in the packetiser's input — including the join
//! between the clear header and the escaped body, which is escaped as two separate pieces.

// A failed setup step should stop the test loudly; the workspace's `expect_used` ban is
// aimed at shipping paths, not assertions.
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use elementium_e2ee::{E2eeContext, E2eeOptions, MediaKind};
use elementium_types::PlaintextMedia;

/// Every index at which a three-byte start code prefix appears.
fn start_code_positions(data: &[u8]) -> Vec<usize> {
    data.windows(3)
        .enumerate()
        .filter(|(_, w)| w == &[0, 0, 1])
        .map(|(i, _)| i)
        .collect()
}

/// An Annex B frame shaped like the ones this codebase publishes: SPS, PPS, then a slice.
///
/// The slice body is filled with a fixed pattern rather than zeros so that any start code
/// found in the encrypted output came from the encryption, not from the input.
fn annex_b_frame(slice_len: usize, tail_of_header: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x28, 0x96, 0x54, 0x0a, 0x0f]);
    frame.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
    frame.extend_from_slice(&[0, 0, 0, 1, 0x65]);
    // The first bytes of the slice are left in the clear by the encryption scheme, so what
    // they end with decides what the escaped body is concatenated onto.
    frame.extend_from_slice(tail_of_header);
    for i in 0..slice_len {
        // Never 0 or 1, so the filler can neither extend a run of zeros nor complete a
        // start code against the header tail -- otherwise the fixture manufactures the very
        // thing under test, which it did on the first attempt.
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

/// An encrypted frame must contain no start codes beyond the ones its NALs began with.
///
/// Run many times because the failure is probabilistic: it needs the clear header to end in
/// a particular byte pattern, or the ciphertext to land one by chance, and a single frame
/// would pass by luck.
#[test]
fn encryption_introduces_no_start_codes_inside_a_nal() {
    let ctx = context();
    let mut offenders = 0_u32;
    let mut checked = 0_u32;

    for round in 0..400_usize {
        // Vary the slice length so the clear/escaped join lands on different content, and
        // vary the tail so the header sometimes ends in the zeros that make a start code
        // possible across the boundary.
        let tail: &[u8] = match round % 4 {
            0 => &[0x88, 0x84, 0x00, 0x21],
            1 => &[0x88, 0x84, 0x00, 0x00],
            2 => &[0x9a, 0xff, 0x2d, 0xb9],
            _ => &[0x00, 0x00, 0x00, 0x00],
        };
        let plain = annex_b_frame(64 + round, tail);
        let plain_codes = start_code_positions(&plain).len();
        assert_eq!(
            plain_codes, 3,
            "fixture must contain exactly the three NAL start codes it wrote, not one it \
             manufactured by accident; round {round}, tail {tail:02x?}"
        );

        let encrypted = ctx
            .encrypt_frame(&PlaintextMedia::from_encoder(plain), MediaKind::VideoH264)
            .expect("a frame with a key installed must encrypt");
        let encrypted_codes = start_code_positions(encrypted.as_bytes()).len();

        checked += 1;
        if encrypted_codes != plain_codes {
            offenders += 1;
            if offenders == 1 {
                let head: String = encrypted
                    .as_bytes()
                    .iter()
                    .take(48)
                    .fold(String::new(), |mut acc, b| {
                        use std::fmt::Write as _;
                        let _ = write!(&mut acc, "{b:02x}");
                        acc
                    });
                println!(
                    "round {round}: plaintext had {plain_codes} start codes, ciphertext has \
                     {encrypted_codes}; header tail {tail:02x?}; first bytes {head}"
                );
            }
        }
    }

    assert_eq!(
        offenders, 0,
        "{offenders} of {checked} encrypted frames gained or lost a start code. str0m splits \
         its input on start codes, so a spurious one makes it emit an extra NAL whose header \
         byte is ciphertext -- which a receiver discards, while our own depacketiser \
         concatenates the pieces back and decodes as though nothing happened"
    );
}

/// The boundary specifically: a clear header ending in zeros, joined to an escaped body.
///
/// `write_rbsp` escapes the body on its own, starting its zero-count at zero, so it cannot
/// see that the bytes it is about to be appended to already end in `00 00`. If that is the
/// gap, this is where it shows.
#[test]
fn a_header_ending_in_zeros_does_not_form_a_start_code_with_the_body() {
    let ctx = context();
    let mut formed = 0_u32;

    for round in 0..200_usize {
        let plain = annex_b_frame(48 + round, &[0x88, 0x00, 0x00, 0x00]);
        let expected = start_code_positions(&plain).len();
        assert_eq!(expected, 3, "fixture sanity: three NALs, three start codes");
        let encrypted = ctx
            .encrypt_frame(&PlaintextMedia::from_encoder(plain), MediaKind::VideoH264)
            .expect("encrypts");
        if start_code_positions(encrypted.as_bytes()).len() != expected {
            formed += 1;
        }
    }

    assert_eq!(
        formed, 0,
        "{formed} frames formed a start code where the clear header meets the escaped body"
    );
}

/// The same question one level down: does str0m packetise an encrypted frame the same way?
///
/// The start-code check above is a proxy. This is the thing that actually matters, because
/// str0m's packetiser is what turns a frame into what Chrome receives: if encryption made
/// it emit a different number of NALs, the extra ones would carry header bytes taken from
/// ciphertext, and a receiver would discard them while our own depacketiser -- str0m's
/// symmetric counterpart -- would concatenate them back and decode.
#[test]
fn encryption_does_not_change_how_strom_splits_the_frame() {
    use str0m::unversioned::{H264Packetizer, Packetizer};

    let ctx = context();
    // Well above any NAL here, so the count reflects how the frame was *split*, not how it
    // was fragmented -- fragmentation is a separate mechanism and was ruled out already.
    let mtu = 100_000;

    for round in 0..200_usize {
        let tail: &[u8] = match round % 3 {
            0 => &[0x88, 0x84, 0x00, 0x00],
            1 => &[0x9a, 0xff, 0x2d, 0xb9],
            _ => &[0x00, 0x00, 0x00, 0x00],
        };
        let plain = annex_b_frame(96 + round, tail);
        let encrypted = ctx
            .encrypt_frame(&PlaintextMedia::from_encoder(plain.clone()), MediaKind::VideoH264)
            .expect("encrypts");

        let plain_packets = H264Packetizer::default()
            .packetize(mtu, &plain)
            .expect("plaintext packetises");
        let encrypted_packets = H264Packetizer::default()
            .packetize(mtu, encrypted.as_bytes())
            .expect("ciphertext packetises");

        assert_eq!(
            plain_packets.len(),
            encrypted_packets.len(),
            "round {round}: str0m split the plaintext into {} packets and the encrypted frame \
             into {} -- encryption changed the NAL structure, so a receiver is being handed \
             NALs whose headers are ciphertext",
            plain_packets.len(),
            encrypted_packets.len()
        );
    }
}
