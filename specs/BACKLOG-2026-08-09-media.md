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

  **Tested, 2026-08-10, and the answer is the opposite of the guess.** The bitrate did not
  match what was configured -- the encoder was producing *five times* it. `vpx_codec_encode`
  was handed a frame duration of 1 in a 1/90000 timebase, so libvpx believed it was encoding
  90,000 frames a second and its rate control never saw a second pass:

  | asked | produced | |
  |---|---:|---:|
  | 720p @ 1700 kbps | 9045 kbps | 532% |
  | 720p @ 2764 kbps | 8852 kbps | 320% |
  | 1080p @ 4000 kbps | 21285 kbps | 532% |

  Fixed (`frame_duration_ticks`, plus the real-time buffer model libvpx does not default to).
  Reproducible without a call:
  `cargo run --release -p elementium-codec --example encode_bitrate`.

  **The residual overshoot was the measurement, not the encoder.** The first reading of the
  fixed build -- 114%, 84% and 86% after the opening keyframe -- was taken against a picture
  built to be incompressible, moving diagonal bands over a checker. Against that picture VP8
  saturates its quantizer and cannot obey any budget it is given, so those numbers are a
  property of the content. The example now encodes a camera-like picture as well, and on that
  one the same build holds **96%, 96% and 96%** over the whole run, keyframes included.

  **A second thing libvpx does not do by default: bound the keyframe.** Everything above
  budgets the average; `VP8E_SET_MAX_INTRA_BITRATE_PCT` defaults to no cap, and a receiver
  that cannot decode asks for a keyframe about once a second. Now set to libwebrtc's
  `MaxIntraTarget` derivation -- half the rate controller's buffer in frame budgets, 900% at
  30fps. Honestly: at 900% it does not bind on either test picture, and tightening it to 100%
  shrinks the keyframe from 122KB to 72KB while making the average *worse* (154% to 170%),
  because frames referencing a starved keyframe cost more to correct. It bounds the
  pathological case; it is not a knob to tune down.

  **Why this is a candidate for the frozen picture.** A sender emitting five times its
  allowance does not get five times the throughput -- it gets loss, wherever the narrowest
  point is, and loss in VP8 breaks the reference chain for every frame that follows. That
  fits the evidence in this entry better than anything ruled out above: the receiver decoded
  the keyframe, could not decode what followed, and asked again -- 215 PLIs in three and a
  half minutes, answered with 192 keyframes, each keyframe itself a burst several times over
  budget. The measured ~336 kbps arriving is then what survived, not what was sent.

  Not proven: this was measured on a synthetic picture with no network in the path, and the
  fault has never reproduced against the local SFU. What it does mean is that the sender was
  behaving badly in a way that would produce exactly these symptoms, and it no longer is.

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

  **The warning half is done** (`NotConnectedWatch`): first warning after five seconds of
  unattached frames -- headroom over `AUDIO_HANDOVER_TIMEOUT`, so an ordinary device swap does
  not cry wolf -- then every fifteen. The decision is a pure function of instants, tested
  without a clock.

  **The recovery half cannot be fixed on this side, and that is worth stating rather than
  rediscovering.** Reviewed 2026-08-10: the only reattachment mechanism we have is
  `adopt_idle_pipelines`, which fills an empty slot from an existing engine connection. In this
  incident there was no connection to adopt from -- livekit closed the publisher and built no
  replacement -- so a timer that retried adoption would find nothing on every tick. Attaching
  the microphone to some *other* live connection would be worse than the fault: a subscriber
  peer connection is not where our audio goes.

  So this is the negotiation rewrite's (N1) to fix, or livekit's. What our side owes is the
  warning, which now exists, and not pretending to recover.

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

  **The instrumentation this entry asked for now exists, and it is a verdict rather than a
  trace** (`key-distribution-watch.ts`, 2026-08-10). Both halves were already logged and
  nothing compared them: `membership-log.ts` records every join and leave, `widget-api-log.ts`
  records every `fromWidget send_to_device` -- Element Call's only route for handing anybody a
  key -- and its own comment said one "should follow this line" while leaving a person to read
  two interleaved streams and notice an absence. A change now arms an expectation, a
  distribution clears it with a latency, and twelve seconds of silence is reported once,
  naming what it was waiting on.

  Main frame only, and that is forced rather than chosen: `toWidget` traffic arrives at the
  widget iframe and `fromWidget` at the main window, so the widget frame can see the
  membership change and never the answer. Only the main frame sees both.

  **On the local stack the path works.** In `just test-app-call-audio`: two participants joined
  at 12.9s, one expectation armed (a flurry deliberately does not restart the clock), answered
  after 2.1s. The audio suite now asserts both directions of that -- no overdue line, and at
  least one answered one, since an absence of failures proves nothing on a run where the watch
  never armed.

  So the fault still has not reproduced here, and the untested shape is the one the incident
  had: a participant joining *late*, after the ten-second grace period, when a rotation and a
  distribution are both due. `app-call.spec.ts` already drives exactly that ("a participant who
  joins after Elementium can decrypt its media") and needs a camera, which is why it has not
  run -- see [M11].

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

- [ ] **M11. The bitrate uplift has never fired in a live call.** `uplift_bitrate_kbps` is
  unit-tested -- 1700 kbps becomes 3825 for a 720p budget spent on 1080p -- and has never once
  run in a call. Every run since it was written has captured 1280x720, which is exactly what
  the page asks for, so there is correctly nothing to uplift and the branch is never entered.

  Blocked on the camera, not on the code. Verification is one run:
  `ELEMENTIUM_CAPTURE_RESOLUTION=1920x1080 just test-app-call-video`, expecting
  `camera capture 1920x1080` and a line reading `sfu_kbps=1700 applied_kbps=3825`. Checked
  again 2026-08-10T18:00Z: `/dev/video7` is held by an Element Desktop electron process
  (`fuser` shows it mmap'd, nearly four hours), and PipeWire negotiates one format per device
  shared across clients -- see M10 -- so whatever that client settled on is what we get.
  Nothing here is worth working around; it needs the camera free.

- [ ] **M12. The forwarder census has never seen the leak it was built to catch.** The
  `ACTIVE_FORWARDERS` count and its `ForwarderCensus` guard exist because a forwarder that
  outlives its connection sends into nothing. Two clean calls have created two connections and
  removed none -- which is the shape of a leak and also the shape of a call that simply ends
  with its connections alive.

  So the guard's arithmetic is pinned by a test and its *diagnosis* is unproven. A leak needs
  reconnection churn to show up: a run that drops and re-establishes the publisher connection
  several times and then asserts the census returns to zero. Worth writing once N1 or M3 gives
  us a reconnection path to drive.

- [ ] **M13. Neither of the two settings this file added has a way to set it.** The frame rate
  (M1) and the capture resolution (M9) are both persisted, both live, both clamped honestly --
  and both reachable only from a Tauri command or an environment variable. Nobody who is not
  holding a debugger can change either.

  Deliberately not built alongside the settings themselves, and the reason is worth keeping:
  the app's own window is the embedded Element Web bundle, so a control has nowhere to live
  that is not a patch to somebody else's UI. That is a real decision to make -- a separate
  Elementium settings window, a tray menu, or a patch to the bundle -- and it is not one to
  make from a backlog note. Until it is made, both settings are developer-only, which is
  what the M1 entry says and what this entry exists to stop us forgetting.

- [x] **M8. A dead virtual camera enumerates first and costs ten seconds of every camera
  open.** Fixed by ranking, not filtering. `list_video_sources` returned:

  ```
  [0] node 161  ...virtual/video4linux/video11  (Unknown device (V4L2))   <- v4l2loopback, no producer
  [1] node 349  ...usb-0_1.1_1.0                (OBSBOT Tiny 2 Lite)
  ```

  That order is both what the page is offered and what capture falls back through, so Element
  Call asked for the loopback by `deviceId: {exact: video-input-pw-161}` and every camera open
  waited out the full `FIRST_FRAME_TIMEOUT` before reaching the camera that works: 12.3s
  against 0.3s once the order is right.

  Kernel virtual devices (`/sys/devices/virtual/video4linux/`, which `PipeWire` renders into
  the node name) now sort last. Ranked rather than removed on purpose: a loopback *with*
  something feeding it is a legitimate camera -- OBS's virtual camera is exactly that -- and
  the substring alone is not enough to remove a device someone chose deliberately. A camera
  merely *called* "Virtual" is unaffected; the test pins that.

## Closed

- [x] **M9. Raising the capture resolution does not raise the bitrate cap.** Found while
  verifying the new capture-resolution setting. Element Call asks `getUserMedia` for 1280x720
  and budgets 1700 kbps for *that*; the setting overrode the request without the page ever
  hearing about it, so 2.25x the pixels went out on a 720p budget and the setting was quietly
  counter-productive -- more pixels, worse picture. Every frame still arrived (1200 encoded,
  1200 decoded at three participants, none lost); this was about how good each one was.

  Two fixes, complementary rather than alternatives. `scaleResolutionDownBy` is now honoured
  (`set_video_scale`, `scale_i420`), and the cap is raised in proportion to the extra pixels
  when the publish exceeds what the page asked for (`uplift_bitrate_kbps`, 1700 -> 3825 for
  720p -> 1080p) -- deliberately exceeding a number that is partly a congestion estimate, paid
  only by someone who explicitly asked for more. Still unverified in a call: see [M11].

  Recorded here because the first reading was wrong and the correction is worth keeping:
  ignoring `scaleResolutionDownBy` was blamed, and reading one encoding out of three is how.
  livekit sends three simulcast encodings (scales 1, 2 and 2.667); this app publishes one
  stream and resolves them to the best layer, which is scale 1. It was never being asked to
  shrink.

- [x] **M10. A requested capture size was honoured or not depending on who else had the
  camera.** Two runs of the same test, same code, same camera, same
  `ELEMENTIUM_CAPTURE_RESOLUTION=1920x1080`, negotiating 1920x1080 on one and 1280x720 on the
  next. Misdiagnosed twice on the way -- a raw-format race, then first-pipeline pinning --
  before `pw-dump` showed a browser holding the camera and PipeWire negotiating **one format
  per device, shared across every client**. Whoever opens it first decides, and the second
  client is told what it got.

  Our side of it is fixed: `format_params` now offers each parameter twice, once demanding the
  size exactly and once merely preferring it, so a source that *can* give us the size does,
  and one that cannot still starts rather than falling back silently to a range default.
  What remains is not ours -- a camera already streaming at 720p for another application will
  stream at 720p for us.

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

  Confirmed end to end, 2026-08-10T10:57Z, three far-end participants:

  | | before | after |
  |---|---:|---:|
  | share frames encoded | 300 in 90-118s | 892 in 29.8s |
  | rate | ~3.3fps | ~30fps |
  | longest gap between decoded frames | -- | 155ms |

  892 encoded reached 892/891/892 decoded at 1280x800, zero dropped, zero of ~2496 packets
  lost; the camera track in the same run reconciled 1200 encoded against 1200/1201/1200
  decoded. `MIN_SHARE_FPS` has been raised from 2 to `MIN_FPS`, now that the floor has been
  met rather than assumed.

  Note the contrast in the original run: the camera path held exactly 30.0fps with `paced_out`
  at zero on every attempt, which is what pointed at the capture rather than the encode or send
  path in the first place.
