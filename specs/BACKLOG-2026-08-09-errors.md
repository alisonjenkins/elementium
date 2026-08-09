# Error handling, 2026-08-09

Opened after a single line — `ok_or("No pending offer to match answer")?` — froze the
signalling state of every call, rebuilt the connection every fifteen seconds, and was
misattributed to livekit-client for hours because it carried no variant, no source and no
log line. Constitution Principle I was ratified the same day and this is the debt it names.

## The scale

Nine error enums exist. Between them they carry **28 variants whose payload is a `String`**,
and the whole codebase contains **three `#[source]` annotations**. A caller cannot match on
any of those 28, and none of them can be walked back to a cause.

| enum | String variants | sources |
|---|---:|---:|
| `WebRtcError` (elementium-webrtc/src/error.rs) | 7 | 0 |
| `ElementiumError` (elementium-types) | 5 | 1 |
| `SecretStoreError` (elementium-keyring) | 4 | 2 |
| `PipewireError`, `ShareError` | 3 each | 0 |
| `CaptureError`, `OpusError` | 2 each | 0 |
| `CameraError`, `E2eeError` | 1 each | 0 |

`SignalError` was the tenth and is now done: variants carry `url::ParseError` and the
tungstenite error, and two variants that were never constructed are gone.

## The migration — complete

Sequenced so the tree builds at every step. The order matters: deleting the escape hatch
first would break roughly thirty call sites in one unatomic pile.

- [x] **X1. DONE. The seven per-surface enums for `peer_connection.rs`.** `CreateOfferError`,
  `CreateAnswerError`, `SetRemoteDescriptionError`, `AddIceCandidateError`,
  `MediaWriteError` (shared by `write_audio`/`write_video` — every caller drops the frame
  and continues, which is the only condition under which Principle I permits sharing),
  `DataChannelWriteError`, `IoLoopError` (shared by the io-loop trio). Add each as a
  `#[from]` variant on `WebRtcError` so aggregating callers keep one type.

  `SetRemoteDescriptionError` gets **no** `NoPendingOffer` variant: that case is now `Ok`,
  and reintroducing it as an error would restore the outage. Split the function into
  offer/answer helpers while converting — it is near the 100-line limit and the two arms
  share nothing.

- [x] **X2. DONE, narrowly and on evidence.** The IPC boundary. Tauri v2 accepts any `Serialize` error; the
  `Result<T, String>` constraint is our plumbing, not the framework's. Keep the string but
  make it a JSON envelope with a stable snake_case `code`, and route every command through
  one helper that walks `source()` into a single `tracing::error!` before flattening — so no
  site can forget to log. The shim then maps codes to the DOMException names livekit
  actually branches on. Conditions that must not reject a promise are fixed in Rust as `Ok`,
  not papered over here; the boundary cannot unfreeze a state machine.

- [x] **X3. DONE — the escape hatch is deleted. Delete `From<String>`, `From<&str>`, `Other` and `other()` from `WebRtcError`.**
  Last, and only once nothing depends on them. This is the commit that makes the whole class
  of regression impossible, because the compiler then enforces it.

- [x] **X4. DONE. The five stringly sites blocked on `error.rs`.** `room.rs:236`, `room.rs:292`,
  `room.rs:621` (all `WebRtcError::Signaling(format!(...))`), and `transport.rs:89-115`
  (four bind/addr `format!`s) plus `transport.rs:211` (`ok_or_else(WebRtcError::Sdp(...))`).
  Deliberately left unconverted because `error.rs` was owned by another change in flight.

- [x] **X5. DONE, and it found far more than the table predicted — see X11. Audit the remaining eight enums** against Principle I. The table above is the
  worklist. `WebRtcError` is the largest and is handled by X1–X3; the rest are smaller and
  independent of each other.

## Open — conditions modelled as failures

The most valuable category, because today's outage was one of these. Each needs a decision,
not a reflex: some are defensible defensive coding, and turning a real fault into an `Ok` is
the opposite mistake.

- [x] **X6. RESOLVED, opposite to expectation. `set_remote_description` returning `Ok(None)` is treated as an error in two
  places** — `room.rs:990-996` and `transport.rs:201-214`. `None` means no answer was
  produced. Establish from `peer_connection.rs` when that is legitimate before changing
  either. Flagged with unusual seriousness: the bug that started this file was precisely a
  legitimate `Ok(None)`-shaped condition treated as a failure, and here the same judgement
  is duplicated across two files.

- [x] **X7. DONE — fixed by construction rather than by error. `room.rs:299-307`: "signal receiver already taken"** is returned as
  `ChannelClosed` immediately after `SignalClient::connect()` returns a fresh client, where
  nothing else could have taken it. An invariant that cannot be violated, modelled as a
  runtime error.

- [x] **X8. DECIDED — kept. `SignalError::SchemeRewriteRejected` is unreachable.** `Url::set_scheme` can only
  fail for the http↔ws and https↔wss swaps `build_ws_url` performs if the URL has no host,
  which would already have failed parsing. Verified experimentally against the `url` crate,
  not by inspection. Keep as defensive coding or delete — but decide.

## Open — silence

- [x] **X9. DONE. Teardown errors are discarded with `let _ =` and no log** — `signaling.rs`'s
  `disconnect()` and the writer loop's final `close()`, and the same pattern in
  `transport.rs`'s shutdown. Judged intentional (documented shutdown races) rather than a
  violation, but nothing observes whether `Leave` ever reached the SFU, and a call the server
  thinks is still running has visible consequences for the next one.

- [ ] **X11. The second audit found what the first could not, and the difference is the
  method.** The first pass audited the eight named error enums; this one audited every
  function that can fail. That is how the entire `VideoEncoder` trait and its four
  implementations -- 24 stringly sites on the encode hot path -- stayed invisible through a
  pass that reported itself complete. All now fixed, but the lesson generalises: audit the
  behaviour, not the abstractions someone already built to describe it.

- [ ] **X12. `ElementiumError::Backend` does not distinguish Wayland from X11.** Only prose
  in its `description` does. Judged not worth splitting, because exactly one backend is
  compiled per platform so the ambiguity is theoretical. Recorded so the judgement is
  visible rather than an oversight, and so it can be revisited if a build ever carries both.

- [ ] **X13. Two conditions in `elementium-keyring` are fallible only to satisfy lints.**
  `file_backend.rs:91,124` construct an AES cipher from a key whose length is fixed at 32
  bytes by construction, and `:134` checks a sum of small constants for `usize` overflow.
  Both defensible defensive coding against `arithmetic_side_effects` and the library's own
  `Result`. Flagged rather than converted, because turning a real guard into an `Ok` is the
  opposite of the mistake this file exists to record.

## Process

- [ ] **X10. Sub-agents must not share a working tree.** Four agents ran concurrently in one
  checkout today. One of them ran `git stash` while diagnosing a build failure, capturing two
  other agents' in-flight edits; one agent lost its first pass entirely and reapplied it from
  scratch, and only noticed because it re-checked `git status`. Nothing was lost in the end,
  but that was luck. Use `isolation: "worktree"` for concurrent agents, or partition strictly
  and forbid any git command that touches the index.
