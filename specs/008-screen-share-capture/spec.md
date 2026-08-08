# Feature Specification: Screen and application sharing, with audio

**Created**: 2026-08-08
**Status**: Draft

## Why this exists

Screen sharing does not work. Not "works badly" — a remote participant sees a black
rectangle, and has done since the feature was written. Three separate breaks stack up,
and each one alone would be enough to produce that result:

1. The browser-facing `getDisplayMedia` shim asks the native side to start a capture,
   throws away the track identifier it gets back, and returns a freshly created
   1920x1080 canvas that nothing ever paints. The page is handed a black track by
   construction.
2. The native `get_display_media` command opens a frame channel, keeps the sending
   half, and drops the receiving half on the floor. Its own comment says so:
   `TODO: Wire frame_rx into the video pipeline for encoding and transmission`.
   Frames, if any were captured, would be discarded immediately.
3. Source enumeration falls back from X11 to the Wayland portal, but starting a
   capture always constructs the X11 capturer. On a Wayland session the two halves
   disagree about which backend is in use.

Separately, there is no desktop or application audio anywhere in the codebase. Sharing
a video call, a game, or a music player currently shares it silently. The audio capture
that exists records the default input device — a microphone — which is a different
thing serving a different purpose.

This matters more than its size suggests. Screen sharing is table stakes for a calling
application; a user who tries it once and sees black does not try it again, and has no
way to tell that the problem is ours rather than theirs.

## What is already true

Worth stating so the work is not re-derived:

- `WaylandCapturer` is real and correct. It opens an XDG ScreenCast portal session,
  gets a PipeWire node id back from the user's pick, and pumps frames through the same
  `PipewireCapturer` path the camera already uses. It is simply never called from
  `get_display_media`.
- The video encode, E2EE and publish path downstream of a captured frame works — it is
  the path the camera uses today, verified end to end.
- On the development machine the ScreenCast portal is served by
  xdg-desktop-portal-gnome, and the compositor (niri) implements
  `org.gnome.Mutter.ScreenCast`. Both monitor and window selection are therefore
  available; this is not the monitor-only wlroots backend.
- PipeWire exposes per-application output streams as distinct nodes, alongside
  device-level sinks. Application-scoped audio is reachable, not just a whole-desktop
  mix.
- The ScreenCast portal provides **no audio**, in any backend. Audio for a share cannot
  come from the same portal session as the video and must be obtained separately.

## User Scenarios

### US1 (P1) — A shared screen is visible to the other participant

A user in a call chooses to share their screen, picks a monitor, and the other
participant sees their screen updating live. Ending the share stops it.

**Why this priority**: it is the whole feature. Everything else is an improvement on a
thing that must first exist.

**Independent test**: join a local call from two endpoints, share a monitor from one,
and confirm the other renders changing content — not a still frame, not black. Verified
by frame counters advancing at the receiver, not by eye alone.

**Acceptance Scenarios**:

1. **Given** an active call, **When** the user starts a screen share and picks a
   monitor, **Then** the remote participant receives a video track whose decoded frame
   count increases over time.
2. **Given** an active screen share, **When** content on the shared monitor changes,
   **Then** the change appears at the remote participant.
3. **Given** an active screen share, **When** the user stops sharing, **Then** the
   track ends, the capture session is torn down, and no capture thread is left running.
4. **Given** a screen share is starting, **When** the user cancels the system picker,
   **Then** the share does not start and the call continues undisturbed.

---

### US2 (P2) — The user can share one window rather than everything

A user shares a single application window instead of a whole monitor, and only that
window's contents leave the machine.

**Why this priority**: the common case in practice, and a privacy property rather than
a convenience — sharing a whole desktop when you meant to share one window leaks
whatever else is on screen. It is separated from US1 because it depends on which
picker backend is present and can be delivered after monitors work.

**Independent test**: share a single window, then change content in a *different*
window, and confirm the change does not appear at the receiver.

**Acceptance Scenarios**:

1. **Given** a picker that supports window selection, **When** the user picks one
   window, **Then** only that window's contents are transmitted.
2. **Given** the shared window is closed mid-share, **When** the capture ends,
   **Then** the share stops cleanly and the user is told why.

---

### US3 (P2) — The shared content's audio is heard too

A user shares a video, a game, or a music player and the other participant hears it,
in addition to — not instead of — the user's microphone.

**Why this priority**: a silently shared video is close to useless, but the feature
delivers value without it, so it does not gate US1.

**Independent test**: share a source playing a known tone, with the microphone also
live, and confirm the receiver's audio contains both the tone and speech.

**Acceptance Scenarios**:

1. **Given** a user starting a share, **When** they opt to include the shared
   content's audio, **Then** the remote participant hears that audio.
2. **Given** share audio is enabled, **When** the user also speaks, **Then** the remote
   participant hears both, and muting the microphone silences only the microphone.
3. **Given** a user starting a share, **When** they do not opt in to audio, **Then** no
   audio beyond the microphone is captured or transmitted.
4. **Given** an active share with audio, **When** the share stops, **Then** audio
   capture for it stops with it.

---

### US4 (P3) — Sharing behaves the same on X11

A user on an X11 session gets the same outcome as one on Wayland, or is told plainly
which part is unavailable.

**Why this priority**: the primary development and target environment is Wayland; X11
must not silently produce the black rectangle this feature exists to remove.

**Independent test**: run the share flow under an X11 session and confirm either a
working share or a specific, accurate failure message.

---

### Edge Cases

- The user cancels the source picker. Must be an ordinary outcome, not an error state,
  and must not disturb the ongoing call.
- No portal is available and no X11 display is present. The share must fail with a
  message naming the missing piece, not a generic failure.
- The shared window or monitor disappears mid-share (window closed, display
  unplugged).
- The user starts a second share while one is running.
- The shared source produces frames at a size or rate the encoder is not already
  configured for — a 4K monitor is not a 1280x720 webcam.
- Share audio is requested but the selected source produces no audio at all.
- The user shares the call window itself, producing visual feedback.

## Requirements

### Functional Requirements

- **FR-001**: Starting a screen share MUST result in captured frames reaching the
  remote participant, through the existing encode and publish path.
- **FR-002**: The video track handed to the calling page MUST carry the captured
  frames. A track that is created but never fed does not satisfy this.
- **FR-003**: The system MUST use the same capture backend for enumerating sources and
  for starting a capture within a single session.
- **FR-004**: On Wayland, source selection MUST be delegated to the system picker; the
  application MUST NOT present its own list of windows.
- **FR-005**: Users MUST be able to share either a whole monitor or a single window,
  where the platform picker offers both.
- **FR-006**: Users MUST be able to choose, at the point of starting a share, whether
  the shared content's audio is included.
- **FR-007**: Shared-content audio MUST be transmitted in addition to the microphone,
  and MUST be independently stoppable.
- **FR-008**: Shared-content audio MUST NOT be captured when the user has not asked
  for it.
- **FR-009**: Stopping a share MUST release the capture session, stop the associated
  threads, and stop any audio captured for that share.
- **FR-010**: A share that cannot start MUST report why, specifically enough to
  distinguish user cancellation from a missing platform capability from a failure.
- **FR-011**: Shared video MUST be protected by the call's existing end-to-end
  encryption, on the same terms as camera video.
- **FR-012**: The system MUST emit structured, queryable diagnostics for the share
  lifecycle — source chosen, capture started, frames flowing, stopped, failed —
  without recording the pixel content or audio of what is shared.

### Key Entities

- **Share session**: one user's act of sharing, from picker to teardown. Owns a video
  capture and optionally an audio capture, and outlives neither.
- **Capture source**: what the user picked — a monitor, a window, or an application.
  Identified differently by each platform backend; the identifier is opaque above the
  backend.
- **Share audio stream**: audio belonging to the shared content, distinct in origin
  and in lifetime from the microphone stream.

## Success Criteria

- **SC-001**: A screen share started in a call produces a steadily increasing decoded
  frame count at the receiving participant, sustained for at least 30 seconds.
- **SC-002**: Content changed on the shared source is visible to the receiver within
  one second.
- **SC-003**: Sharing a single window transmits nothing from outside that window,
  demonstrated by changing content elsewhere and observing no change at the receiver.
- **SC-004**: With share audio enabled, a tone played by the shared source is present
  in the receiver's audio while the microphone remains independently audible and
  independently mutable.
- **SC-005**: With share audio not requested, no audio stream beyond the microphone is
  opened — verifiable from the system's own audio graph, not only from the code.
- **SC-006**: Stopping a share leaves no capture thread, no portal session and no audio
  stream behind, verifiable by inspecting the process after ten start/stop cycles.
- **SC-007**: Every failure to start a share produces a distinct, accurate reason;
  cancelling the picker is not reported as an error.
- **SC-008**: No code path can hand the page a video track that is not backed by a
  capture.

## Assumptions

- **Linux only for now.** macOS and Windows capture backends are explicitly out of
  scope, as stated in the request. The abstraction should not be shaped so as to make
  them impossible later, but no work is done toward them here.
- **Audio does not come from the portal**, because the portal does not offer it. It is
  sourced from the audio graph directly. This is how other Linux applications solve
  the same problem, and is not a workaround around a facility we are ignoring.
- **Audio scope follows the share scope where the platform allows it** — sharing a
  window prefers that application's own audio; sharing a monitor takes the desktop
  mix. Where the application's audio cannot be isolated, the desktop mix is used and
  the user is told, because silently sending more audio than the user expected is a
  privacy failure and must not be silent.
- **The existing camera video path is reused** — capture to encode to E2EE to publish.
  No second pipeline. Screen content differs from camera content in size, rate and
  compressibility, and this may need tuning, but tuning an existing path is the
  assumption, not building a parallel one.
- **One share at a time per user.** Simultaneous shares are not supported and starting
  a second replaces or is refused, not silently layered.
- **Element Web's existing share UI is the entry point.** No new picker UI is built by
  this feature beyond what the platform provides; the system picker is the picker.
- **The development machine's configuration** (Wayland, niri, xdg-desktop-portal-gnome,
  PipeWire) is representative for verification, but requirements are written against
  capabilities rather than against that specific stack.

## Out of scope

- macOS and Windows capture backends.
- Remote control of a shared screen.
- Sharing to a recording or to a file rather than to a call.
- Multi-source simultaneous sharing.
- Any change to microphone capture, which already works.
