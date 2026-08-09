# Findings from the 2026-08-09 call: reconnect leaves the app half-attached

One real call against `matrixrtc.redwood-guild.com`, four peer connections, three reconnects
in about seventy seconds. Reported symptoms were "audio still not coming through properly",
"video very pixelated", "she only gets key frames", "my webcam did not resume after the
reconnect — I had to toggle it", and "I keep getting disconnected".

They are very largely one fault with four faces, and the evidence for that is that **the
transport never failed**: ICE reported `Checking -> Completed` four times and never once
`Disconnected` or `Failed`. Nothing dropped on the network. Something above tore the call
down and rebuilt it, three times, and the rebuild does not restore what the teardown
detached.

Per connection, which is the clearest single view:

| peer connection | egress reports | remote tracks announced | inbound audio frames |
|-----------------|---------------:|------------------------:|---------------------:|
| `pc-…79ab752d`  |           2838 |                       2 |                   39 |
| `pc-…a60fb560`  |            108 |                       2 |                   67 |
| `pc-…a505f4fa`  |              0 |                       0 |                    0 |
| `pc-…3e9b353f`  |             67 |                       0 |                    0 |

The later connections carry almost nothing.

## Open

- [ ] **R1. Capture does not reattach after a reconnect.** The log says it outright --
  `the peer connection this microphone was feeding has closed; audio is detached until a
  connection replaces it` -- and then no connection ever claims it. The camera is the same:
  it came back only when the user toggled it by hand. A reconnect is routine and the
  recovery from it should not be a manual step. Ours, self-contained, and the one that
  removes a workaround the user is currently performing.

- [ ] **R2. Preview fetch loops outlive their tracks.** Three tracks were polling
  `get_video_frame` at 29fps with `drawn=0`; only the newest (`drawn=146`) was real. A
  track that has stopped never stops its loop, so every reconnect adds another 29 IPC calls
  a second, permanently. Ours, small.

- [ ] **R3. Why the call reconnects at all is not established.** Each teardown is preceded
  by `[UnhandledRejection] Error: No changes to apply`, and beside it
  `[EncryptionManager] No matching RTC membership for k…`. That string is in neither our
  source nor the shipped bundle by search, so who throws it is genuinely unknown and must
  not be guessed at: the fix is different depending on whether Element Call is churning its
  own membership or our shim is provoking it. Needs a trace of the rejection rather than
  more log reading. **This is the root; R1 and R2 only make it survivable.**

- [ ] **R4. Video quality does not recover after a restart.** 640 kbps to 1.5 Mbps measured
  against a 2764 kbps target at 720p30, with the encoder recreated on every reconnect and
  its rate control starting from nothing each time. Reported as "very pixelated" and, with
  25 PLIs against 1 in a clean run, as "only key frames" at the far end. Partly downstream
  of R3; worth measuring again once reconnects stop.

- [ ] **R5. Frame rate is fixed at 30 with no way to change it.** Nothing in WebRTC, VP8,
  H.264 or the SFU requires it -- `MAX_ENCODE_FPS` is ours, and the reasoning (60fps roughly
  doubles bitrate and encode cost for a difference few people see on a webcam) is sound as a
  *default* rather than as a law. The camera delivers 60. Wanted as a setting. Note the
  interaction: at 720p60 on software VP8 this is a real CPU load, where the VAAPI H.264
  encoder -- unblocked earlier today -- would absorb it comfortably.

## Checked and not a fault

- **Audio silence in the outbound stats.** `silent_packets` reached 250 of 250 in some
  windows, which looks alarming, but `loud_but_silent` peaked at 2: the input really was
  quiet in those windows. Audio egress showed `loss: 0.0` throughout.
- **The startup gap.** Fixed the same day: audio captured before end-to-end encryption can
  run is now held and released rather than dropped.
- **Inbound decryption.** Zero inbound E2EE drops this session, against 33 the session
  before.
