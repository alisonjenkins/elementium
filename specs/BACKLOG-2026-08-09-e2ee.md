# E2EE audit, 2026-08-09

Three read-only audits — the native crate against the LiveKit frame protocol, the JS key
bridge against livekit-client's worker message protocol, and the media-pipeline integration
— prompted by a call in which neither participant could decrypt the other.

**The frame crypto is sound.** AES-GCM with a 12-byte IV and a fail-closed counter, the
two-byte trailer, HKDF-SHA256 with the `LKFrameEncryptionKey` salt and 128 zero bytes of
info, VP8 10/3 and audio 1 clear-byte framing, H.264 slice-header parsing including two
deliberately reproduced upstream quirks, 256 key slots, server-injected frames recognised
before any decrypt is attempted. Several of these are pinned by vectors computed
independently of the implementation, which is why this section is short.

**Ratcheting being off is not a fault.** `default_ratchet_window()` is 0, and Element Call's
shipped bundle configures `ratchetWindowSize: 0` with `keyringSize: 256` — our defaults
match the peer we actually talk to. Recorded here because "ratcheting is disabled" reads
like a bug and is not one.

## What the call showed

| | |
|---|---|
| our key | index 0, one fingerprint, 11 forwards, never rotated |
| her key | arrived once, at index 6, 92 seconds after we joined |
| her frames | arrived from 12:00:02 at index 5 — a key we never held |
| outcome | every inbound frame dropped until 12:01:22, then audio decoded normally |
| far end | requested a keyframe roughly once a second for the whole call |

Inbound is not broken, it is starved: the moment a key we could use arrived, decoding
worked and kept working. The far end's steady keyframe requests are the mirror image —
a receiver holding RTP it cannot decrypt.

## Open

- [ ] **E1. 897 dropped frames that nobody would have heard, reported as if they were lost
  speech.** Traced, and it is not what it looked like. Identity parsing is not slow: it is
  set 111ms after the SFU WebSocket opens. The socket simply does not open until 12:00:01,
  while capture starts at 11:59:45 and Element Call attaches the pipelines to a peer
  connection of its own at 11:59:48 — one created before the session exists and which never
  carried the call. Every one of those 897 frames was written towards that connection.
  Nothing was lost.

  What is wrong is the reporting: 897 unthrottled WARN lines for a condition that is normal
  and expected during join, on a path whose sibling failures are throttled one-in-500. It
  buries the encrypt failures that do matter and it reads, to anyone opening the log, as
  thirteen seconds of the user's voice going missing. Throttle it, and say plainly that this
  is pre-join. The startup hold is a separate question and, given the above, probably not
  worth lengthening.

- [ ] **E2. A key stored under the local identity would be used to decrypt inbound frames.**
  `decrypt_frame_any` collects every participant in the ring and tries each, with no filter
  excluding the local identity, and `e2ee_set_key` accepts any caller-supplied participant
  string without checking it against the identity we set. Nothing in the IPC boundary stops
  a JS-side mistake from writing a remote key under our own id — where it would silently
  become the key we encrypt with. No evidence it has happened; the point is that nothing
  would stop it or say so.

- [ ] **E3. An environment variable can widen the plaintext header at runtime.**
  `h264::extra_clear_bytes()` reads `ELEMENTIUM_H264_CLEAR_EXTRA` on every frame — a hot-path
  `env::var` and parse, and, more to the point, a live knob that leaves more of each frame
  unencrypted and breaks wire compatibility with a real LiveKit peer. A debug aid should not
  be reachable in a release build, and certainly not per frame.

- [ ] **E4. The key bridge drops every worker message except three.** `init`, `setKey` and
  `setSifTrailer` are forwarded; `removeTransform`, `setKeyIndex`, `ratchetRequest`,
  `encodeOptions` and `enableKeyManagement` fall into a branch that logs the kind once and
  returns. Two of those matter: `removeTransform` means stop encrypting a track that has
  gone away, and a key index that advances without new key material is invisible to us.
  Small, and it removes a class of silent divergence.

- [ ] **E5. `auto_ratchet` is dead config.** Deserialized, stored, never read; only
  `ratchet_window_size > 0` gates anything. It reads like a toggle and is not one. Also
  `IV_SIZE`'s doc comment describes `MAX_KEYS` — a copy-paste that will mislead the next
  person to touch the trailer.

- [ ] **E6. We cannot yet prove where key delivery fails.** Her keys 0–5 never reached us and
  ours may never have reached her; the one to-device batch we sent went out at 11:59:50,
  before she was there. That is Element Call and matrix-js-sdk territory rather than ours,
  but we currently cannot tell "never sent" from "sent and lost" from "arrived and dropped".
  Wanted: a periodic reconciliation of the key indices seen on inbound frames against the
  indices we hold, per participant, so the next call answers this instead of suggesting it.
  No key material, only participants, indices and counts.

## Checked and not a fault

- **Frame format, key derivation, codec framing, key ring size, SIF handling** — see above.
- **`Received an unexpected encrypted to-device event`.** Fires at 12:01:22, the same instant
  her key arrives and is successfully forwarded: the SDK does not recognise
  `io.element.call.encryption_keys` as a known to-device type. Delivery worked, it was late.
  Flagged as suspicious earlier in the session; it is not.
- **Ratcheting disabled.** Matches Element Call's own configuration.
