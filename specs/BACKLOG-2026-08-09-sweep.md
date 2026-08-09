# Protocol and TypeScript-integration sweep, 2026-08-09

Five read-only audits: the `RTCPeerConnection` shim, the LiveKit signalling interception,
the widget-API bridge, the media-device shim, and the Rust engine against what the shim
promises its callers.

The failure class throughout is one this project has now produced five times: **a stub that
returns something plausible.** `addTrack` returned a fabricated sender and recorded nothing.
`getDisplayMedia` returned a canvas nobody painted. A frame arriving as an array of numbers
was skipped with no error, no counter and no log line. `negotiationneeded` was never fired.
Each was invisible at the point of the fault and expensive somewhere else. What follows is
mostly more of the same, found deliberately this time.

Two audits came back clean, which is worth as much as the findings:

- **`get_transport_stats`** — every field is a real str0m measurement, and unmeasured values
  are `None` rather than zero-filled. Nothing invented.
- **The widget key transport** — `io.element.call.encryption_keys` goes over the standard,
  pre-approved `send_to_device` action, and nothing in this repository intercepts widget
  postMessage traffic. The 92-second key delay is not ours.

## Findings — all closed, 2026-08-09

- [x] **S1. FIXED. The page cannot learn that the connection broke.** HIGH. `connectionState` is
  assigned `"connected"` by a hardcoded line in the shim and never changes again:
  `PcEvent::ConnectionStateChange` has zero producers in Rust. Element Call drives its
  reconnect UI from that property. Underneath it, ICE `failed` and `closed` are structurally
  unreachable — str0m's enum has no such variants and ours are dead code with no producer —
  so nothing distinguishes a blip from a dead transport. And the recovery machinery that
  would act on it, `ICE_DISCONNECT_GRACE` and `ice_disconnect_expired()`, has no caller
  anywhere outside its own definition and tests. `restart_ice` works end to end but only if
  something asks, and nothing does.

  This also cost us diagnostic honesty: "ICE never reported failed" was used as evidence in
  the reconnect backlog, and it could not have reported anything else.

- [x] **S2. FIXED. Bitrate and simulcast control is inert.** HIGH. `sender.setParameters` resolves
  successfully and stores nothing; `getParameters` returns the encodings frozen at creation,
  so it never reflects a later `setParameters`. livekit-client drives layer switching,
  bitrate and `degradationPreference` through these — 15 call sites in the shipped bundle.
  Every adaptation the client believes it made is discarded. A strong candidate for "the
  video is very pixelated", and it would explain why quality never recovers on its own.

- [x] **S3. FIXED. Two useful events are hidden in 898 lines of noise.** MEDIUM, downgraded from
  HIGH after checking what the noise actually is. I first read `SenderFeedback` as the far
  end's receiver reports on our outbound media — loss, jitter, round-trip — which would have
  made it the measurement we most lacked. It is not: `poll_sender_feedback` iterates
  `streams_rx`, so these are RTCP *Sender Reports from the remote about media we receive*.
  Outbound loss and round-trip already reach us through `MediaEgressStats`, which is
  handled, and whose `loss` is deliberately `None` rather than `0.0` when no report arrived.

  What is left is still worth doing. 898 of these in two minutes flood the "Unhandled str0m
  event" bucket, and two events that do matter are in there with them: `StreamPaused`, which
  is a freeze signal, and `ChannelBufferedAmountLow`, which has a consumer already written —
  `write_data_channel`'s doc tells callers to retry when there is room, and nothing ever
  tells them there is. A bucket that logs 898 things nobody wants is a bucket nobody reads.

- [x] **S4. FIXED. Remote tracks have no transceiver and no receiver.** MEDIUM. `getReceivers()`
  returns `[]`; no transceiver is created for tracks the SFU pushes at us; `RTCTrackEvent`
  carries `receiver` and `transceiver` as bare `{}`. livekit-client looks tracks up by id
  and by mid through exactly these — 19 call sites — and finds nothing. Separately
  `transceiver.currentDirection` is permanently `null`, and `getLocalTracks()` filters on it
  being `sendonly` or `sendrecv`, so that list is always empty.

- [x] **S5. FIXED. livekit cannot confirm a device switch it asked for.** MEDIUM. It checks
  `track.getSettings().deviceId === <requested>`. Our tracks are canvas- and
  AudioContext-backed, so `getSettings()` reports the synthetic track's id, never the native
  one — the check reads false even when Rust switched correctly.

- [x] **S6. MADE HONEST, not fixed. `applyConstraints` and `getCapabilities` never reach Rust.** MEDIUM. Neither is
  overridden, so both run against the synthetic track: a mid-call resolution change resolves
  successfully and alters nothing, and reported capabilities describe a canvas rather than
  the camera. A caller clamping its request to what we advertise is working from fiction.

  No native command exists to reconfigure a running capture pipeline, and inventing one was
  out of scope, so both now say loudly what they could not do and name the constraints
  asked for. The capability is still missing; it is no longer silent.

- [x] **S7. FIXED. A cloned track is an unwired track.** MEDIUM. `stop()` and the `enabled` setter
  are wired per instance, so `.clone()` yields an object whose `stop()` releases no native
  pipeline, leaks the preview loop, and whose mute reaches nothing. matrix-js-sdk's
  `CallFeed.clone()` is present in the bundle. This is the preview-loop leak again, reached
  by a different door.

- [x] **S8. FIXED. A comment claims a check that does not exist.** MEDIUM.
  `set_local_description` is a documented no-op — reasonable, since str0m applied the SDP
  when it generated it — but its comment says the page's SDP is "checked rather than
  assumed" to match. The body logs and returns `Ok(())`. Either check it or stop saying so;
  a comment asserting a safety property nobody implemented is worse than no comment.

- [x] **S9. FIXED. A device-selection landmine that has not gone off.** LOW. matrix-js-sdk builds
  `deviceId` as `{exact: ...}`; the shim casts it straight through to Rust, which expects a
  string, so IPC deserialization would fail and throw `NotAllowedError` for the whole
  `getUserMedia`. Element Call uses a plain string today, which is the only reason this is
  theoretical.

- [x] **S10. MADE AUDIBLE. The by-kind fallback is still armed.** LOW. An unrecognised `source` string
  from JS falls back to `default_key_for(kind)` — the exact rule that put the camera on the
  screen-share m-line. Deliberate, for forward compatibility with an older frontend, but it
  means a track type this list has not caught up with collides silently rather than failing.

  Kept, because the reasoning for it holds. It is no longer silent: a track taking the
  fallback names the source that was not recognised, so the fix next time is to add the
  name rather than to spend a call and a screenshot rediscovering the collision.

- [x] **S11. MADE AUDIBLE. Non-PipeWire camera selection is silently ignored.** LOW. On the nokhwa
  fallback path device ids cannot be resolved back to a node, and the capture path takes the
  first source that works. The code says so in a comment; the user is not told.

## Upstream, recorded not fixed

- **`io.element.join` and `io.element.device_mute` are refused** by Element Web's widget
  layer. A driver that would accept both exists in the shipped bundle, attaching its
  listeners after an `await`; every refusal in the log falls in the first three seconds and
  none after, which fits that race. Element Web is unmodified v1.12.25 against Element Call
  `embedded-v0.22.0`. The call joins regardless.
- **Key delivery took 92 seconds** — see `BACKLOG-2026-08-09-e2ee.md`. Not the widget
  bridge, not the native crypto, not the key bridge.
