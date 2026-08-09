//! Does E2EE ciphertext make str0m's H.264 packetiser split a frame where no NAL
//! boundary exists?
//!
//! Hypothesis under test (no network, no SFU): AES-GCM ciphertext is effectively random
//! bytes, so if a start-code-like sequence (`00 00 01` / `00 00 00 01`) survives into the
//! packetiser's input, `str0m::packet::h264::H264Packetizer` splits the frame there and
//! emits fragments with bogus NAL headers taken from ciphertext. Chrome would discard
//! those; our own depacketiser, being str0m's symmetric counterpart, would happen to glue
//! them back together.
//!
//! The counter-argument from reading the code: `elementium-e2ee` RBSP-escapes everything
//! after the clear header -- ciphertext, IV and trailer as one buffer
//! (`crates/elementium-e2ee/src/lib.rs`, `encrypt_frame`), so no `00 00 [00..=03]` can
//! exist inside the body, and the boundary between the clear header and the body cannot
//! form a start code either as long as the clear header ends two bytes into a slice NAL
//! whose first payload byte has `first_mb_in_slice == 0` (that byte then always has its
//! top bit set, so the header never ends in the two zeros a spanning start code needs).
//!
//! This test settles it empirically over many frames of real ciphertext:
//!
//! 1. Packetize plaintext and encrypted versions of the same frames and assert the
//!    encrypted frame splits into exactly the same number of RTP payloads with the same
//!    NAL packet types -- i.e. the packetiser found no phantom NAL boundaries.
//! 2. Reconstruct the frame from those payloads exactly the way Chrome does (libwebrtc
//!    inserts 4-byte start codes for STAP-A members and single/FU-A NALs --
//!    `modules/video_coding/h264_sps_pps_tracker.cc`, `start_code_h264[] = {0,0,0,1}` --
//!    which is also precisely what str0m's own `H264Depacketizer` emits) and assert the
//!    reconstruction is byte-identical to the encrypted frame we handed the packetiser.
//! 3. Run the `LiveKit` receiver's framing on the reconstruction (our `h264` module is a
//!    differentially-tested transliteration of livekit-client 2.21.0's `naluUtils.ts`)
//!    and assert it selects the same clear-header bytes the sender authenticated, then
//!    decrypt the reconstruction and get the original frame back.
//!
//! If all of that holds across keyframes and hundreds of delta frames, the bytes we put
//! on the wire are provably reassembled and decrypted correctly by a byte-faithful
//! Chrome + livekit-client receiver, and the packetiser-split hypothesis is dead.

// Test setup failing loudly beats a green test that asserted nothing; the workspace's
// `expect_used` ban targets the shipping paths.
#![allow(clippy::missing_const_for_fn, clippy::expect_used, clippy::indexing_slicing)]

use elementium_e2ee::{E2eeContext, E2eeOptions, MediaKind, h264};
use elementium_types::{PlaintextMedia, WireMedia};
use str0m::format::CodecExtra;
use str0m::unversioned::{Depacketizer, H264Depacketizer, H264Packetizer, Packetizer};

const MTU: usize = 1200;
const STAP_A: u8 = 24;

const fn nal_type(b: u8) -> u8 {
    b & 0x1F
}

/// Deterministic pseudo-random bytes so the slice payloads are not compressible filler.
/// The exact content is irrelevant -- after encryption the packetiser sees AES-GCM
/// output either way -- but realistic input keeps the plaintext side honest.
struct Lcg(u32);
impl Lcg {
    fn next_byte(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        u8::try_from((self.0 >> 24) & 0xff).unwrap_or(0)
    }
    fn fill(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_byte()).collect()
    }
}

/// The keyframe shape `ELEMENTIUM_FRAME_DUMP` showed on the wire: SPS (`67 42 c0 ...`),
/// PPS, then a slice NAL. `first_payload_byte` is the first byte of slice data after the
/// NAL header -- a real encoder's first slice starts with `first_mb_in_slice = 0`, whose
/// Exp-Golomb coding pins the top bit, hence `0x9a`-style values in the dumps.
fn annexb_keyframe(rng: &mut Lcg, slice_payload_len: usize) -> Vec<u8> {
    let start = [0u8, 0, 0, 1];
    let mut f = Vec::new();
    f.extend_from_slice(&start);
    f.extend_from_slice(&[0x67, 0x42, 0xc0, 0x28, 0x96, 0x54, 0x05, 0x01, 0x6c, 0x80]);
    f.extend_from_slice(&start);
    f.extend_from_slice(&[0x68, 0xce, 0x3c, 0x80]);
    f.extend_from_slice(&start);
    f.push(0x65); // IDR slice NAL header
    f.push(0x9a); // first slice byte, top bit set (first_mb_in_slice = 0)
    f.extend(rng.fill(slice_payload_len));
    f
}

/// The delta-frame shape from the dumps: one non-IDR slice NAL (`61 9a ...`).
fn annexb_delta(rng: &mut Lcg, slice_payload_len: usize) -> Vec<u8> {
    let mut f = vec![0u8, 0, 0, 1, 0x61, 0x9a];
    f.extend(rng.fill(slice_payload_len));
    f
}

fn e2ee_sender() -> E2eeContext {
    let ctx = E2eeContext::new(E2eeOptions::default());
    ctx.set_local_identity("alice");
    ctx.set_key("alice", 0, b"video-key-material-abcdefghijkl");
    ctx
}

fn encrypt(ctx: &E2eeContext, frame: &[u8]) -> Vec<u8> {
    ctx.encrypt_frame(
        &PlaintextMedia::from_encoder(frame.to_vec()),
        MediaKind::VideoH264,
    )
    .expect("encryption succeeds")
    .into_bytes()
}

/// Packetize one frame and return the payloads. A fresh packetiser per call would lose
/// the SPS/PPS buffering that spans `emit` calls, so the caller owns the packetiser,
/// exactly as `write_video` owns str0m's.
fn packetize(p: &mut H264Packetizer, frame: &[u8]) -> Vec<Vec<u8>> {
    p.packetize(MTU, frame)
        .expect("packetizing well-formed Annex B must not fail")
}

/// Rebuild the access unit from RTP payloads the way Chrome's receive path does: 4-byte
/// start codes before every NAL recovered from STAP-A, single-NAL and FU-A packets.
/// str0m's depacketizer implements exactly that convention, so it stands in for
/// libwebrtc's `h264_sps_pps_tracker.cc`.
fn reassemble_like_chrome(payloads: &[Vec<u8>]) -> Vec<u8> {
    let mut d = H264Depacketizer::default();
    let mut extra = CodecExtra::None;
    let mut out = Vec::new();
    for p in payloads {
        d.depacketize(p, &mut out, &mut extra)
            .expect("depacketizing our own wire output must not fail");
    }
    out
}

/// The core differential: for every frame, encrypted packetization must mirror the
/// plaintext packetization -- same payload count, same packet types -- and the
/// receiver-side reconstruction must be byte-identical to what was encrypted, agree on
/// the clear header, and decrypt back to the original.
#[test]
fn encrypted_frames_packetize_identically_to_plaintext_and_survive_the_wire() {
    let ctx = e2ee_sender();
    let mut rng = Lcg(0x1234_5678);

    // A GOP-ish sequence: keyframe, then hundreds of delta frames, repeated. Sizes match
    // the observed captures (1577-byte keyframes at 320x240, 83-122 byte deltas) plus a
    // keyframe large enough to force FU-A fragmentation.
    let mut frames = Vec::new();
    frames.push(annexb_keyframe(&mut rng, 1540)); // ~1577 bytes, FU-A territory
    for _ in 0..300 {
        frames.push(annexb_delta(&mut rng, 100));
    }
    frames.push(annexb_keyframe(&mut rng, 600)); // small keyframe, single-NAL slice
    for _ in 0..300 {
        frames.push(annexb_delta(&mut rng, 80));
    }

    let mut plain_packetizer = H264Packetizer::default();
    let mut enc_packetizer = H264Packetizer::default();

    for (i, frame) in frames.iter().enumerate() {
        let encrypted = encrypt(&ctx, frame);

        let plain_payloads = packetize(&mut plain_packetizer, frame);
        let enc_payloads = packetize(&mut enc_packetizer, &encrypted);

        // 1. No phantom NAL boundaries: the ciphertext body must not add splits. The
        // encrypted frame is larger (GCM tag + IV + trailer + escapes), so an FU-A run
        // may gain a fragment; what it must never do is change the *kinds* of packets or
        // split a single-NAL frame into several NALs.
        let plain_types: Vec<u8> = plain_payloads
            .iter()
            .filter_map(|p| p.first().copied().map(nal_type))
            .collect();
        let enc_types: Vec<u8> = enc_payloads
            .iter()
            .filter_map(|p| p.first().copied().map(nal_type))
            .collect();
        let type_class = |t: u8| if t == 28 { 28 } else { t }; // collapse FU-A runs
        let plain_classes: Vec<u8> = plain_types.iter().map(|&t| type_class(t)).collect();
        let enc_classes: Vec<u8> = enc_types.iter().map(|&t| type_class(t)).collect();
        let dedup = |v: &[u8]| {
            let mut d: Vec<u8> = Vec::new();
            for &t in v {
                if d.last() != Some(&t) {
                    d.push(t);
                }
            }
            d
        };
        assert_eq!(
            dedup(&plain_classes),
            dedup(&enc_classes),
            "frame {i}: encrypted packetization changed packet-type structure \
             (plain {plain_types:?} vs encrypted {enc_types:?}) -- a start code \
             survived into the packetiser's input"
        );
        // For frames whose payloads all fit the MTU the count itself must match exactly.
        if plain_types.iter().all(|&t| t != 28) && enc_types.iter().all(|&t| t != 28) {
            assert_eq!(
                plain_payloads.len(),
                enc_payloads.len(),
                "frame {i}: encrypted frame split into a different number of packets"
            );
        }
        // A delta frame must be exactly one single-NAL packet, encrypted or not.
        if frame[4] == 0x61 {
            assert_eq!(
                enc_payloads.len(),
                1,
                "frame {i}: encrypted delta frame no longer fits one single-NAL packet"
            );
            assert_eq!(nal_type(enc_payloads[0][0]), 1, "frame {i}: NAL type mangled");
        }

        // 2. Chrome-faithful reassembly is byte-identical to the encrypted frame.
        let rebuilt = reassemble_like_chrome(&enc_payloads);
        assert_eq!(
            rebuilt, encrypted,
            "frame {i}: receiver-side reconstruction differs from the frame we encrypted \
             -- AAD would mismatch and AES-GCM would reject every frame"
        );

        // 3. The receiver's framing picks the same clear header the sender authenticated
        // (both sides run livekit's findNALUIndices/findSliceNALUUnencryptedBytes logic).
        let sender_clear = h264::unencrypted_bytes(frame).expect("sender finds the slice");
        let receiver_clear =
            h264::unencrypted_bytes(&rebuilt).expect("receiver finds the slice");
        assert_eq!(
            &rebuilt[..receiver_clear],
            &frame[..sender_clear],
            "frame {i}: receiver-side clear header bytes differ from the sender's AAD"
        );

        // 4. And the reconstruction actually decrypts back to the original.
        let decrypted = ctx
            .decrypt_frame(
                &WireMedia::from_network(rebuilt),
                "alice",
                MediaKind::VideoH264,
            )
            .expect("decryption must not error")
            .expect("decryption must produce output");
        assert_eq!(
            decrypted.as_bytes(),
            frame.as_slice(),
            "frame {i}: wire round trip corrupted the frame"
        );
    }
}

/// The STAP-A keyframe path specifically: SPS and PPS travel in the clear, so the
/// packetiser must still recognise and aggregate them, and the slice packet must carry
/// the clear `65`/`61` NAL header Chrome keys its keyframe detection on.
#[test]
fn encrypted_keyframe_still_yields_stap_a_with_clear_parameter_sets() {
    let ctx = e2ee_sender();
    let mut rng = Lcg(0xdead_beef);
    let frame = annexb_keyframe(&mut rng, 600);
    let encrypted = encrypt(&ctx, &frame);

    let mut p = H264Packetizer::default();
    let payloads = packetize(&mut p, &encrypted);

    assert_eq!(payloads.len(), 2, "STAP-A + one single-NAL slice expected");
    assert_eq!(nal_type(payloads[0][0]), STAP_A);
    // STAP-A: header byte, then len+SPS, len+PPS -- both byte-identical to the plaintext
    // parameter sets, because they sit inside the E2EE clear header.
    let sps_len = usize::from(u16::from_be_bytes([payloads[0][1], payloads[0][2]]));
    assert_eq!(&payloads[0][3..3 + sps_len], &frame[4..4 + sps_len]);
    assert_eq!(
        nal_type(payloads[1][0]),
        5,
        "the slice packet must still open with the clear IDR NAL header"
    );
}

/// The theoretical clear-header/ciphertext boundary hole, pinned down so it is a known
/// quantity rather than a suspicion: a start code *can* span the boundary, but only when
/// the clear header's last byte is `0x00` -- and the clear header ends two bytes into the
/// first slice NAL, whose second byte always has its top bit set for a first slice
/// (`first_mb_in_slice = 0` Exp-Golomb-codes with a leading 1 bit). So the hole is
/// unreachable for streams whose frames begin at macroblock 0, which is every frame our
/// encoder produces. This documents the boundary rule the escaping relies on.
#[test]
fn the_escaping_boundary_is_safe_because_the_clear_header_cannot_end_in_zero() {
    // write_rbsp never escapes a body that merely *starts* with `00 01 ...`; only a
    // preceding pair of zeros triggers it, and those zeros would have to come from the
    // clear header:
    assert_eq!(h264::write_rbsp(&[0x00, 0x01, 0xaa]), vec![0x00, 0x01, 0xaa]);
    // ...so `[header ends 0x00][body 0x00 0x01]` would form a start code. Show the
    // guard that prevents it: the sender's framing puts the boundary two bytes into the
    // slice NAL, and for a first slice that second byte is >= 0x80, never zero.
    let mut rng = Lcg(1);
    let frame = annexb_keyframe(&mut rng, 64);
    let clear = h264::unencrypted_bytes(&frame).expect("slice found");
    assert_eq!(frame[clear - 2] & 0x1F, 5, "boundary is inside the slice NAL");
    assert!(
        frame[clear - 1] & 0x80 != 0,
        "first slice byte carries first_mb_in_slice=0's leading 1 bit"
    );
}
