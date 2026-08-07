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
