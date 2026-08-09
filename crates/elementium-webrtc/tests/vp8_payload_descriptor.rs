//! What does str0m actually put on the wire for our outbound VP8, and does the absence of
//! a `PictureID` in that descriptor explain "encoded at 30fps, sent with no errors, but the
//! far end decodes only the keyframe it PLIs for every three seconds"?
//!
//! # The hypothesis, and what disproves it
//!
//! `str0m::packet::vp8::Vp8Packetizer` (see `str0m-0.16.2/src/packet/vp8.rs`) is built via
//! `Vp8Packetizer::default()` in `str0m-0.16.2/src/packet/mod.rs` (`Codec::Vp8 =>
//! CodecPacketizer::Vp8(Vp8Packetizer::default())`), which derives `enable_picture_id:
//! false`. The field is private and there is no setter, so every VP8 packet str0m emits for
//! us has the `X` extension bit clear: RFC 7741 SS4.2's REQUIRED one-byte descriptor only,
//! no `PictureID`/TL0PICIDX/TID/KEYIDX extension at all. This test proves that shape directly
//! against str0m's own packetizer (below).
//!
//! Two independent, authoritative sources say that shape is not just legal but an
//! explicitly supported code path, not an edge case tolerated by accident:
//!
//! 1. **libwebrtc's frame-reference finder**
//!    (`modules/video_coding/rtp_frame_reference_finder.cc`,
//!    `RtpFrameReferenceFinder::Impl::ManageFrame`, `kVideoCodecVP8` case). When the parsed
//!    VP8 header's `pictureId == kNoPictureId`, it does not error or refuse the frame -- it
//!    dispatches to `RtpSeqNumOnlyRefFinder::ManageFrame`, a second, fully-implemented
//!    reference finder that orders frames by RTP sequence number instead of `PictureID`. This
//!    is the *designed* fallback for exactly our stream shape, not a crash path.
//!
//! 2. **`LiveKit` SFU's VP8 forwarding path**
//!    (`pkg/sfu/codecmunger/vp8.go`, type `VP8`, and
//!    `pkg/sfu/../mediatransportutil/pkg/codec/vp8.go`, `VP8::Unmarshal`/`MarshalTo`).
//!    `Unmarshal` only reads the `I`/`L`/`T`/`K` extension fields when the packet's `X` bit
//!    is set (`if payload[idx]&0x80 > 0 { ... }`); with `X` clear -- our exact case -- `I`
//!    stays `false` and `PictureID` stays its zero value, no error. The munger's
//!    `SetLast`/`UpdateOffsets`/`UpdateAndGet` all gate their `PictureID`-specific logic
//!    behind `v.pictureIdUsed` (set from `vp8.I`), and `MarshalTo` reproduces the same
//!    extension-less one-byte header on the way back out
//!    (`if v.I || v.L || v.T || v.K { ... } else { buf[idx] &^= 0x80; idx++ }`). Nothing in
//!    that path returns an error or drops a packet because `PictureID` is absent.
//!
//! So `PictureID` is genuinely optional here, exactly as RFC 7741 SS4.2 says ("OPTIONAL"),
//! and both the SFU and the receiver's decode pipeline have first-class support for a
//! `PictureID`-less stream. **The `PictureID` hypothesis from `crates/elementium-webrtc`'s
//! investigation notes is not the cause of the fault** -- see this crate's fix commit
//! history for the next most likely explanation and the evidence gathered for it.
//!
//! This file therefore does not change packetizer behaviour. It exists to pin down, with a
//! runnable assertion, exactly what shape leaves our process -- so if a future str0m
//! upgrade ever flips the packetizer's default (e.g. enables `PictureID`), or if this
//! analysis is revisited, the wire shape it was based on is captured in code, not prose.

#![allow(clippy::expect_used, clippy::panic)]

use str0m::format::CodecExtra;
use str0m::unversioned::{Depacketizer, Packetizer, Vp8Depacketizer, Vp8Packetizer};

/// RFC 7741 SS4.2 `X` bit: extended control bits present.
const X_BIT: u8 = 0x80;
/// RFC 7741 SS4.2 `S` bit: start of a VP8 partition.
const S_BIT: u8 = 0x10;

/// A synthetic VP8 keyframe payload: not a real libvpx bitstream (this test only exercises
/// RTP packetisation, not the codec), just distinct bytes so fragmentation and
/// reconstruction are observable.
fn fake_vp8_frame(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect()
}

/// str0m's default VP8 packetizer -- the one `write_video` actually hands frames to --
/// never sets the RFC 7741 `X` (extended control bits present) bit. Every packet is the
/// bare one-byte required descriptor: `X R N S R PID`, with only `S` (start of partition)
/// ever set, on the first fragment.
///
/// This is the direct evidence for "str0m sends no `PictureID`": not the source reading
/// above, but the packetizer's actual output.
#[test]
fn default_packetizer_emits_no_extension_bit() {
    let frame = fake_vp8_frame(50);
    let mtu = 1200;

    let mut packetizer = Vp8Packetizer::default();
    let payloads = packetizer.packetize(mtu, &frame).expect("packetizing must not fail");

    assert_eq!(payloads.len(), 1, "a 50-byte frame fits in one packet at this MTU");
    let header = payloads.first().and_then(|p| p.first()).copied().expect("packet has a header byte");

    assert_eq!(
        header & X_BIT,
        0,
        "default Vp8Packetizer must not set the X (extended control bits present) bit -- \
         if this now fails, str0m's default changed and the PictureID question should be \
         re-examined"
    );
    assert_eq!(header & S_BIT, S_BIT, "first packet of a frame must carry the S (start) bit");

    // Header is exactly one byte: VP8_HEADER_SIZE with no PictureID/TL0PICIDX/TID/KEYIDX
    // extension appended.
    let payload_len = payloads.first().map_or(0, Vec::len);
    assert_eq!(payload_len, 1 + frame.len(), "descriptor must be exactly one byte with X clear");
}

/// The same shape holds for a multi-packet frame: only the first fragment gets `S`, no
/// fragment ever grows a `PictureID` extension, and str0m's own depacketizer reconstructs the
/// frame exactly -- i.e. this is a self-consistent, round-trippable RTP encoding, not a
/// malformed one that happens to not crash.
#[test]
fn fragmented_frame_still_carries_no_picture_id_and_round_trips() {
    let frame = fake_vp8_frame(3000);
    let mtu = 1200;

    let mut packetizer = Vp8Packetizer::default();
    let payloads = packetizer.packetize(mtu, &frame).expect("packetizing must not fail");
    assert!(payloads.len() > 1, "a 3000-byte frame must fragment at this MTU");

    for (i, p) in payloads.iter().enumerate() {
        let header = p.first().copied().expect("every packet has a header byte");
        assert_eq!(header & X_BIT, 0, "packet {i}: no fragment should set the X bit");
        let expect_s = i == 0;
        assert_eq!(
            (header & S_BIT) == S_BIT,
            expect_s,
            "packet {i}: S bit should be set only on the first fragment"
        );
    }

    let mut depacketizer = Vp8Depacketizer::default();
    let mut extra = CodecExtra::None;
    let mut reconstructed = Vec::new();
    for p in &payloads {
        depacketizer
            .depacketize(p, &mut reconstructed, &mut extra)
            .expect("depacketizing str0m's own output must not fail");
    }
    assert_eq!(reconstructed, frame, "descriptor-only framing must still round-trip exactly");

    // And the depacketizer agrees no PictureID was present: `Vp8CodecExtra::picture_id` is
    // only `Some` when the `I` extension bit was seen on the wire.
    if let CodecExtra::Vp8(vp8_extra) = extra {
        assert_eq!(
            vp8_extra.picture_id, None,
            "depacketized extra must report no PictureID, matching what was sent"
        );
    } else {
        panic!("expected CodecExtra::Vp8 after depacketizing a VP8 stream");
    }
}
