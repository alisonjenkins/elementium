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
