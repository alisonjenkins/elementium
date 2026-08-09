# Tasks: Screen and application sharing, with audio

**Spec**: [spec.md](spec.md) | **Plan**: [plan.md](plan.md) | **Created**: 2026-08-08

Sequenced by what can be *observed*, not by what is easiest. Phase 2 is blocking for a
reason worth restating at the top: until a media write can name its track, a second video
track cannot reach the wire, and a screen share that captures perfectly will still show
nothing while the sender's own counters advance. That failure is indistinguishable from
success from the sending side.

Every task is intended to leave the workspace compiling on its own, so each can be an
atomic commit that reverts cleanly.

---

## Phase 1: Setup

- [X] T001 **Superseded by the after-measurement (T045), which passes**: the leak this baseline existed to size was fixed by `ShareSession` owning the portal session before any baseline was taken, and capture could not run at all until the DMA-BUF work landed. Record the pre-change baseline for the teardown leak in `specs/008-screen-share-capture/quickstart.md` — run ten start/stop cycles of the current `get_display_media` and capture the PipeWire node count before and after via `pw-dump`, so SC-006 has a measured "before" rather than an asserted one
- [X] T002 [P] Confirm and record the portal backend actually serving ScreenCast on this machine in `specs/008-screen-share-capture/research.md` (append to R7) — `busctl --user list | grep Mutter.ScreenCast` and the resolved `niri-portals.conf` preference, because a US2 failure under `xdg-desktop-portal-wlr` would be environmental rather than ours
- [X] T003 [P] Add a `--screen` mode to `crates/elementium-media/examples/capture_attribution.rs` that captures from a portal-supplied node id, so screen capture can be measured without a call (quickstart Level 2)

---

## Phase 2: Foundational — per-track routing (BLOCKING)

**Nothing in Phase 3 onward can be observed working until this is done.** See research R2.

- [X] T004 Add `MediaTrackKey { kind, source }` to `crates/elementium-types/src/lib.rs` with the four source values matching LiveKit's `TrackSource` names exactly (`microphone`, `camera`, `screen_share`, `screen_share_audio`), plus a method deriving the LiveKit `cid` suffix `{kind}-{source}` so the routing key and the SFU pairing key cannot drift apart
- [X] T005 Add `MediaTrackKey` to `IoCommand::WriteAudio` and `IoCommand::WriteVideo` in `crates/elementium-webrtc/src/engine.rs`, updating every construction site; keep the existing behaviour by passing the camera/microphone keys at each site so this commit changes shape without changing routing
- [X] T006 Replace the single `audio_mid` / `video_mid` send pair in `crates/elementium-webrtc/src/peer_connection.rs` with a `HashMap<MediaTrackKey, Mid>` of published send mids, populated as each track is published
- [X] T007 Route `WriteAudio` / `WriteVideo` to the mid registered for the command's `MediaTrackKey` in `crates/elementium-webrtc/src/peer_connection.rs`, logging and dropping a write whose key has no published mid rather than falling back to a mid of the same kind — a silent fallback here reproduces exactly the invisible failure this phase exists to prevent
- [X] T008 Record the published `MediaTrackKey` against its mid in `crates/elementium-webrtc/src/livekit/room.rs` when `publish_track_inner` sends its `AddTrackRequest`, deriving the key from the `kind` and `source` it already has
- [X] T009 Add a test to `crates/elementium-webrtc/tests/` that publishes two video tracks with different `MediaTrackKey`s and asserts frames written for one appear on that track's mid and not the other's — quickstart Level 1, and the assertion that makes the rest of the feature diagnosable
- [X] T010 Replace `MediaState`'s `camera` and `audio_capture` singleton slots with `pipelines: Mutex<HashMap<MediaTrackKey, PipelineHandle>>` in `src-tauri/src/commands/media_devices.rs`, folding `CameraPipelineHandle` and `AudioCaptureHandle` into one `PipelineHandle` carrying the key (the two already share the `CapturePipeline` trait, which is the map's natural element)
- [X] T011 Update `adopt_idle_pipelines` in `src-tauri/src/commands/webrtc.rs` to adopt every idle pipeline by key rather than looking in two named slots, preserving the existing per-key mid-call connection inheritance so a source restarted mid-call still does not freeze the far end
- [X] T012 Update `get_user_media` and `stop_track` in `src-tauri/src/commands/media_devices.rs` to address pipelines by key, keeping camera and microphone behaviour byte-for-byte identical — this commit should change no observable behaviour and is the checkpoint that the refactor is safe

**Checkpoint**: camera and microphone work exactly as before, and two video tracks can be routed independently. `cargo test --workspace` and `cargo clippy --workspace -- -D warnings` clean.

---

## Phase 3: US1 — A shared screen is visible to the other participant (P1)

**Goal**: a remote participant sees the sharer's monitor updating live.

**Independent test**: two endpoints in a local call; `framesDecoded` at the receiver increases over at least 30 seconds. A track existing, or being `live`, or one frame arriving, is not a pass — the bug being fixed produces a valid track carrying nothing.

- [X] T013 [US1] Extract the encoder-negotiation and per-frame stages of `camera_pipeline_loop` into named helpers in `src-tauri/src/commands/media_devices.rs` with no behaviour change — the function is already near the workspace's 100-line `too_many_lines` cap, so this must precede generalising it or the next commit cannot pass lint
- [X] T014 [US1] Generalise the extracted loop into a source-agnostic `video_pipeline_loop` in `src-tauri/src/commands/media_devices.rs` taking an already-started capture and a `MediaTrackKey`, with the camera path as its first caller
- [X] T015 [US1] Add a screencast constructor to `crates/elementium-media/src/video_source.rs` that opens `PipewireCapturer` against a node id supplied by the caller rather than one found by enumeration, reusing the existing PipeWire variant — the portal already ends at a node id, so this is the same source opened differently, not a new source kind
- [X] T016 [US1] Add `ShareSession` to `crates/elementium-screen/src/lib.rs` owning the chosen backend, the source kind, the video node id and the video `MediaTrackKey`, whose `Drop` stops the pipeline and closes the portal session — the current code drops its capturer handle with the capture still running, which is the leak T001 measured
- [X] T017 [US1] Select the capture backend once in `crates/elementium-screen/src/lib.rs` and hold it on the `ShareSession`, so enumeration and start cannot disagree (research R6); `get_capture_sources` and `get_display_media` both consult the session's choice
- [X] T018 [US1] Rewrite `get_display_media` in `src-tauri/src/commands/screen_capture.rs` to build a `ShareSession`, start a video pipeline keyed `{video, screen_share}` from its node id, and return the structured response from `contracts/tauri-commands.md` — deleting the `TODO: Wire frame_rx into the video pipeline` channel that currently drops every frame
- [X] T019 [US1] Distinguish picker cancellation from failure in `src-tauri/src/commands/screen_capture.rs`, returning a cancellation error the frontend can map to `NotAllowedError` without treating it as a fault (FR-010, SC-007)
- [X] T020 [US1] Publish the screen video track via `publish_video_track("screen_share", …)` in `src-tauri/src/commands/livekit.rs` — the LiveKit layer already maps this to `TrackSource::ScreenShare` (research R3), so this is a call site, not protocol work
- [X] T021 [US1] Extend `stop_track` in `src-tauri/src/commands/media_devices.rs` so stopping a screen video track drops its `ShareSession`, tearing down the capture and portal session with it (FR-009)
- [X] T022 [US1] Treat an empty `get_capture_sources` result as "the platform delegates to a system picker" in `frontend/src/shim/media-devices.ts`, removing the `sourceId: "default"` fallback that sends a value no backend defines
- [X] T023 [US1] Rewrite `getDisplayMedia` in `frontend/src/shim/media-devices.ts` to mirror the working `getUserMedia` path — size the canvas from `firstFrameGeometry(videoTrackId)` **before** `captureStream`, attach it in-viewport at zero opacity, and pump native frames via `startLocalVideoFrameFetch` — replacing the fixed 1920x1080 canvas that is never painted. Both the sizing order and the in-viewport attachment are fixes already earned on the camera path; omitting either reintroduces a fault (stride banding, compositor sampling) on a path nobody is watching
- [X] T024 [US1] Wire the page-side track's `stop()` through to `stop_track` in `frontend/src/shim/media-devices.ts`, so a share stopped from the Element Web UI reaches native teardown
- [X] T025 [US1] **Done — the attended SC-001 test in `frontend/tests/browser/receive-path.spec.ts` asserts exactly this, sampling every 5s across 30s and requiring every consecutive pair to climb.** Add a browser test to `frontend/tests/browser/` that starts a share and asserts the receiving endpoint's `framesDecoded` increases over at least 30 seconds (quickstart Level 4, SC-001)
- [X] T026 [US1] **Done.** Source chosen, capture started, frames flowing and stopped were already emitted (`screen share started`, the per-source frame line in `video_pipeline_loop`, `screen share torn down`). Two gaps closed: the portal exchange now times `CreateSession`, `SelectSources` and `Start` separately, so a share that never showed a picker is distinguishable from one where a person took four seconds to choose; and the command runs under a `share` span carrying the call's correlation id, so the steps *before* a pipeline exists belong to the call. No source name, window title, pixel or sample is recorded. Emit structured share-lifecycle diagnostics (source chosen, capture started, frames flowing, stopped, failed) across `src-tauri/src/commands/screen_capture.rs` and `crates/elementium-screen/src/`, carrying the correlation id and recording no pixel or audio content (FR-012)

**Checkpoint**: US1 delivered. This alone is a viable increment — screen sharing works, silently.

---

## Phase 3b: US1 blocker found by measurement (2026-08-08)

Everything in Phase 3 is done and the pipeline is in place, but a real capture cannot
negotiate on this machine. See research R9 for the measured evidence.

- [X] T047 [US1] Give `PipeWire` format negotiation a source profile in `crates/elementium-media/src/pipewire_capture.rs`, so a screen is not asked for camera parameters — accept a variable frame rate (`0/1`, which a compositor advertises because it emits on damage rather than on a clock) and raise the size bound to 8K. Necessary but **not sufficient**: negotiation still fails without T048
- [X] T048 [US1] Offer a `modifier` property listing `DRM_FORMAT_MOD_INVALID` and `DRM_FORMAT_MOD_LINEAR` for the screencast profile in `crates/elementium-media/src/pipewire_capture.rs`. **Done, and it settled the cheaper of the two routes in R9 by ruling it out**: negotiation now succeeds (`1880x1446 Raw(Bgrx)`) but the stream then fails at `error alloc buffers: Invalid argument` with zero frames delivered — the compositor allocates DMA-BUF whatever modifier is offered. Kept regardless: a screencast node requires the property to negotiate at all, so without it the DMA-BUF work would fail earlier for a second, unrelated reason. See research R10
- [X] T050 [US1] **Done 2026-08-08, and US1 is unblocked.** Import DMA-BUF buffers in `crates/elementium-media/src/pipewire_capture.rs` — accept `SPA_DATA_DmaBuf`, and read the frame through the DRM PRIME fd rather than `MAP_BUFFERS`, which cannot map a GPU handle. This is spec 005's T010, filed there as a camera optimisation and a hard prerequisite here. R10 established by measurement that no cheaper route exists on this compositor
- [X] T049 [US1] Re-run quickstart Level 2 (`cargo run -p elementium-media --example capture_attribution -- --screen`) once T048 lands, and record the frame counters in `specs/008-screen-share-capture/quickstart.md`

---

## Phase 4: US2 — Sharing one window rather than everything (P2)

**Goal**: only the chosen window's contents leave the machine.

**Independent test**: share one window, change content in a *different* window, confirm the receiver sees no change. The negative test is the test — a backend that silently falls back to full-monitor capture passes the positive one and fails the user.

- [X] T027 [US2] Record the portal's returned source kind (monitor vs window) on the `ShareSession` in `crates/elementium-screen/src/wayland.rs`, since the audio scoping in Phase 5 and the diagnostics both need to know which was chosen
- [X] T028 [US2] **Done 2026-08-08, and the obvious implementation would have been wrong.** Closing a shared window does *not* error the stream — it goes Streaming/Paused/Streaming and simply stops delivering, which is indistinguishable from a healthy static share. The signal is the node being removed from the `PipeWire` registry (its id was observed being recycled for an unrelated Link seconds later). A frame-stall timeout, the tempting fix, would end legitimate shares of a still document. Verified live by killing a shared window mid-capture. Handle the shared window disappearing mid-share in `crates/elementium-screen/src/wayland.rs` — end the share cleanly and report why, rather than pumping a dead node
- [X] T029 [US2] **SC-003 proven 2026-08-08 by pixel comparison at the capture side, not in the browser** — 0.00 mean change from content outside the shared window against 4.56 from content inside it (quickstart Level 5). The browser harness cannot host it: Chromium takes the workspace on a tiling compositor, the scripted windows stop being composited, and a window that is not drawn produces no frames. Original task: add a browser test to `frontend/tests/browser/` asserting that content changed outside the shared window produces no change at the receiver (SC-003)

---

## Phase 5: US3 — The shared content's audio is heard too (P2)

**Goal**: the remote participant hears the shared content in addition to the microphone.

**Independent test**: shared source plays a known tone with the mic live; receiver hears both; muting the mic silences only speech.

- [X] T030 [US3] Add `crates/elementium-media/src/pipewire_audio.rs` capturing PCM from a PipeWire node id using the `pipewire` crate already used for video — a new module rather than an extension of `audio_capture.rs`, which is cpal/microphone and would drag ALSA-compat semantics into a PipeWire concern (research R4)
- [X] T031 [US3] Enumerate candidate share-audio nodes in `crates/elementium-media/src/pipewire_nodes.rs` — sink monitors (`Audio/Sink`) and per-application output streams (`Stream/Output/Audio`) — alongside the existing video source enumeration
- [X] T032 [US3] Add the `audio: boolean` request field and the structured response (`videoTrackId`, `audioTrackId`, `audioScope`, `audioScopeFallback`) to `get_display_media` in `src-tauri/src/commands/screen_capture.rs` per `contracts/tauri-commands.md`
- [X] T033 [US3] Start a second audio pipeline keyed `{audio, screen_share_audio}` from the desktop sink monitor when audio is requested, in `src-tauri/src/commands/screen_capture.rs`, hanging its lifetime off the `ShareSession`
- [X] T034 [US3] Already supported — `livekit_publish_track` maps `("audio", "screen_share_audio")` to `MediaTrackKey::screen_share_audio()`, and the frontend bridge passes the source through unchanged. Publish the share audio track via `publish_track("audio", "screen_share_audio")` in `src-tauri/src/commands/livekit.rs`
- [X] T035 [US3] Ensure no audio capture is opened at all when `audio` is false, in `src-tauri/src/commands/screen_capture.rs` (FR-008) — the guard, not merely the absence of a call site
- [X] T036 [US3] Add the share audio track to the returned `MediaStream` in `frontend/src/shim/media-devices.ts` only when `constraints.audio` is truthy, and surface `audioScopeFallback` to the caller
- [~] T037 [US3] **Partly done 2026-08-08** — measured for the capture path (twelve audio streams before, the same twelve by id during a running screen capture); the `audio: false` branch of `get_display_media` still needs the running app. Verify SC-005 from outside the process and record the result in `specs/008-screen-share-capture/quickstart.md` — start a share without audio and confirm via `pw-dump` that no new input stream belonging to this application appears; reading the code is not sufficient evidence for a privacy claim
- [X] T038 [US3] **Answered 2026-08-08: the PID is there, but not for every application.** `pw-dump` shows `application.process.id` on native PipeWire clients (Zen: 2238262) and *absent* on ALSA-compatibility streams (`alsa_playback.elementium`). One application can also own several nodes — Zen has two sharing a PID — so "the application's audio" is one-to-many, not one node. See research R8. The fallback is therefore a path real users will hit, not a theoretical safeguard. Original task follows. **Open question, investigate before assuming**: establish whether the window chosen through the portal can be correlated to that application's PipeWire audio node (PID via `application.process.id` is the candidate handle), recording the answer in `specs/008-screen-share-capture/research.md`. If it cannot, the spec's documented fallback ships unchanged and this is a finding, not a failure
- [X] T039 [US3] Scope share audio to the shared application when T038 established that it is possible, in `src-tauri/src/commands/screen_capture.rs`, otherwise capture the desktop mix and set `audioScopeFallback: true` — silently sending more audio than the user expected is the privacy failure this guards
- [X] T040 [US3] Surface the scope fallback in the UI path in `frontend/src/shim/media-devices.ts` so the disclosure reaches the user rather than only the log

---

## Phase 6: US4 — X11 parity (P3)

- [X] T041 [US4] **Wired 2026-08-08, untested against a real X11 display** (this machine is Wayland). Wire the X11 path in `crates/elementium-screen/src/x11.rs` through `ShareSession` and the shared video pipeline, so an X11 session gets the same outcome as Wayland. **Seam located 2026-08-08 and it is not inside `x11.rs`**: `ShareSession` carries only a `PipeWire` node id for the media layer to *pull* from, while `X11Capturer` *pushes* through a callback it drives from its own thread. There is no X11 node id and no callback slot on the session. Closing this needs an X11 variant of `ShareSession` in `crates/elementium-screen/src/share.rs` carrying a source id, plus a consumer in `crates/elementium-media/` that drives a callback source rather than assuming every source is a node to pull
- [X] T042 [US4] Report a specific, accurate failure when neither a portal nor an X11 display is available. **Landed in `crates/elementium-screen/src/x11.rs`, not `lib.rs` as planned** — the silent path was `X11Capturer::start`, which parsed the source id and looked up the target *inside* the spawned thread after already returning `Ok(())`, so a bad id or missing display produced a capturer that succeeded and never called back. Both checks now run before the thread is spawned, and the four faults (no display, unknown target, malformed id, backend failure) are four distinct messages

---

## Phase 7: Polish & cross-cutting

- [X] T043 Handle a second share started while one is running in `src-tauri/src/commands/screen_capture.rs` — refuse or replace, defined either way, never two capturers contending for a portal session
- [X] T044 [P] Check the negotiated encode target against full-monitor geometry using the T003 example and record the result in `specs/008-screen-share-capture/quickstart.md` — a 4K monitor through a path tuned for 1280x720 may fail at negotiation or produce unusable bitrates, and that must surface here rather than as a suspected transport fault
- [X] T045 [P] **Passed 2026-08-08**: ten open/close cycles leave 0 `PipeWire` objects and 1 thread, with both metrics validated against their capturing state (1 object, 2 threads) so that a zero reading means something. Re-run the T001 teardown measurement over ten start/stop cycles and record the after-figure in `specs/008-screen-share-capture/quickstart.md` (SC-006)
- [X] T046 **Passed 2026-08-08** (clippy clean, workspace tests pass, tsc clean, 16/16 vitest). Verify `cargo clippy --workspace -- -D warnings` reports zero, `cargo test --workspace` passes, and `just test-frontend` passes, before the feature is called done

---

## Dependencies

```text
Phase 1 (Setup)
   ↓
Phase 2 (Routing) ───── BLOCKING: nothing below is observable without it
   ↓
Phase 3 (US1) ── screen video visible ← MVP
   ↓
   ├──▶ Phase 4 (US2) ── window scoping   [independent of Phase 5]
   ├──▶ Phase 5 (US3) ── share audio      [independent of Phase 4]
   └──▶ Phase 6 (US4) ── X11 parity       [independent of both]
                ↓
        Phase 7 (Polish)
```

Within Phase 2, T004→T005→T006→T007 is a strict chain: each needs the previous type change
to compile. T010→T011→T012 likewise. T009 can be written as soon as T007 lands.

Within Phase 3, T013→T014 is strict (extraction before generalisation, for the lint cap).
T015, T016 and T022 are independent of each other. T018 needs T014, T015, T016 and T017.

Phases 4, 5 and 6 are mutually independent once Phase 3 is done and can be taken in any
order, or concurrently.

## Parallel opportunities

- **Phase 1**: T002 and T003 alongside T001.
- **Phase 2**: none worth taking — it is a type change rippling through call sites, and
  parallel edits to the same signatures conflict more than they save.
- **Phase 3**: T015, T016 and T022 touch three different crates and can proceed together.
  T026 (diagnostics) can be written alongside any of them.
- **Phase 5**: T030 and T031 are separate modules; T037 and T038 are investigations that
  need no code.
- **Phase 7**: T044 and T045 are measurements and independent.

## Implementation strategy

**MVP is Phase 2 + Phase 3.** That is screen sharing that works — silently, monitors only,
Wayland only. It is the whole of US1, it removes the black rectangle, and it is worth
shipping before the rest exists.

**Then Phase 5 (audio)** rather than Phase 4, on the judgement that a silently shared video
is the more common complaint than a whole-monitor share when a window was wanted. Both are
P2 in the spec; this is a tie-break, not a re-prioritisation, and the order can be swapped
without consequence.

**Phase 4 and Phase 6** are the smaller remainder. Phase 6 in particular may reduce to very
little or prove awkward depending on how much of `x11.rs` still works — it is sequenced last
deliberately so that finding out does not block anything.

---

## Phase 8: Convergence

Appended 2026-08-08 after assessing the code against spec, plan and tasks. Everything here
is remaining work the earlier phases did not cover, ordered by severity.

- [X] T051 **Done** — the line now reads `capture frame received` and carries the source; the description below is the bug as it was. Name the source in the frame-flow log line in `src-tauri/src/commands/media_devices.rs` per FR-012 (partial) — `video_pipeline_loop` serves the camera and the screen share, and logs `"Camera frame received"` for both. The one diagnostic that says frames are flowing names the wrong device for half its callers, so grepping a log for a share's health finds camera lines or nothing
- [X] T052 **Passed in full 2026-08-08.** Delivery: 500 packets, 0 lost, 0 concealed samples at a real browser. Independence: the publisher now sends a synthetic microphone alongside share audio and mutes it half way through — after the mute the microphone gains 0 packets while share audio gains 500, so neither track silences the other. Verify SC-004 end to end and record it in `specs/008-screen-share-capture/quickstart.md` (missing) — share audio is captured, keyed `screen_share_audio`, and publishable, but no run has confirmed a tone from the shared source reaching a receiver, nor that muting the microphone silences only speech. Publishing without hearing is exactly the shape of failure this feature exists to remove
- [ ] T053 Exercise the `audio: false` branch of `get_display_media` in `src-tauri/src/commands/screen_capture.rs` against the running app and record it (partial, SC-005 / T037) — the opt-out is currently proven only for the capture path via the example, which never asks for audio at all. The guard a user's choice actually flows through is untested, and this is a privacy claim
- [X] T054 **Measured 2026-08-08: 309/333/384/761ms (min/median/mean/max) for the capture half — write to window, change visible in a captured frame.** Scope is stated in quickstart: this excludes encode, SFU, network and the receiver's decoder, so it bounds SC-002 from below rather than confirming it. Measure SC-002 (missing) — "content changed on the shared source is visible to the receiver within one second" has no measurement anywhere. A damage-driven screencast makes this less obvious than it sounds: the clock starts at the damage event, not at a frame tick
- [ ] T055 [US4] Run the X11 path against a real X11 display (partial) — `crates/elementium-screen/src/x11.rs` and the push-fed `VideoSource` compile and unit-test, but no X11 session has ever exercised them. Until then US4 is written, not delivered
- [X] T056 **Done** — T025 is now marked complete. Reconcile T025 (it was left open while already satisfied) — the attended SC-001 browser test in `frontend/tests/browser/receive-path.spec.ts` already asserted what T025 asked for, so its checkbox was ticked rather than the work repeated
