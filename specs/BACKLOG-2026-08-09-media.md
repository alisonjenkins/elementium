# Outbound media, 2026-08-09

Opened from a live call in which the far end saw one pixelated frame that never updated,
heard nothing after a mid-call device change, and in which four fifths of the room's video
never decrypted. Each of these is a separate fault with separate evidence; they are
collected here because they were all found in the same session.

## Open

- [ ] **M2. The far end sees one keyframe and then a frozen picture.** Confirmed from a
  second client on the same machine: video *into* Elementium renders smoothly, video *out*
  of it does not. The receiver sent 215 PLIs across three and a half minutes -- roughly one
  a second, sustained -- and we answered 192 of them with a keyframe. A receiver that keeps
  asking is a receiver that decoded the keyframe and could not decode what followed.

  Ruled out on evidence, so the next investigation does not repeat them:
  - Not `write_video` failing: zero failures in the whole session.
  - Not E2EE encryption failing: three dropped frames, all in the first second.
  - Not the E2EE VP8 framing rule: `framing()` implements livekit's 10-bytes-on-a-keyframe,
    3-on-a-delta correctly, and encryption happens on the whole encoded frame before
    packetisation, which is where livekit's browser side does it too.
  - Not the encode pacer (fixed today): dropping frames *before* the encoder cannot break
    the reference chain, because VP8 predicts from the last frame the encoder actually saw.

  Untested and next: whether the bitrate we transmit matches what we configure. The encoder
  was created at 2764 kbps and `setParameters` asked for 1700, but the measured rate was
  ~336 kbps -- about a fifth of target. A picture starved to that degree at 720p is
  pixelated, which is the other half of the user's description.

- [ ] **M3. A mid-call device change can leave the microphone attached to nothing.** When the
  camera became available mid-call, livekit unpublished both tracks, closed the publisher
  connection, and logged `could not createOffer with closed peer connection`. It never built
  a replacement. For the following 45 seconds the microphone captured into a void:
  `captured_frames: 2250, encoded_frames: 1, sent_frames: 0, skipped_not_connected: 2249`.

  The inheritance path that exists for exactly this (`stop_pipeline_inheriting_connection`
  into `connection_for_new_pipeline`) found nothing to inherit, because the connection it
  would have inherited was the one being closed, and `sfu_media_tx` was empty. Nothing then
  adopted the orphan: `adopt_idle_pipelines` runs on connection *creation*, and no
  connection was created.

  A capture pipeline that is producing frames nobody will ever receive should not be able to
  stay in that state silently. Whatever the fix, `skipped_not_connected` climbing past a
  threshold deserves a warning in its own right.

- [ ] **M5. Our key is distributed once, at join, and never again -- which is what freezes
  the far end mid-call.** The strongest lead in this file, and the likely cause of both "they
  cannot see me" and "they cannot hear me".

  Every `setKey` that reached the backend in one call:

  | time | participant | index |
  |---|---|---:|
  | 18:43:52 | `xDoYzMrXlX` (us) | 0 |
  | 18:45:23 | `Ji1WSmp16W` (our own browser, second device) | 11 |
  | 18:45:25 | `CJu6o8gDTf` | 11 |
  | 18:45:27 | `NQYgk79Mqj` | 34 |
  | 18:45:27 | `UNtCLxEwoA` | 5 |

  A participant joined at 18:45:23. Our last decodable outbound frame is in the window ending
  18:45:22. Exactly one batch of to-device messages leaves this client all call --
  `Sending batch of 4 to-device messages with ID 38`, at 18:43:52 -- and none at 18:45:23.

  **The mechanism is missed *distribution*, not the missed rotation.** A first reading blamed
  our key index staying at 0, and that reading is wrong: frames name their key index in the
  trailer and receivers retain old ring slots, so a peer that already holds index 0 keeps
  decrypting us regardless of whether we rotate. Only a peer that never received our key at
  all sees a frozen picture while our encoder runs perfectly -- which is exactly what a
  joiner we never sent to would see. Reviewed against `RTCEncryptionManager`,
  `ParticipantKeyHandler` and `matrixKeyProvider.ts`.

  Confirmed rather than assumed:
  - The shipped bundle really is `RTCEncryptionManager` (`keyRotationGracePeriodMs` is
    present in `index-ZYqhOGev.js`), so its rotation rules are the ones in force. A JOIN
    re-distributes the existing key inside a 10s grace period and rotates outside it; ours
    was 91s old, so a rotation *and* a distribution were both due and neither happened.
  - The bridge is not dropping anything. Element Call's key provider only ever calls
    `onSetEncryptionKey`, never `ratchetKey`, so zero `ratchetRequest` messages is normal and
    not evidence of a missed route. The only unforwarded worker messages all call were
    `enable` and `updateCodec`.

  So the fault is upstream of the bridge, in this client's MatrixRTC membership handling:
  `onMembershipsUpdate` appears not to fire (or to diff to nothing) after the initial join.
  The room carries dozens of stale `LEFT` membership events for our user, one per past
  device, which is the obvious suspect for a broken changed-memberships diff.

  Next: instrument `MatrixRTCSession`'s membership callbacks in the webview to see whether
  the update fires at all, and whether the stale entries break the diff.

- [ ] **M4. Other participants' keys arrive up to 36 seconds after joining, or not at all.**
  Joined at 18:14:52; the first key belonging to anyone else arrived at 18:15:28, and a third
  participant's at 18:15:33. 4,400 frames were dropped undecryptable in between, which is
  every remote camera black for over half a minute.

  Not a decryption bug: the native keyring is 256 slots per participant with a working
  ratchet, and the moment keys landed everything decoded.

  Nor, on review, is it fixable from our side by asking. Key acquisition in this protocol is
  pure push -- `RTCEncryptionManager` has no request mechanism, and a joiner waits to be sent
  keys when *existing members' clients* observe its membership. A 36s gap therefore means
  remote clients saw our membership event that late, which points at membership propagation
  (see [M5]) rather than at our keyring. Related: frames arriving at one index below the one
  we hold are protocol-expected for a few seconds after a sender rotates -- the sender keeps
  transmitting on the previous index for `useKeyDelay = 5000`ms -- and dropping them is the
  conformant response, since indices are independent keys and index N-1 is not derivable
  from N. Thousands of them is not expected and is worth a second look once M5 is fixed.

## Closed

- [x] **M1. The frame rate is not configurable by a person.** Implemented, unverified until
  a real call. `set_max_encode_fps`/`get_max_encode_fps` (Tauri commands in
  `src-tauri/src/commands/media_devices.rs`) persist the setting through
  `tauri-plugin-store` (`settings.json`, already a dependency, previously registered but
  unused from Rust) and push it live into every running video pipeline's own
  `fps_override` atomic -- the same level-not-event pattern `apply_bitrate_override`
  already uses for `setParameters`, polled once per frame by the new
  `apply_fps_override`, so a change takes effect on the pacer without a new encoder, a
  keyframe, or a dropped call.

  Resolved against the page's own `frameRate` constraint by `resolve_encode_fps`: the
  setting wins outright when one is set, logged whenever it overrides a different page
  ask, otherwise the page's constraint behaves exactly as before. Clamped honestly by
  `clamp_encode_fps_setting`, to the same `MAX_ENCODE_FPS_CEILING` the removed
  `ELEMENTIUM_MAX_FPS` was held to.

  Reaches the second, capture-side limiter noted in the ask (`pipewire_capture` halving a
  60fps camera before the encoder ever sees a frame) for every pipeline that *starts*
  after the setting changes, because `start_camera_pipeline` now resolves `req_fps` --
  what it asks the camera for -- from the same setting. It does not reach a camera stream
  already open: that rate is negotiated once at stream start, and reopening a camera
  mid-call risks the same `EBUSY` window `start_camera_pipeline` already waits out.

  No UI: exposed only as the two Tauri commands above. A UI would need a numeric or
  slider control (bounded 1..=120) wired to `set_max_encode_fps`, reading its initial
  value from `get_max_encode_fps` -- not built here because it would mean patching the
  embedded Element Web bundle rather than this app's own Rust/Tauri surface.

## Found by the automated suite, 2026-08-10

Both from the screen-share work. Neither is why anyone complained; both are real, and
neither had anywhere to be recorded before there was a test that exercised the path.

- [x] **M6. The X11 capturer reports its own size as 0x0.** Fixed, unverified against a real
  X server. `video pipeline started` logged `width=0 height=0` for an X11 share because a push
  capturer negotiates nothing and its size was seeded at zero until a frame arrived -- a zero
  that reads as a measurement rather than as "not yet known".

  `elementium_screen::x11::source_size` now asks the X server, and `VideoSource::start_push`
  takes what the producer declares. The first frame still overrides it: a declared size is a
  claim and a frame is a fact, and a window can be resized between the two. A source that
  refuses to report its size starts anyway, with a warning, since the frames carry their own
  geometry regardless.

  The share test keeps its Xvfb `-screen` fallback for exactly that case, but should now
  normally read the resolution out of the pipeline log like the camera's does.

- [x] **M7. X11 capture runs at about 3.3fps under Xvfb.** Diagnosed and fixed, and the
  diagnosis was not the one recorded here. The original entry blamed a full `XGetImage` per
  frame with no shared memory and named MIT-SHM as the remedy. Measured
  (`cargo run -p elementium-screen --example x11_capture_rate`, one 1280x800 Xvfb display,
  same build, same run):

  | environment | `capture_image` | conversion | effective |
  |---|---:|---:|---:|
  | `XDG_SESSION_TYPE=wayland` (as the harness ran it) | 406ms | 0.3ms | 2.4fps |
  | `XDG_SESSION_TYPE=x11` | 5.1ms | 0.3ms | 170fps |

  `XGetImage` was never slow. xcap 0.4 branches *monitor* capture on `XDG_SESSION_TYPE` or
  `WAYLAND_DISPLAY` and, if either says Wayland, ignores `DISPLAY` and screenshots the whole
  Wayland desktop over D-Bus -- `org.gnome.Shell.Screenshot` or the portal -- writing a PNG to
  `/tmp/screenshot`, reading it back and decoding it, once per frame. Window capture has no
  such branch.

  Two consequences, one of them not about speed at all:

  - **The frames were of the real desktop.** `just app-join` blanked `WAYLAND_DISPLAY` but
    left `XDG_SESSION_TYPE=wayland`, so the automated share test captured this machine's actual
    screen rather than the Xvfb stage it had carefully set up, and sent it to the far-end test
    participants. `/tmp/screenshot` exists on this machine, dated during that work.
    In the product the same path means someone who picks one X11 monitor on a Wayland session
    shares everything on screen instead.
  - The 3.3fps figure, and the share test's 2fps floor built on it, described a D-Bus round
    trip rather than a capture path.

  Fixed by refusing an X11 *monitor* capture when xcap would route it to the portal (naming the
  PipeWire path, which asks what to share), and by setting `XDG_SESSION_TYPE=x11` in
  `just app-join` alongside the blanked `WAYLAND_DISPLAY`.

  Next: with the harness fixed the share should run at the encode cap, so `MIN_SHARE_FPS` in
  `app-call-video.spec.ts` should rise from 2 to `MIN_FPS`. Left at 2 until a run confirms it,
  since a floor that has never been met is a red test rather than a measurement.

  Note the contrast in the original run: the camera path held exactly 30.0fps with `paced_out`
  at zero on every attempt, which is what pointed at the capture rather than the encode or send
  path in the first place.
