//! Does str0m's H.264 packetiser emit something Chrome's depacketiser is entitled to reject?
//!
//! Background (measured, not assumed): our encrypted H.264 is forwarded correctly by the
//! SFU and decoded fine by our own Rust client, but Chrome never assembles a frame --
//! `packetsReceived` climbs, `framesReceived` stays 0, PLIs pile up. The RTP headers are
//! right and the Annex B bytes handed to the packetiser are the same shape libwebrtc hands
//! its own packetiser (verified with `ELEMENTIUM_FRAME_DUMP=1`). That leaves exactly one
//! layer: how str0m turns that Annex B frame into RTP packets, and whether that shape is
//! legal under the `packetization-mode` we actually negotiate.
//!
//! `crates/elementium-webrtc/src/peer_connection.rs` (`write_video`) picks, among the
//! several H.264 payload types str0m offers, the one that "permits fragmentation" --
//! i.e. whose negotiated `packetization-mode` is not 0. This test does not touch that
//! selection logic (the file is off limits in this session); it instead calls str0m's own
//! `H264Packetizer` directly -- no network, no SFU -- on frames shaped like the ones we
//! actually send, to answer two questions with evidence:
//!
//! 1. What does str0m actually put on the wire for a multi-NAL frame (SPS+PPS+IDR, the
//!    1577-byte shape we observed) and for a single-slice frame (the 83-122 byte shape)?
//! 2. Does the packetiser itself know or care what `packetization-mode` was negotiated?
//!
//! Finding: str0m's `H264Packetizer::packetize` takes no mode parameter at all (see
//! `str0m::packet::h264::H264Packetizer::packetize`, which forwards straight to `emit`).
//! It always aggregates a pending SPS+PPS pair into a single STAP-A packet (NAL type 24)
//! ahead of the NAL that follows them, and always fragments any NAL larger than the MTU
//! into FU-A packets (NAL type 28) -- regardless of what packetization-mode was put in the
//! SDP. Both STAP-A and FU-A are legal only under `packetization-mode` 1 (RFC 6184 SS5.6,
//! non-interleaved mode); `packetization-mode` 0 (RFC 6184 SS6.2, single NAL unit mode)
//! forbids NAL types 24-31 outright, and a compliant receiver -- Chrome included -- is
//! entitled to drop them silently, which matches the symptom exactly (packets counted,
//! no frame assembled, PLI storm).
//!
//! So the packetiser is only correct because `write_video` steers it onto a
//! packetization-mode-1 payload type. This test proves what the packetiser emits is
//! legal *only* under mode 1, by reconstructing RFC 6184's mode-0 restriction from first
//! principles and showing the emitted packet types violate it. It cannot observe the SDP
//! `write_video` actually negotiates (that requires the excluded file and a live peer), so
//! it does not assert a live mismatch -- it demonstrates the mechanism the guard in
//! `write_video` exists to prevent, and that removing that guard would reproduce the bug.

// Test setup failing loudly beats a green test that asserted nothing; the workspace's
// `expect_used` ban targets the shipping paths.
#![allow(clippy::expect_used)]

use str0m::format::CodecExtra;
use str0m::unversioned::{Depacketizer, H264Depacketizer, H264Packetizer, Packetizer};

const STAP_A: u8 = 24;
const FU_A: u8 = 28;
const SPS: u8 = 7;
const PPS: u8 = 8;
const IDR: u8 = 5;

const fn nal_type(b: u8) -> u8 {
    b & 0x1F
}

/// The first byte of `packets[index]`, without indexing.
fn first_byte(packets: &[Vec<u8>], index: usize) -> Option<u8> {
    packets.get(index).and_then(|p| p.first()).copied()
}

/// Build an Annex B frame: SPS, PPS, then an IDR slice `slice_payload_len` bytes long
/// (not counting the NAL header byte), each separated by a 4-byte start code -- the exact
/// shape `ELEMENTIUM_FRAME_DUMP` showed for our 1577-byte outbound keyframes ("begins with
/// SPS and evidently contains more than one NAL").
fn annexb_keyframe(slice_payload_len: usize) -> Vec<u8> {
    let start_code = [0x00, 0x00, 0x00, 0x01];
    let sps_body = [0x67, 0x42, 0xc0, 0x1f, 0x1a, 0x32, 0x35, 0x01, 0x40, 0x7a];
    let pps_body = [0x68, 0xce, 0x3c, 0x80];

    let mut frame = Vec::new();
    frame.extend_from_slice(&start_code);
    frame.extend_from_slice(&sps_body);
    frame.extend_from_slice(&start_code);
    frame.extend_from_slice(&pps_body);
    frame.extend_from_slice(&start_code);
    // NAL header byte for an IDR slice: forbidden_zero_bit=0, nal_ref_idc=3, type=5.
    frame.push(0x65);
    frame.extend(std::iter::repeat_n(0xAB, slice_payload_len));
    frame
}

/// A single-slice non-keyframe, delta frame: the 83-122 byte shape we observed for
/// ordinary frames (no SPS/PPS, one NAL, well under any sane MTU).
fn annexb_delta_frame(slice_payload_len: usize) -> Vec<u8> {
    let start_code = [0x00, 0x00, 0x00, 0x01];
    let mut frame = Vec::new();
    frame.extend_from_slice(&start_code);
    // Non-IDR slice: nal_ref_idc=2, type=1.
    frame.push(0x41);
    frame.extend(std::iter::repeat_n(0xCD, slice_payload_len));
    frame
}

/// The 1577-byte multi-NAL keyframe shape: str0m must emit the SPS+PPS pair as a single
/// STAP-A packet (NAL type 24) and the oversized IDR as FU-A fragments (NAL type 28) --
/// never as one packet containing multiple Annex B NALs back to back, which RFC 6184
/// never permits under any packetization-mode.
#[test]
fn multi_nal_keyframe_becomes_stap_a_plus_fua() {
    // Payload of ~1550 bytes puts the whole frame at ~1577 bytes, matching the observed
    // capture, and is comfortably larger than any real-world MTU.
    let frame = annexb_keyframe(1550);
    let mtu = 1200;

    let mut packetizer = H264Packetizer::default();
    let payloads = packetizer
        .packetize(mtu, &frame)
        .expect("packetizing a well-formed Annex B frame must not fail");

    assert!(
        payloads.len() >= 2,
        "expected at least a STAP-A packet and one FU-A fragment, got {} packets",
        payloads.len()
    );

    // First packet: STAP-A aggregating SPS and PPS. Never a raw concatenation of
    // multiple start-coded NALs in one RTP payload -- that shape does not exist in RFC
    // 6184 under any packetization-mode.
    let first_type = first_byte(&payloads, 0).map(nal_type);
    assert_eq!(
        first_type,
        Some(STAP_A),
        "SPS+PPS must be aggregated into a STAP-A packet, got NAL type {first_type:?}"
    );

    // Every remaining packet is an FU-A fragment of the IDR slice.
    for (i, p) in payloads.iter().enumerate().skip(1) {
        let ty = p.first().copied().map(nal_type);
        assert_eq!(
            ty,
            Some(FU_A),
            "packet {i} should be an FU-A fragment of the oversized IDR slice, got NAL type {ty:?}"
        );
    }

    // FU-A start/end bits: exactly one start, exactly one end, in order.
    let fua_headers: Vec<u8> = payloads
        .iter()
        .skip(1)
        .map(|p| *p.get(1).expect("FU-A packet must have a header byte"))
        .collect();
    let start_count = fua_headers.iter().filter(|b| **b & 0x80 != 0).count();
    let end_count = fua_headers.iter().filter(|b| **b & 0x40 != 0).count();
    assert_eq!(start_count, 1, "exactly one FU-A start fragment expected");
    assert_eq!(end_count, 1, "exactly one FU-A end fragment expected");
    assert!(
        fua_headers.first().is_some_and(|b| b & 0x80 != 0),
        "the first FU-A fragment must carry the start bit"
    );
    assert!(
        fua_headers.last().is_some_and(|b| b & 0x40 != 0),
        "the last FU-A fragment must carry the end bit"
    );

    // Round-trip through str0m's own depacketizer to confirm the emitted packets
    // reconstruct byte-identical Annex B, i.e. this is not just plausible-looking framing.
    let mut depacketizer = H264Depacketizer::default();
    let mut extra = CodecExtra::None;
    let mut reconstructed = Vec::new();
    for p in &payloads {
        depacketizer
            .depacketize(p, &mut reconstructed, &mut extra)
            .expect("depacketizing str0m's own output must not fail");
    }
    assert_eq!(
        reconstructed, frame,
        "STAP-A + FU-A must reconstruct the original Annex B frame exactly"
    );
}

/// The 83-122 byte single-slice shape: well under the MTU, so str0m must emit it as one
/// single-NAL-unit packet (NAL type 1-23), which is legal under packetization-mode 0 or 1.
#[test]
fn small_delta_frame_becomes_single_nal_packet() {
    let frame = annexb_delta_frame(100);
    let mtu = 1200;

    let mut packetizer = H264Packetizer::default();
    let payloads = packetizer
        .packetize(mtu, &frame)
        .expect("packetizing a well-formed Annex B frame must not fail");

    assert_eq!(
        payloads.len(),
        1,
        "a single small NAL should produce exactly one RTP packet"
    );
    let nal_ty = first_byte(&payloads, 0)
        .map(nal_type)
        .expect("expected exactly one packet");
    assert!(
        (1..=23).contains(&nal_ty),
        "small single-slice frames must be single-NAL-unit packets, got NAL type {nal_ty}"
    );
}

/// The decisive point: `H264Packetizer::packetize` has no `packetization-mode` parameter
/// and no field tracking one. It emits STAP-A/FU-A purely based on NAL size and whether an
/// SPS+PPS pair is pending -- it cannot know, and does not ask, whether the negotiated
/// payload type advertised `packetization-mode=0` or `=1`.
///
/// RFC 6184 SS6.2 (single NAL unit packetization mode, i.e. `packetization-mode` absent or
/// 0): "the single NAL unit packet, STAP-A, STAP-B, MTAP16, MTAP24, FU-A, and FU-B
/// payload structures... the single NAL unit packet MUST be used" -- STAP-A and FU-A are
/// simply not among the packet types SS6.2 permits. A receiver that negotiated mode 0 is
/// RFC-compliant in discarding them outright.
///
/// This test packetizes the same oversized keyframe used above and shows the output
/// contains STAP-A/FU-A regardless -- i.e. nothing internal to str0m would stop this from
/// being sent on a payload type whose SDP `fmtp` line says `packetization-mode=0`. The
/// only thing preventing that today is `write_video` in `peer_connection.rs` choosing a
/// payload type that permits fragmentation before handing frames to this packetizer; this
/// test cannot observe that selection (the file is out of scope for this session and SDP
/// negotiation needs a live peer), but it demonstrates precisely the mechanism that
/// selection guards against.
#[test]
fn packetizer_output_contains_types_forbidden_under_packetization_mode_zero() {
    let frame = annexb_keyframe(1550);
    let mtu = 1200;

    let mut packetizer = H264Packetizer::default();
    let payloads = packetizer
        .packetize(mtu, &frame)
        .expect("packetizing a well-formed Annex B frame must not fail");

    let types_forbidden_under_mode_zero: Vec<u8> = payloads
        .iter()
        .filter_map(|p| p.first().copied().map(nal_type))
        .filter(|t| (24..=31).contains(t))
        .collect();

    assert!(
        !types_forbidden_under_mode_zero.is_empty(),
        "expected this frame shape to force at least one STAP-A/FU-A packet -- if it \
         didn't, the packetizer changed behaviour and the mode-0 hazard this test \
         documents may no longer apply"
    );
    // NAL type 24 is STAP-A (used here for the SPS+PPS pair); FU-A/FU-B (28/29) are the
    // other RFC 6184 SS6.2-forbidden types this packetizer can emit for an oversized NAL.
    for t in &types_forbidden_under_mode_zero {
        assert!(
            *t == STAP_A || *t == FU_A,
            "unexpected forbidden-under-mode-0 NAL type {t}"
        );
    }
}

/// Sanity check on the two SPS/PPS bodies used above, so a future edit to
/// `annexb_keyframe` can't silently stop testing what it claims to.
#[test]
fn keyframe_fixture_starts_with_sps_then_pps() {
    let frame = annexb_keyframe(10);
    // 4-byte start code, then SPS NAL header.
    let sps_byte = frame.get(4).copied().expect("frame has an SPS NAL header");
    assert_eq!(nal_type(sps_byte), SPS);

    // Walk to the next start code to find the PPS.
    let pps_start = frame
        .windows(4)
        .position(|w| w == [0x00, 0x00, 0x00, 0x01])
        .map(|i| i + 4)
        .and_then(|from| {
            frame
                .get(from..)
                .and_then(|rest| rest.windows(4).position(|w| w == [0x00, 0x00, 0x00, 0x01]))
                .map(|i| from + i + 4)
        })
        .expect("fixture must contain a second start code before the PPS");
    let pps_byte = frame
        .get(pps_start)
        .copied()
        .expect("frame has a PPS NAL header");
    assert_eq!(nal_type(pps_byte), PPS);
    let _ = IDR; // referenced via the NAL header byte 0x65 above; kept for documentation.
}
