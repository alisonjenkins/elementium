# Feature Specification: MatrixRTC protocol faults

**Created**: 2026-08-07
**Status**: Environment built; faults not yet reproduced under it

## Why this exists

Two faults were reported that no test in this repository could reach, because both
are driven by Matrix rather than by media:

1. **Keys take a long time to arrive.** New participants hear nothing for a while.
2. **Someone joining or leaving can silence everyone already in the call.**

Neither is reachable against a bare SFU. Keys travel as Matrix to-device messages,
and Element Call re-keys on membership changes — so reproducing either needs a real
homeserver, real membership events, and at least three participants.

`test-env/` now provides that: synapse, LiveKit and lk-jwt-service on localhost,
brought up and torn down around the browser suite. This feature is about using it.

## What is already known

From reading `matrix-js-sdk`'s `RTCEncryptionManager` and livekit-client, and from
production logs:

- A sender rotates its key on **every leaver**, and on a **joiner** when the current
  key is more than ten seconds old. So both reported faults share a mechanism.
- After distributing a new key, a sender keeps encrypting with the **old** one for
  `useKeyDelay` (1s default) so peers have time to receive it. Whether our client
  honours the same delay when it adopts a key is not established.
- A key can die inside `RTCEncryptionManager` before reaching livekit at all:
  `addKeyToParticipant` parks it indefinitely when no RTC membership matches the
  sender, and `OutdatedKeyFilter` drops keys judged out of order. Neither produces
  a `setKey`, so from our side they are indistinguishable from a key never sent.
- A production session showed inbound frames stamped with key indices **3** and
  **6** while every key our bridge saw was at index 0, 1 or 4 — with no key ever
  failing to be captured. Either those keys never reached livekit's worker, or the
  frames were being misread. `trailer_is_livekit_shaped` now distinguishes the two
  and has not yet been observed in a real call.


## Finding — 2026-08-07: neither fault reproduces with Element Call alone

Three real Element Web clients, in a real Element Call, on the local stack. Four
scenarios, all of which work:

| Scenario | Result |
|---|---|
| Three join in sequence; the last hears the other two | works |
| Three join; one leaves; the other two keep hearing each other | works |
| Everyone hangs up and calls again in the same browser | works |
| The room is encrypted, so key handling actually runs | asserted |

That last row matters: Element Call performs frame encryption only in an encrypted
room, and the first version of this suite used a plain one. It exercised none of the
key handling these faults live in, and passed.

So whatever produces the reported symptom is not Element Call and Matrix alone on
this stack. What remains is Elementium as a participant, or something about a real
homeserver — federation, slower to-device delivery, a busier sync — that a local
Synapse with four users does not reproduce.

### A false reproduction, and what it was worth

An earlier version of this section reported the fault as reproduced on demand, with
numbers: about 1,500 RTP packets arriving from each remote participant over thirty
seconds and `totalSamplesReceived` at exactly zero — media delivered, not one frame
decrypting. That was real, repeatable, and self-inflicted.

The cause was the harness reusing an access token and device id in a *fresh browser
context*. That keeps the device and discards the crypto store, so the other
participants encrypt their keys to a device that can no longer read them. Indistinguishable
from the reported fault by any measurement taken at the receiver.

It was caught by testing a prediction and having it refuted. If the re-send were driven
by a new membership fingerprint appearing, a brand-new participant joining the stuck call
should have unstuck it. It did not — and the next hypothesis, that a browser keeping its
crypto store would be fine, held.

Two things follow, and both are kept:

- `freshSessions` logs each test's participants in again, so no test reuses a device by
  accident. Tests that reuse one now do it deliberately and say so.
- **Packets-without-samples does not identify a cause.** A key never sent, a key sent to a
  device that cannot read it, and a key arriving at an index the receiver has stopped
  attempting all look identical from the receiver. Distinguishing them needs the sender's
  side, which is what the key-arrival logging is for.

### The instrument that settled it

The harness records every `setKey` message on its way to livekit's worker — participant
and index only, never material. It is what turned "no audio" into a specific claim:

```
first call  (works):  ... @tester2:localhost:AUUTRURUIG index 0 ...
second call (broken): only @tester1's own key, seven times
```

## User Scenarios

### US1 (P1) — A participant hears the others promptly after joining

Someone joining an established call hears the others within a bounded time, and the
time is known rather than anecdotal.

**Independent test**: three participants; the third joins late; measure the interval
between joining and the first successfully decrypted frame from each other
participant.

### US2 (P1) — Membership changes do not silence the room

A participant joining or leaving does not stop the remaining participants hearing
each other, beyond a brief and bounded gap.

**Independent test**: three participants in a call; one leaves, then another joins;
measure any interruption in decryptable audio between the two that stayed.

### US3 (P2) — A key that never arrives is diagnosable

When a key does not arrive, the logs say which of the known causes it was: parked
for want of a membership, dropped as out of order, never sent, or sent and not
captured.

## Success Criteria

- **SC1**: A joiner decodes audio from every existing participant within 5 seconds
  of joining, measured, not estimated.
- **SC2**: A leave or a join causes no more than 1 second of undecryptable audio
  between participants that did not move.
- **SC3**: Every key that reaches the client is either installed or logged with the
  reason it was not.
- **SC4**: Both faults are reproducible on demand in `test-env/`, so a fix can be
  shown to fix them.

## Assumptions

- Element Call's rotation policy is not ours to change; we have to interoperate with
  it. If a fix belongs upstream, the outcome of this work is a reproduction and a
  precise description rather than a patch here.
