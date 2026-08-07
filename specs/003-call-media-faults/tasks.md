# Tasks: Call media faults

**Spec**: [spec.md](spec.md)

Ordered so each phase produces something checkable on its own. The E2EE work is
first because it is the single cause behind two of the three symptoms.

## Phase 1: Diagnosis that does not depend on a call

- [X] T001 [US1][US2] Record what our E2EE encrypt path actually produces for one frame — key, key index, IV, AAD, trailer layout — in `crates/elementium-e2ee/src/lib.rs`, and write it down in `research.md` beside what LiveKit's own client does
- [X] T002 [US1][US2] Compare against the reference: read the key derivation and frame layout the JS `livekit-client` E2EE worker uses, and record every point where ours differs
- [X] T003 [US1][US2] Write a round-trip test in `crates/elementium-e2ee/` that encrypts with our code and decrypts with our code, to establish the internal contract holds before questioning the wire format
- [X] T004 [US1][US2] Check our format against the reference implementation rather than against ourselves — satisfied more strongly than written: `frontend/tests/browser/receive-path.spec.ts` has a real Chromium running livekit's own E2EE worker decrypt what our Rust encrypts, which tests the whole format against the reference rather than one captured frame

## Phase 2: E2EE correctness (US1, US2)

- [X] T005 [US1][US2] ~~Fix the difference T002 identifies~~ — there is no difference. Closed as answered rather than done; see the finding below
- [X] T006 [US1][US2] Make a decrypt failure name the participant, key index and reason rather than only a count, in `crates/elementium-webrtc/src/e2ee_io.rs`
- [X] T007 [US1][US2] Rate-limit the failure log: 331 identical lines for one call is unreadable, and the interesting fact is that it happens at all

## Phase 3: The transport outliving its connection (US1, US3)

- [X] T008 [US1][US3] Distinguish a closed channel from a full one at the `try_send` in `src-tauri/src/commands/media_devices.rs`, counting and reporting them separately
- [X] T009 [US1][US3] Detach a pipeline whose channel is closed, so it stops encoding into nothing and reports itself as unattached (depends on T008)
- [X] T010 [US1][US3] Re-attach the capture pipelines when a peer connection is created, not only in `create_offer`, so a connection replaced mid-call is picked up in `src-tauri/src/commands/webrtc.rs`
- [X] T011 [US1][US3] Test that a pipeline whose connection closes and is replaced resumes sending, without needing a new offer

## Phase 4: Confirmation in a real call

- [X] T012 [US1] Two-participant call: confirm audio is heard both ways. **Done, three-way**: Elementium sent 12,000 of 12,000 frames and both peers heard them; Elementium decoded 1,219 inbound Opus frames. The mid-call device change is not covered and is still worth doing
- [ ] T013 [US2] Two-participant call: confirm video is seen both ways. **Partly answered**: with two remote publishers only the first delivers frames — see T017
- [ ] T014 [US1][US2][US3] Confirm the logs show zero decrypt failures and zero closed-channel drops for a healthy call
- [ ] T015 [US1] `crates/elementium-webrtc/tests/livekit_local_roundtrip.rs` hangs indefinitely against the local stack — a test designed to take about ten seconds ran for twenty-five minutes without output. It is cited elsewhere as the proof that our client pushes audio through a real SFU with a delivery ratio of 1.000, and that claim is currently unverifiable. Not caused by the encryption-declaration fix: it hangs identically with that change reverted. It exercises `LiveKitRoom`, which the application does not use, so this is a broken instrument rather than a broken product path
- [X] T016 [US2] Check whether more than one remote video renders. **The fix works, and a second fault sits behind it**: the slots are now distinct (`…-2`, `…-3`) where before both were `…-video`; only the first receives frames
- [ ] T017 [US2] Find why the second remote video track is polled once and then abandoned — `has_frame=False` on one call and never asked for again, so a call with two other people shows one picture. See the 2026-08-07 finding in spec.md


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


## Finding (2026-08-07T16:40:00Z): E2EE is not the fault

T005 assumed T002 would find a discrepancy. It did not, and the assumption is
worth recording because it was wrong in a useful way.

Our frame layout, unencrypted header sizes, AAD and HKDF parameters were checked
against livekit-client, Element Call and LiveKit's native transformer, and every
one matches. Then the stronger check: a real Chromium running livekit's own E2EE
worker decrypts audio our Rust encrypted, with **500 packets, 0 lost, 0 concealed
samples**. Our encryption is interoperable with the reference implementation.

So the outbound half of US1 is not an encryption fault. The rest of that path is
also accounted for:

| Link | Evidence |
|---|---|
| Encoded | `encoded 724` |
| Encrypted | no encryption-failure warnings |
| SDP well-formed | `m=audio 111 opus/48000/2`, `sendonly`, msid and ssrc present |
| Track associated | `Added transceiver mid=DKP kind=Audio track_id=ed854cca-…` |
| Written to the socket | `Outbound audio socket pacing: packets 500` |
| **Received by the SFU** | `MediaEgressStats { mid: DKP, packets: 612, rtt: Some(..) }` |

That last line is RTCP receiver reports coming back: the SFU has our audio. The
fault is therefore **downstream of the SFU** — either it does not announce our
track to the other participant, or that participant does not subscribe to it.

Two real bugs were found and fixed while establishing that, neither of which was
the reported one: a 16-slot key ring that aliased index 19 onto index 3 while
Element Call rotates modulo 256, and server-injected frames being fed to AES-GCM
instead of passed through.

T012-T014 remain, and now need the far end's view rather than ours. See feature
004, which builds the environment for exactly that.
