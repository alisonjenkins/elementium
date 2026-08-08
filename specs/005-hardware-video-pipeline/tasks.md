# Tasks: Finish the hardware video pipeline

**Spec**: [spec.md](spec.md)

T001 gates everything else. If H.264 is never negotiated, the rest of this feature
is answering a question nobody asked.

## Phase 1: Does it engage at all?

- [X] T001 [US1] Read the SDP from a real call and record whether H.264 is offered and negotiated — the log already contains the offer and answer, so this needs reading rather than instrumenting. **Answered: it is not offered.** See the Finding in spec.md
- [X] T002 [US1] Log the selected encoder backend and codec once per call in `src-tauri/src/commands/media_devices.rs`, so "is the GPU being used" is answerable from a log rather than by reasoning about the policy
- [X] T003 [US1] If H.264 is not negotiated, establish whether that is Element Call's choice, the SFU's `enabled_publish_codecs`, or our own offer being wrong — the three have different owners. **Answered: ours.** `clear_codecs().enable_opus(true).enable_vp8(true)` at `peer_connection.rs:159` never offers H.264, so the other two were never consulted
- [ ] T012 [US1] Make the offered codec set include H.264 and make the send path follow the negotiated codec instead of assuming VP8 (payload selection at `peer_connection.rs:637`, and the packetiser). ~~Blocked on feature 003~~ — **unblocked 2026-08-08**, 003 is closed and remote video works. Split into T013–T016 below, because reading it through found a fourth part that is not in this description and that breaks peers rather than us
- [ ] T013 [US1] Offer H.264 alongside VP8 at `crates/elementium-webrtc/src/peer_connection.rs:159`
- [ ] T014 [US1] Select the payload type from the codec of the frame being written rather than `Codec::Vp8`, in `write_video` (`peer_connection.rs:637`), and packetise accordingly
- [ ] T015 [US1] Make `unencrypted_header_size` in `crates/elementium-e2ee/src/lib.rs` codec-aware and match livekit's H.264 rule. **Do this first of the three**: it currently reads the VP8 frame tag on every video frame, so an H.264 frame would have its clear-text header sized from an unrelated bit and no peer could authenticate it — silently, and only at the receiver. The contract is written down in `spec.md`, read from livekit-client 2.21.0: clear bytes end two bytes into the first slice NAL unit, **and** the ciphertext must be RBSP-escaped on the way out and unescaped on the way in, which nothing in this repository does today
- [ ] T016 [US1] Confirm against a real peer that H.264 video is decrypted and displayed, not merely negotiated — the failure mode T015 describes is invisible from the sending side

## Phase 2: Prove the saving

- [ ] T004 [US2] Measure CPU per frame on the software path with a real camera, as a baseline (the capture counters added for the frame-rate work already report decode cost)
- [ ] T005 [US2] Measure the same on the hardware path and record both, so the claim about 37.6% is checked rather than repeated
- [ ] T006 [US3] Confirm no CPU JPEG decode happens on the hardware path beyond the rate-limited self-view, using the `offered`/`rate_limited` counters

## Phase 3: The rest of the hardware

- [ ] T007 [US3] Implement the `VideoToolbox` probe and encoder for macOS in `crates/elementium-codec/src/hardware.rs` and a new `videotoolbox` module — the selection policy is already platform-independent, so only the probe and construction are new
- [ ] T008 [US3] Implement the Media Foundation probe and encoder for Windows, likewise
- [ ] T009 [US1] AV1 encoding on VAAPI: the GPU offers it to 8192x4352 and it carries the same quality in roughly two thirds of H.264's bitrate. Only worth doing once H.264 is proven to engage. **Blocked for encrypted calls, which is all of them**: livekit's frame cryptor throws on AV1 outright (`av1 is not yet supported for end to end encryption`), so an AV1 publisher cannot be decrypted by any Element Call peer

## Phase 4: The copies that remain

- [ ] T010 [US2] Import the camera's buffer as a DMA-BUF so it reaches the GPU without the CPU touching it, removing the two remaining copies on the accelerated path. This is a different mechanism from the current staging-image upload rather than an optimisation of it
- [ ] T011 [US2] Extend the zero-copy test to cover the surface upload, so a copy reintroduced there fails a test rather than showing up in a benchmark later
