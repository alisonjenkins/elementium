# Outbound media, 2026-08-09

Opened from a live call in which the far end saw one pixelated frame that never updated,
heard nothing after a mid-call device change, and in which four fifths of the room's video
never decrypted. Each of these is a separate fault with separate evidence; they are
collected here because they were all found in the same session.

## Open

- [ ] **M1. The frame rate is not configurable by a person.** Two mechanisms exist and
  neither is reachable from the UI: `ELEMENTIUM_MAX_FPS` (an environment variable, so it
  needs a restart and a shell), and the page's own `frameRate` constraint, which reaches the
  backend only as of today -- before the `camelCase` fix it arrived as `None` for the life of
  the project, so the encoder has always run at the compiled-in `MAX_ENCODE_FPS = 30`
  regardless of what anyone asked for. Wanted: a setting, persisted, applied without a
  restart, and honest about the cap it is clamped to (`MAX_ENCODE_FPS_CEILING = 120`).
  Requested directly by the user, mid-call, having watched the frame rate be bad and having
  no way to change it.

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

- [ ] **M4. Other participants' keys arrive up to 36 seconds after joining, or not at all.**
  Joined at 18:14:52; the first key belonging to anyone else arrived at 18:15:28, and a third
  participant's at 18:15:33. 4,400 frames were dropped undecryptable in between, which is
  every remote camera black for over half a minute.

  Not a decryption bug: the native keyring is 256 slots per participant with a working
  ratchet, and the moment keys landed everything decoded. The gap is that joining does not
  fetch the keys of participants already in the room -- we wait for the next rotation to
  happen to include us.
