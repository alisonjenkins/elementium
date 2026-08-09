# Publishing and reconnects, 2026-08-09

Seven builds in one afternoon, each tested on a real call, none of which fixed the reported
fault: the far end could not see or hear the user, and the call cycled through
"reconnecting" every fifteen seconds.

This file exists because the *method* failed, not just the code. Each round took the newest
symptom as the specification, changed the negotiation state machine to suit, and shipped.
Five of those changes fixed real bugs. At least one fixed a bug that did not exist. The net
result oscillated: build six published media for ten seconds, build seven published nothing.
Anyone picking this up should read **N1** before touching anything else.

## What is actually fixed, and stays fixed

Each of these was verified by reverting the fix and watching a test fail. Any that arrives
without that check should be treated as unverified.

- **Tracks were deleted mid-offer.** `createOffer` handed the backend the pending
  transceivers and cleared the list *after* the await, deleting anything published while the
  backend built the SDP. The backend took 1.1 seconds on a real call and the microphone was
  published 20ms in. It went into no offer, and the record was gone, so no later offer could
  carry it. **This was the original fault** and it alone would have kept the user silent
  forever.
- **`negotiationneeded` was never fired** by `addTrack` or `addTransceiver`. livekit-client's
  publisher is driven entirely by that event.
- **`new MediaStreamTrack()`** — an illegal constructor in every browser — was the fallback
  for a remote track with no media, so every remote *audio* track threw, out of its
  try/catch, part-way through applying an answer. The connection was left mid-offer.
- **Descriptions were not serialised.** Each wrote `_signalingState` when its own IPC
  returned, so overlapping operations landed in whatever order the backend answered.
- Plus, from the earlier sweep: `connectionState` never changing, remote tracks having no
  receiver or transceiver, cloned tracks unwired, `setParameters` inert, our own key being a
  decrypt candidate, SDP and TURN credentials reaching the log.

## Open

- [ ] **N1. Rewrite the negotiation handling instead of patching it again.** HIGH, and do
  this first. The current code is seven layered patches over an implementation that was
  already approximate: a negotiation flag, a request sequence counter, an in-flight counter,
  a partial operations chain, a receive-only gate, and an offer-coverage comparison — each
  added to answer one trace. They interact in ways no one has modelled.

  Replace with a faithful reading of the DOM: signalling state derived from which
  descriptions have actually been applied; one real operations queue that every one of
  `createOffer`, `createAnswer`, `setLocalDescription`, `setRemoteDescription` goes through;
  and the negotiation-needed check written from the specification rather than from symptoms.

  Write the tests first. There are five real traces in this session's logs to drive them,
  including the stuck-state one in N2 — that corpus is the most valuable thing produced
  today.

- [ ] **N2. The signalling state reaches `have-local-offer` and never leaves.** HIGH, and
  unexplained — three attempts, no answer. In the 14:29:31 log: an offer is applied at
  44.310, answers are applied at 46.057 and 47.631, and at 49.180 the state still reads
  `have-local-offer`. Operations are serialised by then, so the descriptions cannot be
  overlapping, and no `setLocalDescription` is logged between. Either an answer failed
  silently before the state assignment, or something sets the state that is not logged.
  Whichever it is, the held publish request never releases and the connection dies at the
  fifteen-second timeout. **Instrument the state transition itself** — every write, with its
  cause — rather than inferring it again.

- [ ] **N3. Both peer connections send an offer, and only one should.** MEDIUM. LiveKit runs
  a publisher that offers and a subscriber that only answers. The log shows `createOffer`
  with `tc=6` on *both*, with no `negotiationneeded` from us — so livekit is doing it
  itself, and my "we caused it" fix in build seven was aimed at a fault that was not
  happening. Establish what livekit actually intends here before acting: it may be correct
  and the extra answer may be ours to route, or the subscriber offer may be a consequence of
  something else the shim does. Do not guess again.

- [ ] **N4. The microphone captures silence after the pipeline restarts.** HIGH for the
  user, untouched. Input peak reads 0.126, 0.607, 0.224 and then exactly 0.0 for the rest of
  the call, with the mono-fold channel peak decaying 6.8e-8 → 5.4e-12 — the decay curve of a
  device delivering zeros. It follows a capture-pipeline restart. Separate subsystem from
  everything above, and it would leave the user silent even once publishing works.

- [ ] **N5. `no audio writer/mid available` and `no mid published`.** LOW. Three and one
  occurrence at the teardown seam. Probably consequences of the connection closing rather
  than causes; confirm once N1 lands and the connection survives.

## Instrumentation, which cost more than the bugs

- [ ] **I1. The E2EE key watchdog cries wolf.** It counts every HKDF `importKey` as a
  derived call key, but Matrix's own crypto derives with HKDF too, so unrelated imports are
  reported as call keys that "never reached the worker" — and the message asserts the fault
  is upstream of the bridge. It read as damning and was noise: in the same run twelve keys
  forwarded normally and the only gap was 320ms at startup. Narrow the heuristic or drop the
  claim in the message.

- [x] **I2. A test stub lied, so no test could catch a real crash.** `MediaStreamTrack` was
  stubbed as a constructible class; browsers throw. Fixed, and the lesson generalises: a
  stub that is more permissive than the real thing makes the suite agree with whatever the
  code does.

- [x] **I3. Silent early returns in the code under investigation.** The negotiation check
  returned without logging when it declined to fire, so a log could not distinguish "never
  fired" from "held and never released" — which need opposite fixes. That silence cost a
  full build-and-test round.

- [ ] **I4. Two warnings were over-read as faults today.** The to-device "unexpected
  encrypted event" (benign; the SDK simply does not know the type) and the key watchdog
  above. Both were quoted to the user as findings before being checked. Prefer a counter
  that distinguishes cause from consequence over a warning that describes a symptom.

## Environment, not code

- **OBS holds the webcam.** `another application is using the camera`, every V4L2 fallback
  failing, and the PipeWire node disappearing mid-call are all OBS (pid 80262) owning
  `/dev/video*`. Neither Elementium process had a camera handle. Nothing to fix here, but it
  invalidates camera observations from these calls.
- **A 27-hour-old debug build was running throughout.** `target/debug/elementium`, started
  the previous morning, holding sockets. Not the camera holder, but plausibly a second
  signed-in device contributing to the room's membership churn.
- **`nix run` serves a frozen snapshot**, deliberately, from
  `~/.local/share/elementium/snapshots/latest`. It does not rebuild. One whole test round was
  spent on a build that predated the fix being tested, because this was assumed rather than
  checked. `nix run .#snapshot` refreshes it; `just app-join` and `cargo tauri dev` rebuild
  the shims themselves. **Check the snapshot id in the startup line before believing any
  test result.**

## Still unmeasured, from earlier backlogs

R1 (camera resuming after reconnect), R4 (bitrate recovery), R7 (screen-share picker), and
the audio pacer in `BACKLOG-2026-08-09-audio.md` all need a call that stays up. None of them
can be measured while the connection dies every fifteen seconds, and none should be guessed
at in the meantime.
