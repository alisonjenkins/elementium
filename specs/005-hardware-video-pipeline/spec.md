# Feature Specification: Finish the hardware video pipeline

**Created**: 2026-08-07
**Status**: Built and verified in isolation; dormant in the running application

## Why this exists

A hardware path exists end to end and has never encoded a frame in a real call.

Built and verified against the GPU: a VAAPI H.264 encoder whose output a reference
decoder reconstructs correctly, a GPU JPEG decoder checked against libjpeg-turbo, a
colour-space converter, and a capture path that hands compressed frames through
untouched. Verified means verified — each has a test that fails when the code is
broken deliberately.

None of it runs, because it engages only when H.264 is negotiated, and it is not
known whether Element Call ever offers H.264. Until that is answered the work is
inert, and the honest description of the state is "built, unproven in production".

The prize is real: MJPEG decoding was 37.6% of the capture path, the single largest
cost in the application, and on this hardware it can move to the GPU entirely.

## What is already true

- The GPU reports H.264 (4096x4096), AV1 (8192x4352), JPEG decode and a
  post-processor.
- `negotiation_order` offers H.264 first on this machine.
- `EncodeTarget::negotiated` resolves to hardware, which flips MJPEG from the worst
  capture format to the best, and the PipeWire path then hands over compressed
  frames — confirmed with a real camera: all 139 frames arrived undecoded.
- No software encoder exists for AV1, so AV1 is negotiable only in hardware.

## User Scenarios

### US1 (P1) — The hardware path is actually used, or known not to be

In a real call on capable hardware, video is encoded on the GPU; and if it is not,
the reason is recorded rather than assumed.

**Independent test**: join a call, read the negotiated codec and the selected
encoder backend from the log.

### US2 (P2) — The saving is measured

The CPU cost of a call on the hardware path is measured against the software path on
the same machine and the same camera.

### US3 (P3) — Other platforms are not left behind

macOS and Windows have hardware encoders; their probes currently return nothing, so
those machines silently take the software path.

## Finding — 2026-08-07: the question T001 asked is answered, and the answer is us

H.264 is never negotiated, and Element Call is not the reason. **We never offer it.**

From the 14:27 session log, our own `createOffer raw SDP` (str0m, our side):

```
m=video 9 UDP/TLS/RTP/SAVPF 96 97
a=rtpmap:96 VP8/90000
a=rtpmap:97 rtx/90000
```

The SFU's answer mirrors it exactly, because an answer cannot introduce a codec the
offer did not contain. So the SFU's `enabled_publish_codecs` and Element Call's
preferences are both exonerated — neither was ever consulted.

The cause is one line, `crates/elementium-webrtc/src/peer_connection.rs:159`:

```rust
RtcConfig::new().clear_codecs().enable_opus(true).enable_vp8(true)
```

`clear_codecs()` removes str0m's defaults and only Opus and VP8 are put back.

This changes what the rest of this feature is. It was written as "find out whether
the hardware path engages"; the answer is that it cannot, for a reason entirely
inside this repository. Enabling H.264 is not a one-line change either — the send
path selects a payload by `Codec::Vp8` (`peer_connection.rs:637`) and packetises
accordingly, so a second codec needs the payload choice and the packetiser to follow
the negotiated codec rather than assume one.

Deliberately not done yet: video is currently not received at all (feature 003), and
changing the offered codec set while that is unexplained would confound the two.
H.264 goes in after remote video works, not before.

## Finding — 2026-08-08: T012 is unblocked, and larger than it is written

Feature 003 is closed and remote video works, so the reason to hold H.264 back is gone.
Reading the work through before starting it turns up a fourth part the task does not
mention, and it is the one that can break other people's calls rather than ours.

**E2EE frame framing is VP8-specific.** `unencrypted_header_size` in
`elementium-e2ee` decides how much of a video frame travels in the clear by reading the
VP8 frame tag: 10 bytes for a key frame, 3 for a delta. That is livekit's VP8 rule, and
it is applied to every `MediaKind::Video` frame unconditionally. An H.264 frame has no
VP8 frame tag; bit 0 of its first byte is part of a NAL header and means something else
entirely. So the header size would be chosen by reading an unrelated bit, and the peer —
which computes its own header size from its own knowledge of the codec — would slice the
frame somewhere else and fail authentication on every frame.

That fails *silently and only for the receiver*, which is the same shape as the fault
this repository has spent two features chasing.

So T012 is four things, and only the first three are in the task text:

1. Offer H.264 alongside VP8 (`peer_connection.rs:159`).
2. Choose the payload type from the codec of the frame being sent, not `Codec::Vp8`
   (`peer_connection.rs:637`).
3. Packetise per negotiated codec.
4. **Make `unencrypted_header_size` codec-aware, matching livekit's H.264 rule.** Without
   this the other three produce video no peer can decrypt.

The encoder side needs nothing new: `VideoCodec`, `VideoEncoder::VaapiH264` and
`ActiveCodec` already exist, and the SFU can already move the pipeline between codecs
mid-call. This is wiring plus one correctness gap, not new capability.

## Success Criteria

- **SC1**: A real call on this machine logs `backend=vaapi` for video, or logs why
  it could not.
- **SC2**: The CPU cost per frame on the hardware path is measured and recorded
  beside the software figure.
- **SC3**: No frame is decoded on the CPU when the GPU is doing the encoding, other
  than the rate-limited self-view.

## Assumptions

- Element Call's codec preferences are not ours to set. If it offers VP8 only, the
  outcome of US1 is that finding, recorded — not a workaround.
