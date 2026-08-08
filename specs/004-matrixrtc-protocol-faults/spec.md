# Feature Specification: MatrixRTC protocol faults

**Created**: 2026-08-07
**Status**: Complete. Every task closed; neither fault reproduces on this stack, with
Element Call alone or with Elementium as a participant. What remains is the difference
between this stack and the user's — delegated auth, a hosted SFU, federation

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

## Decision — 2026-08-08: what to do when a key misses the `useKeyDelay` window (T012)

The fault T008 left standing: a sender distributes a new key, waits `useKeyDelay`
(5000ms), and starts encrypting with it whether or not the key arrived. A peer it did
not reach hears nothing until it does.

**Decision: change nothing in the sending schedule, and rely on the receive path
already retrying.** The reasoning, option by option, because three of the four are
worse than they look.

**Raise `useKeyDelay`.** The delay is a sender-side constant, so raising it is safe for
interoperability — peers adopt whatever index arrives, whenever it arrives. It is not
safe for the property the rotation exists to provide: the window is exactly how long a
participant who has just left can still decrypt the call. Doubling the delay doubles
that. Trading forward secrecy for a shorter silence is the wrong trade to make silently,
and this fault has never been measured on the stack where it was reported.

**Defer adoption until distribution is acknowledged.** There is nothing to wait for.
Matrix to-device delivery gives the sender no receipt, so an acknowledgement would have
to be an application-level message that no other client sends — it would work only
between two Elementium instances, which is the one case that is not the reported fault.

**Re-send at a fresh index on suspicion.** This makes it worse. A new index starts the
same race over again, and it does so at exactly the moment the evidence says the first
one was lost.

**Do nothing on the sender, because the receiver already recovers.** Two facts make this
tolerable rather than resigned:

- livekit-client stops retrying an index after `failureTolerance` (10) failures, but
  installing a key at that index clears the count, so a late key revives it. The gap is
  a gap, not a permanent loss.
- **Our own decrypt path has no such latch at all.** `elementium-e2ee` tries every frame
  against every key it holds; the only counter it keeps is for throttling the failure
  log. So "I cannot hear others" self-heals the moment the key lands, without a
  rejoin — and it is the reported symptom whose recovery we actually control.

What is left is "others cannot hear me" during the gap, and that is governed entirely by
the peer's client. We cannot fix it from here without breaking interoperability.

So the useful work is measurement, not mitigation, and T001 already built it: the
interval between a key being installed and the first frame it decrypts. If that interval
is small on the user's homeserver, this is not their fault and the search continues
elsewhere. If it is seconds, raising the delay becomes a trade worth putting to them
with a number attached rather than a guess.

### The number, on this stack

From the 2026-08-08 app-join run:

```
E2EE key decrypted its first frame  @tester2:...:HTYDMWBJWO  index 1  waited_ms 1325
E2EE key decrypted its first frame  @tester3:...:YOKREAJPDQ  index 1  waited_ms 1325
```

**1.3 seconds**, against a 5-second budget. Nothing came close to missing the window,
which is the third independent reason not to change the schedule here.

The same run shows nothing dying inside `RTCEncryptionManager` either: 29 keys received
by the bridge, 29 forwarded to the native backend, 29 installed. No parked key, no key
judged out of order. The failure modes described under "What is already known" are real
in the source and simply do not occur on this stack.

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
