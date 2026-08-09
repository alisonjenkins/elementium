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

**Ratcheting is on, with a window of 10.** Worth stating carefully, because I got it wrong
once in this session by reading the shipped bundle instead of the running app. The bundle
contains `ratchetWindowSize` values of 0, 8 and 10, so grepping it proves nothing about
which is used; the log settles it — `E2EE context initialized ratchet_window_size=10`. Our
`default_ratchet_window()` of 0 therefore only applies when no options arrive at all, which
is not the case in a real call. `keyringSize: 256` does match our 256 slots.

A consequence: a frame whose key index we hold no material for cannot be ratcheted to, since
ratcheting walks forward from the material stored at that same index. Her index-5 frames
were unreachable no matter the window.

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

## Findings — all closed, 2026-08-09

- [x] **E1. FIXED. 897 dropped frames that nobody would have heard, reported as if they were lost
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

- [x] **E2. FIXED. A key stored under the local identity would be used to decrypt inbound frames.**
  `decrypt_frame_any` collects every participant in the ring and tries each, with no filter
  excluding the local identity, and `e2ee_set_key` accepts any caller-supplied participant
  string without checking it against the identity we set. Nothing in the IPC boundary stops
  a JS-side mistake from writing a remote key under our own id — where it would silently
  become the key we encrypt with. No evidence it has happened; the point is that nothing
  would stop it or say so.

- [x] **E3. FIXED. An environment variable can widen the plaintext header at runtime.**
  `h264::extra_clear_bytes()` reads `ELEMENTIUM_H264_CLEAR_EXTRA` on every frame — a hot-path
  `env::var` and parse, and, more to the point, a live knob that leaves more of each frame
  unencrypted and breaks wire compatibility with a real LiveKit peer. A debug aid should not
  be reachable in a release build, and certainly not per frame.

- [x] **E4. FIXED. The key bridge drops every worker message except three.** `init`, `setKey` and
  `setSifTrailer` are forwarded; `removeTransform`, `setKeyIndex`, `ratchetRequest`,
  `encodeOptions` and `enableKeyManagement` fall into a branch that logs the kind once and
  returns. Two of those matter: `removeTransform` means stop encrypting a track that has
  gone away, and a key index that advances without new key material is invisible to us.
  Small, and it removes a class of silent divergence.

- [x] **E5. FIXED. `auto_ratchet` is dead config.** Deserialized, stored, never read; only
  `ratchet_window_size > 0` gates anything. It reads like a toggle and is not one. Also
  `IV_SIZE`'s doc comment describes `MAX_KEYS` — a copy-paste that will mislead the next
  person to touch the trailer.

- [x] **E6. ANSWERED — the reconciliation already existed, and it is decisive.** I proposed
  building this before checking whether it was there. `decrypt_frame_any` already logs the
  frame's key index beside a full inventory of every key held, precisely so that "their key
  never reached us" and "we hold the wrong key for that index" can be told apart. The call
  says:

      frame_key_index: 5   keys_held: @alijenkins:…:xDoYzMrXlX@0:bab66d82

  One key, ours. Nothing for `@lace` at any index. Since the bridge forwards every `setKey`
  it is given, Element Call never handed us one until 12:01:22. So the fault is not in the
  native crypto, not in the key ring, and not in the bridge: **no key for the remote
  participant was ever delivered to the application** for the first 92 seconds.

  That moves the question to Element Call's key distribution over Matrix to-device, which is
  the next thing to look at, and out of this file.

## Checked and not a fault

- **Frame format, key derivation, codec framing, key ring size, SIF handling** — see above.
- **`Received an unexpected encrypted to-device event`.** Fires at 12:01:22, the same instant
  her key arrives and is successfully forwarded: the SDK does not recognise
  `io.element.call.encryption_keys` as a known to-device type. Delivery worked, it was late.
  Flagged as suspicious earlier in the session; it is not.
- **A ratchet window of 0 in the shipped bundle.** Not what runs: Element Call sends 10.
