# Tasks: Call media faults

**Spec**: [spec.md](spec.md)

Ordered so each phase produces something checkable on its own. The E2EE work is
first because it is the single cause behind two of the three symptoms.

## Phase 1: Diagnosis that does not depend on a call

- [X] T001 [US1][US2] Record what our E2EE encrypt path actually produces for one frame — key, key index, IV, AAD, trailer layout — in `crates/elementium-e2ee/src/lib.rs`, and write it down in `research.md` beside what LiveKit's own client does
- [X] T002 [US1][US2] Compare against the reference: read the key derivation and frame layout the JS `livekit-client` E2EE worker uses, and record every point where ours differs
- [X] T003 [US1][US2] Write a round-trip test in `crates/elementium-e2ee/` that encrypts with our code and decrypts with our code, to establish the internal contract holds before questioning the wire format
- [ ] T004 [US1][US2] Write a test against a captured real frame from the reference client, so "matches LiveKit" is checked rather than assumed

## Phase 2: E2EE correctness (US1, US2)

- [ ] T005 [US1][US2] Fix the difference T002 identifies (depends on T002, T004)
- [X] T006 [US1][US2] Make a decrypt failure name the participant, key index and reason rather than only a count, in `crates/elementium-webrtc/src/e2ee_io.rs`
- [X] T007 [US1][US2] Rate-limit the failure log: 331 identical lines for one call is unreadable, and the interesting fact is that it happens at all

## Phase 3: The transport outliving its connection (US1, US3)

- [X] T008 [US1][US3] Distinguish a closed channel from a full one at the `try_send` in `src-tauri/src/commands/media_devices.rs`, counting and reporting them separately
- [X] T009 [US1][US3] Detach a pipeline whose channel is closed, so it stops encoding into nothing and reports itself as unattached (depends on T008)
- [X] T010 [US1][US3] Re-attach the capture pipelines when a peer connection is created, not only in `create_offer`, so a connection replaced mid-call is picked up in `src-tauri/src/commands/webrtc.rs`
- [X] T011 [US1][US3] Test that a pipeline whose connection closes and is replaced resumes sending, without needing a new offer

## Phase 4: Confirmation in a real call

- [ ] T012 [US1] Two-participant call: confirm audio is heard both ways, including across a mid-call input-device change
- [ ] T013 [US2] Two-participant call: confirm video is seen both ways
- [ ] T014 [US1][US2][US3] Confirm the logs show zero decrypt failures and zero closed-channel drops for a healthy call


## Progress note (2026-08-07T14:10:00Z)

**T001-T003 done by consulting the reference rather than reimplementing it.**
livekit-client, Element Call and LiveKit's native transformer were read
directly and every element of our format checked against them. All of it
matches: frame layout `[header][ciphertext+tag][IV][IV_LENGTH][key_index]`,
unencrypted header of 1 byte for Opus and 10/3 for VP8 key/delta frames, the
header as AES-GCM associated data, and HKDF-SHA256 with salt
`"LKFrameEncryptionKey"`, 128 zero bytes of info, 128-bit output. The key
posted to livekit's worker is the same material captured at
`importKey("raw", ...)`, so re-deriving it in Rust yields the identical AES
key.

Two things came out of it that the logs alone could not have shown.

**A real bug, fixed.** Element Call rotates with `(prev + 1) % 256` and
configures `keyringSize: 256`, so indices legitimately exceed 15. Our ring
held 16 and aliased index 19 onto 3, overwriting a key still in use.

**The reported symptom is probably not a key problem at all.** A livekit
participant has ONE current key index shared by all its tracks, so the
observed steady 3 for audio and 6 for video cannot come from one sender. That
points at the frames being misread rather than the keys being missing --
livekit encrypts *before* RED encoding and operates on fully depacketized
frames, so an Opus payload still wrapped in RFC 2198 RED, or a VP8 payload
with its descriptor attached, would fail exactly this way and produce
"indices" that are just whatever byte sits at that offset.

T004 is now the priority, and the check is cheap: the second-to-last byte of
a livekit frame is always 12. `trailer_looks_like_livekit` reports it.
