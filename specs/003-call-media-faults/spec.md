# Feature Specification: Call media faults

**Created**: 2026-08-07
**Status**: Draft — evidence gathered, root causes partly identified
**Input**: "I still cannot talk via this at all. Also I cannot see other's web cams
and streams yet."

## Why this exists

Three symptoms were reported. Log analysis of real calls on 2026-08-07 shows they
are not three faults:

| Symptom | Cause |
|---|---|
| Others cannot hear me | E2EE (probably), plus a stale transport |
| I cannot hear others | E2EE decrypt failure |
| I cannot see others' cameras | E2EE decrypt failure |

## Evidence

All from `elementium.log.2026-08-07`, real calls with a second participant.

### Inbound media is received and discarded

```
331  E2EE dropping inbound video frame: decrypt failed
137  E2EE dropping inbound audio frame: decrypt failed
```

with

```json
{"mid":"4","reason":"decryption failed: tried 3 participants, none could decrypt"}
```

The frames arrive. The keys are installed — for the local device, for a second
local device, and for the remote participant `@lace:...`. Decryption is attempted
against all of them and fails against all of them.

**This is not a missing-key problem.** Keys are present and do not work, which
points at key derivation, the frame's cipher-text framing, or the key index,
rather than at distribution.

Observed key material: `key_len: 16` (128-bit), `key_index` values of 0, 1 and 4.

### Outbound audio stops when the peer connection is replaced

Audio does transmit initially — 717 frames sent, input peak 0.046, so the
microphone works and the path is real. Then:

```
06:16:21  captured 750  encoded 717  sent 717            <- working
06:16:24  Starting audio capture
06:16:24  Audio capture attached to a live call on startup
06:16:24  peer connection removed from engine
06:16:24  peer connection closed                          <- the one just inherited
06:16:29  captured 250  encoded 250  sent 0  dropped_channel_full 250
```

`getUserMedia` is re-called mid-call, the new capture pipeline inherits the
sender for the current peer connection, and that connection is closed three
milliseconds later. Nothing invalidates the inherited sender, so every later
frame is encoded and thrown away.

`create_offer` is the only place a pipeline is attached, and no further offer was
made in that session, so the pipeline never recovered.

### The statistic that hid it

A failed `try_send` is counted as `dropped_channel_full` whether the receiver is
**full** or **gone**. Those are opposite problems -- one is back-pressure from a
slow consumer, the other is a dead connection -- and reporting them as one number
is why this looked like congestion.

### 2026-08-07: a key index, once doubted, is never trusted again

Two defects, one hiding the other. Both verified against livekit's own source and
reproduced in `frontend/tests/browser/receive-path.spec.ts`.

**We announced encrypted tracks as unencrypted.** `AddTrackRequest.encryption` was
left at its default, `NONE`, while the transport encrypted every frame. Fixed.

That is not a cosmetic mismatch, because subscribers act on it:

```js
room.on(TrackPublished, pub =>
  setParticipantCryptorEnabled(pub.trackInfo.encryption !== NONE, identity))
...
if (!this.isEnabled() || byteLength === 0) return controller.enqueue(encodedFrame);
```

A participant announced as `NONE` has their cryptor switched off and every frame is
passed through undecrypted. It produced no visible damage in our tests because
livekit's frame layout leaves the Opus TOC byte in the clear: each frame reached
NetEq with a valid header and a ciphertext payload, so the right number of samples
arrived on time with nothing to conceal. The receiver reported a perfectly healthy
stream of noise — which is the symptom, described from the other side.

**And underneath it, the reason a bad moment does not pass.** With the declaration
corrected the frames really are decrypted, and a test that used to pass now cannot:

```js
hasInvalidKeyAtIndex(i) -> failureTolerance >= 0 && failureCounts[i] > tolerance
decrypt: if (this.keys.hasInvalidKeyAtIndex(keyIndex)) return;   // dropped
```

The drop happens *before* decryption is attempted, so the successful decryption that
would clear the count can never occur.

There is one escape, and it matters: installing a key at that index clears the
count.

```js
setKey(material, keyIndex, updateCurrentKeyIndex = true) {
  await this.setKeyFromMaterial(material, keyIndex, null, updateCurrentKeyIndex);
  if (updateCurrentKeyIndex) this.resetKeyStatus(keyIndex);
}
```

Element Call always passes a key index, and livekit forwards that as
`updateCurrentKeyIndex: true`, so a key arriving late does revive the index it
belongs to. The latch is therefore *not* the permanent killer it first appears to
be, and an earlier draft of this section said it was — wrongly, from reading
`setKeySet` instead of the `setKey` that wraps it.

What survives is narrower and still real: the index stays dead for any stream that
keeps failing after the last key install at that index. A wrong AAD, a header-size
mismatch, a stale key we never correct — anything that fails persistently rather
than transiently — stops being retried after a fifth of a second and produces
silence with no error on either side.

So the latch is a severity amplifier, not the root cause. **The root cause found
here is the encryption declaration**, and its symptom — a clean, well-paced,
zero-concealment stream of noise — is exactly what the user reports.

### Which path this actually fixes

Not the one the application runs, and that must not be glossed over.

`AddTrackRequest` is built by `LiveKitRoom`, which is used by the Rust harness and
the `publish_test_tone` example. The application takes the shim path — our
`RTCPeerConnection` beneath the real livekit-client — and there livekit-client
builds the request itself, with `encryptionType` set to `GCM` by `setE2EEEnabled`.
So the live path already declared its tracks correctly.

What the fix repairs is the instrument, not the patient. Every "healthy" verdict the
browser suite produced was taken through a publisher that announced NONE, so the
receiver never decrypted anything and graded noise as clean audio. Those results
are void, and the suite only became capable of detecting this class of fault with
this change in place.

The user-facing audio fault therefore remains open. What has changed is that the
test that would show it now can.

Eleven undecryptable frames reach that state. At 50 frames per second that is a
fifth of a second, and it is exactly what a peer sees while our key is still in
flight to them: at every call start, and at every rotation.

Element Call is subject to it. Its provider is
`super({ratchetWindowSize: 10, keyringSize: 256})` on `BaseKeyProvider`, which never
sets `failureTolerance`, so it takes the default of 10. Only `ExternalE2EEKeyProvider`
sets -1, and Element Call does not use it.

This is a far better fit for the reported faults than any codec explanation: it is
per-participant, it survives for the life of a key, it produces clean packet counts
and no errors on either side, and it is triggered by exactly the events the user
reports — joining, and someone else joining or leaving.

What follows from it is that we must not publish frames encrypted with a key our
peers cannot yet hold. See feature 004 T008 (`useKeyDelay`), which is now the
highest-value open task in either feature.


## Finding — 2026-08-07: with Elementium in a real call, both directions work

The first measurement of Elementium as a participant, against the local stack, with two
real Element Web clients in the same call. Driven headlessly (`just call-peers` and
`just app-join`), both sides instrumented at once.

| Measured at | Result |
|---|---|
| Elementium outbound audio | 12,000 captured, 12,000 encoded, **12,000 sent**, 0 skipped, 48 kbps |
| The peers, hearing it | **2/2 usable streams each**, and they hold `@tester1`'s key |
| Elementium inbound audio | **1,219 Opus frames decoded** from the two peers |
| Elementium inbound video | 4 `MediaAdded` (2 audio, 2 video), one track delivering frames |

So audio works in both directions, E2EE keys are exchanged both ways, and neither
reported fault reproduces here. That is worth stating plainly because it contradicts
the assumption this feature started from: on this stack, with this homeserver,
Elementium can hear and be heard.

### Correction: the second remote camera does work

An earlier draft of this section reported that with two remote publishers only the
first delivered video, on the strength of one run. Two later runs, after the harness
was fixed, show both tracks delivering frames:

```
pc-18c9c16eb644c995-2   has_frame=True   12 samples
pc-18c9c16eb644c995-3   has_frame=True   25 samples
video-18c9c16efec7679c  has_frame=True   25 samples   (the local self-view)
```

with 1,028 inbound Opus frames in the same run. So remote video works for more than
one participant, and the per-track display fix is doing exactly what it was written
for.

What the single earlier observation was is not established. The most likely reading is
a peer that had joined but not yet published when the sample was taken — the frame
counter is global and throttled to every 300th call, so a track that starts late is
easy to miss entirely. The fetch loop now reports when it stops and counts failures
instead of swallowing them, so a recurrence will say which it is rather than leaving
it to be inferred.

The original observation, kept because the reasoning from it was wrong twice over:

### What one run showed, and did not mean

Two remote participants published video. The frontend asked for both display slots and
only one ever had a frame:

```
pc-18c9a22d841cf3cf-2   has_frame=True    49 calls
pc-18c9a22d841cf3cf-3   has_frame=False    1 call
```

The second track was requested **once** and never polled again. That is a real defect,
and it matches "I cannot see other people's webcams" in the shape a user would meet it:
in a call with two other people, one picture appears.

It also confirms the per-track display fix from earlier today is doing its job -- the
two ids differ by mid, where before both were `{pc_id}-video` and would have
overwritten each other in one slot. The remaining fault is downstream of that: the
frontend stops asking for the second track.

### What this leaves

The reported faults are real and were seen against a *different* environment: a
homeserver using MSC2965 delegated authentication (`matrix-auth.`), a hosted SFU, and
federation. None of those exist locally. Which of those differences matters is the open
question -- and the local stack cannot answer it, but it can now serve as the control,
which it could not before today.

## User Scenarios

### US1 (P1) — Participants can hear each other

A participant speaks; every other participant hears them, and continues to after
a device change, mute/unmute, or renegotiation mid-call.

**Independent test**: join with a second client, speak, confirm audio arrives;
change input device mid-call; confirm audio still arrives.

### US2 (P1) — Participants can see each other

Remote video is decoded and displayed.

**Independent test**: join with a second client publishing video; confirm the
picture appears and updates.

### US3 (P2) — A dead transport is visible and recovered from

A pipeline whose peer connection has gone reports it distinctly from congestion
and is re-attached when a new connection exists.

**Independent test**: force a renegotiation mid-call; confirm audio resumes
without restarting the app, and that logs distinguish "closed" from "full".

## Success Criteria

- **SC1**: With two participants in a call, each hears the other for the whole
  call, including across a mid-call device change.
- **SC2**: With two participants publishing video, each sees the other.
- **SC3**: Inbound decrypt failures are zero in a healthy call; any that occur
  name the participant and key index they were tried against.
- **SC4**: No frame is ever encoded into a closed channel more than briefly; the
  condition is reported as closed, not as full.

## Assumptions

- The remote participant is a working reference client (Element Web), so the wire
  format it produces is correct and ours must match it.
- E2EE is expected to be on. Disabling it is a diagnostic step, not a fix.
