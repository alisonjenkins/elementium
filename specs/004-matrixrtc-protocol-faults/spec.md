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


## Finding — 2026-08-07: one of the two faults is now reproducible on demand

Three real Element Web clients, in a real Element Call, on the local stack. Of the
three scenarios built, two work and one fails — and the one that fails is the one
that was reported.

| Scenario | Result |
|---|---|
| Three join in sequence; the last hears the other two | works |
| Three join; one leaves; the other two keep hearing each other | works |
| **A second call, by devices that have already been in one** | **silent for everyone** |

The failure, from the receiver's own statistics: roughly **1,500 RTP packets** arrive
from each of the two other participants over thirty seconds, and
`totalSamplesReceived` stays at **exactly zero** for both. The media is delivered.
Not one frame of it can be decrypted, for the entire call.

**The device carries it, not the room.** Before each test was given devices of its
own, the leave scenario failed in exactly this way purely because the test before it
had run — a different room, the same devices. Fresh devices in the same room work;
reused devices in a new room do not.

This reframes US1 and US2. The reported symptom is described as following a join or
a leave, and neither a join nor a leave reproduces it. What reproduces it is a device
taking part in a second call, which is what a person does after any join or leave
they were disconnected by — so the anecdote and the measurement agree, while pointing
somewhere different.

### What this does not yet say

Zero samples against 1,500 packets is "no key ever worked", not "the key was late" or
"the key was wrong" — those produce samples, of noise. Which of the three possible
causes it is:

- the second call's key is never sent
- it is sent and never received
- it is received at an index livekit has already stopped attempting

is the next question. It wants the key-arrival logging from T001 and the
derived-versus-forwarded counting from T002 read from a failing run, not another
browser test. The third possibility is described in the 2026-08-07 finding in
`specs/003-call-media-faults/spec.md`.

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
