# Implementation Plan: Screen and application sharing, with audio

**Branch**: `008-screen-share-capture` | **Date**: 2026-08-08 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/008-screen-share-capture/spec.md`

## Summary

Screen sharing sends a black rectangle. Three defects stack (the frontend discards the
native track and paints nothing; the backend drops the frame receiver; the backend choice
disagrees between enumerate and start), and there is no desktop or application audio at
all.

The approach is deliberately not "fix the three defects". Reading the code turned up a
constraint underneath them: **a second video track has nowhere to go**.
`IoCommand::WriteVideo`/`WriteAudio` carry no track identity, `peer_connection` holds one
send mid per kind, `MediaState` has one camera slot and one microphone slot, and
`adopt_idle_pipelines` attaches "the camera" by looking in that slot. Fixing the visible
three without this yields a share that captures correctly and still shows nothing —
frames written to the camera's mid, sender counters advancing, no picture anywhere. That
failure looks exactly like success from the sending side, which is why it is worth naming
before starting.

So: introduce a per-track routing key first, then reuse everything. `VideoSource` gains a
screencast constructor (the portal already ends at a PipeWire node id, which is what the
camera path already consumes), `camera_pipeline_loop` becomes source-agnostic, and
LiveKit needs no protocol work at all — `TrackSource::ScreenShare` and `ScreenShareAudio`
are already wired in `publish_track_inner` and simply never called. Share audio is a
second PipeWire capture with its own lifetime, because the ScreenCast portal has no audio
in any backend.

## Technical Context

**Language/Version**: Rust (workspace, 2024 edition idioms; strict deny-lints) + TypeScript (frontend shims)

**Primary Dependencies**: `pipewire` (video capture today, audio capture new), `ashpd` (XDG portal), `str0m` (WebRTC), `livekit-protocol`, `tauri`, `openh264`/VAAPI via `elementium-codec`, `cpal` (microphone only — deliberately not extended, see research R4)

**Storage**: none — all state is in-memory for the lifetime of a share

**Testing**: `cargo test --workspace`, `vitest` (`just test-frontend`), Playwright (`just test-browser`) against the local MatrixRTC stack; plus out-of-process verification via `pw-dump` for the audio-opt-out claim

**Target Platform**: Linux/Wayland primarily (niri + xdg-desktop-portal-gnome verified present), X11 as a parity path. macOS and Windows explicitly out of scope.

**Project Type**: Desktop application — Tauri shell, Rust media/WebRTC workspace, patched Element Web frontend

**Performance Goals**: sustained capture at the requested rate for full-monitor geometry; changes visible to the receiver within 1s (SC-002); no regression to the camera path's measured per-frame cost

**Constraints**: workspace denies `unwrap_used`, `expect_used`, `indexing_slicing`, `arithmetic_side_effects`, `as_conversions`, `panic`, `missing_const_for_fn`, and caps functions at 100 lines (`too_many_lines`). `camera_pipeline_loop` is already close to that cap, so generalising it requires extraction, not addition. E2EE key material must never be logged. Share audio must not be captured when not requested — a privacy constraint, verified from the audio graph rather than from the code.

**Scale/Scope**: one share at a time per user; two simultaneous video tracks (camera + screen) and two audio tracks (microphone + share) as the new maximum

## Constitution Check

`.specify/memory/constitution.md` is an **unfilled template** — every principle is still a
`[PRINCIPLE_N_NAME]` placeholder. Constitution gates are therefore skipped rather than
fabricated, per the documented graceful-skip behaviour.

The project's real governing constraints live in the workspace lint configuration and in
the user's global instructions (atomic commits that individually revert cleanly; never
squash; ISO8601 UTC in files and logs; ask before mutating live infrastructure). Those are
honoured and are what the task breakdown is sequenced around — in particular, every task
below is intended to leave the workspace compiling on its own.

**Re-evaluated after Phase 1 design**: no violations introduced; nothing to record in
Complexity Tracking. The one structural expansion (keyed pipelines replacing two
singleton slots) reduces duplication rather than adding a layer — the two existing slots
already shared a `CapturePipeline` trait precisely because the code needed to treat them
uniformly.

## Project Structure

### Documentation (this feature)

```text
specs/008-screen-share-capture/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0 output — seven findings, one open question
├── data-model.md        # Phase 1 output — MediaTrackKey, PipelineHandle, ShareSession
├── quickstart.md        # Phase 1 output — seven validation levels
├── contracts/
│   └── tauri-commands.md
├── checklists/
│   └── requirements.md
└── tasks.md             # Phase 2 — created by /speckit-tasks, not by this command
```

### Source Code (repository root)

```text
crates/
├── elementium-webrtc/src/
│   ├── engine.rs                 # IoCommand gains a MediaTrackKey  ← the load-bearing change
│   ├── peer_connection.rs        # send mids become per-key, not one per kind
│   └── livekit/room.rs           # already supports screen_share; called, not changed
├── elementium-media/src/
│   ├── video_source.rs           # + screencast constructor over an existing PipeWire node
│   ├── pipewire_capture.rs       # reused unchanged for screen video
│   └── pipewire_audio.rs         # NEW — share audio capture (sink monitor / app node)
├── elementium-screen/src/
│   ├── traits.rs                 # backend selection; audio is not forced into this trait
│   ├── wayland.rs                # already correct — wired up rather than rewritten
│   └── x11.rs                    # parity path
└── elementium-types/src/         # MediaTrackKey, ShareSession types

src-tauri/src/commands/
├── media_devices.rs              # MediaState: two singleton slots → keyed map
├── screen_capture.rs             # get_display_media: actually wire the receiver
└── webrtc.rs                     # adopt_idle_pipelines: adopt by key

frontend/src/shim/
└── media-devices.ts              # getDisplayMedia: mirror the working getUserMedia path
```

**Structure Decision**: Existing workspace layout, unchanged. One new module
(`pipewire_audio.rs`) because share audio has no existing home — `audio_capture.rs` is
cpal/microphone and extending it would drag ALSA-compat semantics into a PipeWire
concern. Everything else is modification of files that already own the relevant
behaviour.

## Approach and sequencing

The order is dictated by what can be *observed*, not by what is easiest.

**Phase A — routing (blocking).** `MediaTrackKey` into `IoCommand`; per-key send mids in
`peer_connection`; `MediaState`'s two singleton slots become one keyed map;
`adopt_idle_pipelines` adopts by key while preserving the existing mid-call connection
inheritance per key. Testable with no camera, no portal and no call — two video tracks,
assert frames land on their own mid. This is Level 1 of the quickstart, and it exists as
its own level because getting it wrong is the failure that most resembles success.

**Phase B — screen video (US1).** `VideoSource` screencast constructor; generalise
`camera_pipeline_loop` (extraction first — it is near the 100-line cap); `ShareSession`
owning backend choice and teardown; `get_display_media` wiring the receiver and choosing
one backend for both enumerate and start; frontend `getDisplayMedia` mirroring the
`getUserMedia` path *including* `firstFrameGeometry` before `captureStream` and in-viewport
attachment. Both of those were fixes already earned once on the camera path and are absent
from the screen path; re-deriving them would be waste.

**Phase C — window scoping (US2).** Mostly falls out of B on this machine, since the GNOME
portal backend offers window selection. The work is the negative test — proving content
outside the chosen window does not leave.

**Phase D — share audio (US3).** `pipewire_audio.rs` capturing a sink monitor; second
audio pipeline keyed `screen_share_audio`; publish via the existing
`publish_track("audio", "screen_share_audio")`. Then the open question from research R4 —
whether the portal's window choice can be correlated to that application's PipeWire node
via PID — investigated explicitly, with the spec's documented fallback (desktop mix,
`audioScopeFallback: true`, user told) shipping if it cannot.

**Phase E — X11 parity (US4)** and cleanup.

## Risks worth stating

- **`camera_pipeline_loop` is near the 100-line lint cap.** Generalising it will fail
  `too_many_lines` unless extraction comes first. Sequenced accordingly rather than
  discovered at commit time.
- **Screen geometry is not webcam geometry.** A 4K monitor through an encode path tuned
  for 1280x720 may fail at negotiation or produce unusable bitrates. Quickstart Level 2
  surfaces this before it can be mistaken for a transport fault.
- **The per-application audio correlation may not be available.** Carried as an open
  question with a shipped fallback, not as an assumption. Named in research R4.
- **The portal backend is environmental.** `xdg-desktop-portal-wlr` is also installed
  here; were it preferred, window capture would not exist and US2 would fail for reasons
  that are not ours. Recorded in research R7 so a future failure is diagnosable.

## Complexity Tracking

No constitution violations to justify — the constitution is an unfilled template and the
design introduces no new architectural layer.
