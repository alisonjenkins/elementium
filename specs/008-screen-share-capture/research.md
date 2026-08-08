# Phase 0 Research: Screen and application sharing, with audio

**Created**: 2026-08-08

Everything below was established by reading the code and probing the running system, not
by recall. Where a claim is inferred rather than observed it says so.

---

## R1: Where the video actually has to come from

**Decision**: Reuse `VideoSource` by adding a screencast constructor that connects to a
PipeWire node id the portal supplies, and generalise `camera_pipeline_loop` into a
source-agnostic pipeline loop.

**Rationale**: `elementium_media::video_source::VideoSource` is an enum over
`{PipeWire, V4L2}` exposing exactly four operations — `start_at`, `try_recv`, `size`,
`stop`. `WaylandCapturer` already ends at a PipeWire node id and already hands it to
`PipewireCapturer::start(node_id)`, which is the same type the camera path drives. The
screencast case is therefore not a new source kind; it is the existing PipeWire source
opened against a node the portal chose rather than a node the camera enumeration found.

`camera_pipeline_loop` (`src-tauri/src/commands/media_devices.rs:617`) is 100+ lines of
encoder negotiation, keyframe policy, preview throttling, pacing and E2EE-bound
dispatch. All of it applies unchanged to screen content. Duplicating it for screen share
would create two places for every future encode fix.

**Alternatives considered**:

- *A separate screen pipeline loop.* Rejected: it duplicates the encode path, and the
  spec explicitly assumes one pipeline. The parts that genuinely differ (geometry, frame
  rate, keyframe cadence for static content) are parameters, not a different algorithm.
- *Driving the existing `ScreenCapturer` trait callback into the encoder.* Rejected: the
  trait is push-based (`Box<dyn Fn(I420Frame)>`) while the pipeline loop is pull-based
  (`try_recv`), and bridging them adds a queue and a thread that `PipewireCapturer`
  already provides internally.

---

## R2: The structural blocker — one track slot per kind

**Decision**: This is the largest piece of work in the feature and must be done before
anything else can be observed working.

**Observed**, in three places that all say the same thing:

1. `MediaState` (`src-tauri/src/commands/media_devices.rs:60`) holds
   `camera: Mutex<Option<CameraPipelineHandle>>` and
   `audio_capture: Mutex<Option<AudioCaptureHandle>>`. Singletons. Starting a screen
   share through the camera slot would stop the camera.
2. `adopt_idle_pipelines` (`src-tauri/src/commands/webrtc.rs:144`) attaches "the camera"
   and "the microphone" to a new peer connection by looking in those two slots. With two
   video pipelines it has no way to say which is which.
3. `IoCommand::WriteVideo(PlaintextMedia, VideoCodec)` and `WriteAudio(PlaintextMedia)`
   (`crates/elementium-webrtc/src/engine.rs:26-40`) **carry no track identity**, and
   `peer_connection` keeps a single `video_mid` / `audio_mid` pair for sending. Two
   simultaneous video tracks have two mids and no way to address them.

**Rationale for the ordering**: (3) is the deepest. Until a write can name its track,
a second video track cannot reach the wire no matter what the layers above do. (1) and
(2) are then mechanical. This is why the plan sequences routing first and treats "screen
share visible" as depending on it, rather than starting at the frontend where the most
visible bug is.

**Alternatives considered**:

- *Replace the camera track while sharing* (what some clients do — one video track, swap
  the source). Rejected: it violates FR-007's sibling expectation that a user's camera
  and their share coexist, it is visibly wrong in a call UI, and LiveKit models them as
  distinct sources (`TrackSource::Camera` vs `ScreenShare`) which the SFU and every
  other participant's UI relies on.
- *A second peer connection for the share.* Rejected: doubles ICE, DTLS and E2EE state
  for no benefit; LiveKit publishes multiple tracks on one publisher connection by
  design.

---

## R3: LiveKit already knows about screen share

**Decision**: No protocol work needed. Call the existing API with the right argument.

**Observed**: `publish_track_inner` (`crates/elementium-webrtc/src/livekit/room.rs:491`)
already maps `"screen_share"` → `TrackSource::ScreenShare` and `"screen_share_audio"` →
`TrackSource::ScreenShareAudio`. `publish_video_track(source, codec, width, height)`
takes the source as its first parameter. The `cid` is derived as
`{local_sid}-{kind}-{source}`, so camera and screen video already produce distinct cids
and will pair with distinct m-lines without change.

This is a pleasant finding: the protocol layer was written with screen share in mind and
is simply never invoked with it. It also means the `cid` scheme gives us the per-track
key that R2 needs, rather than inventing one.

---

## R4: Audio cannot come from the portal

**Decision**: Capture share audio directly from PipeWire, as a second audio pipeline,
using the `pipewire` crate already in the dependency tree.

**Rationale**: The XDG ScreenCast portal has no audio in its interface, in any backend.
This is not a limitation of the backend installed here — it is absent from the portal
API. Every Linux application that shares audio with a screen (browsers included) obtains
it from the audio server separately. So share audio is necessarily a second capture with
its own lifetime, not a flag on the video session. The spec already assumes this.

**Observed on this machine** (`pw-dump`): PipeWire exposes per-application output
streams as distinct nodes with `media.class = Stream/Output/Audio` and readable names
(`Zen`, `OBS: Game Audio`), alongside device sinks with `media.class = Audio/Sink`
(`easyeffects_sink`, the Focusrite interface). Capturing an application's audio means
connecting a capture stream to that application's node; capturing the desktop means
capturing a sink's monitor.

**The correlation problem, stated honestly**: the portal returns a *video* node id for
the chosen window. Mapping that to the *audio* node of the same application is not
something the portal gives us. The available handle is the PID: the portal's window
selection and PipeWire's `application.process.id` property can in principle be
correlated. Whether niri's Mutter ScreenCast implementation exposes the source window's
PID through the portal response has **not** been verified and is the one genuine unknown
in this feature. The plan therefore sequences desktop-mix audio first (which needs no
correlation) and treats per-application audio as a refinement that may prove
unavailable — in which case the spec's stated fallback applies: use the desktop mix and
tell the user.

**Alternatives considered**:

- *`cpal` for the monitor source*, reusing the microphone path. Rejected: `cpal` on Linux
  goes through ALSA/PulseAudio compatibility and does not model PipeWire per-application
  nodes at all, so it forecloses R4's refinement entirely. The `pipewire` crate is
  already a dependency for video capture.
- *A `parec`/`pw-record` subprocess.* Rejected: process management, no structured error
  reporting, and an extra copy of every sample.

---

## R5: The frontend track is a preview, not the wire

**Decision**: The `getDisplayMedia` fix is the same shape as the working `getUserMedia`
path — size a canvas from the first real frame, attach it, and pump native frames into
it — not a new mechanism.

**Rationale**: This corrects a natural misreading of the bug. The canvas track in
`getUserMedia` is *not* what gets encoded; encoding happens in Rust from the native
capture. The canvas exists so the page has a `MediaStreamTrack` object to hold and so
local self-view renders. Media that reaches the far end never passes through it.

`getUserMedia` (`frontend/src/shim/media-devices.ts:131-181`) does the full job:
`firstFrameGeometry(id)` to size the canvas *before* `captureStream` (the comment there
records that resizing afterwards causes stride-mismatch banding — a fault already found
and fixed once), attaches it in-viewport at zero opacity (a compositor-sampling fault
also already found and fixed), then `startLocalVideoFrameFetch` pumps
`invoke("get_video_frame", {trackId})`.

`getDisplayMedia` (lines 209-217) does none of this: fixed 1920x1080, no attachment, no
fetch, and the native track id discarded. Both prior fixes are absent from it.

**Consequence for effort**: the frontend fix is small and its correct form is already
written 70 lines above. The temptation to "just paint the canvas" must not skip
`firstFrameGeometry`, or the banding fault returns on a path where nobody is looking for
it.

---

## R6: The backend/frontend disagreement

**Decision**: Select the capture backend once, at session start, and use the same one to
enumerate and to start.

**Observed**: `get_capture_sources` tries X11 and falls back to Wayland;
`get_display_media` unconditionally constructs `X11Capturer`. On this machine
(`XDG_SESSION_TYPE=wayland`, niri) the fallback fires for enumeration while the start
path still attempts X11 through XWayland.

**Rationale**: The two commands are separate IPC calls with no shared state, so
"whichever worked last time" is not available to the second. The backend choice should be
made once and held for the share session, which R2's session object provides a home for.

---

## R7: Environment, verified

Probed rather than assumed, because the feasibility of US2 depends on it:

| Fact | Value | How established |
|---|---|---|
| Session type | Wayland | `XDG_SESSION_TYPE=wayland` |
| Compositor | niri | `XDG_CURRENT_DESKTOP=niri` |
| ScreenCast portal backend | xdg-desktop-portal-gnome | `niri-portals.conf` sets `default=gnome;gtk;` |
| Window selection available | Yes | niri owns `org.gnome.Mutter.ScreenCast` on the session bus, which is the interface the GNOME portal backend drives |
| Per-application audio nodes | Yes | `pw-dump` shows `Stream/Output/Audio` nodes named per application |
| PipeWire tooling present | Yes | `pw-cli`, `pw-dump`, `wpctl` |

Note the significance of the portal backend: had this been `xdg-desktop-portal-wlr`
(also installed, but not preferred), only monitor capture would exist and US2 would be
untestable here. It is worth re-checking this if the machine's portal configuration
changes, because a US2 failure would then be environmental rather than ours.

---

## R8: the PID handle exists, but not for every application — probed 2026-08-08

Half of the open question below is now answered by measurement rather than assumption.
`pw-dump` on this machine, with the audio classes filtered:

| node | class | `application.process.id` |
|---|---|---|
| `Zen` | `Stream/Output/Audio` | 2238262 |
| `Zen` | `Stream/Output/Audio` | 2238262 |
| `alsa_playback.elementium` | `Stream/Output/Audio` | *absent* |
| `alsa_playback.publish_test_tone` | `Stream/Output/Audio` | *absent* |
| `easyeffects_sink` | `Audio/Sink` | *absent* |
| `alsa_output.…Scarlett_2i2…` | `Audio/Sink` | *absent* |

Two findings, both material, and the second was not anticipated:

1. **The PID is present for native PipeWire clients and absent for ALSA-compatibility
   ones.** A browser reports it; a program playing through the ALSA shim does not. So
   application-scoped audio is genuinely reachable for the applications users most often
   share (browsers, media players), and genuinely unreachable for others. The spec's
   fallback is therefore a path that will be taken in practice, not a theoretical
   safeguard — which is exactly why the disclosure requirement attached to it matters.
2. **One application can own several nodes.** Zen has two, sharing a PID. "Capture the
   application's audio" is therefore not "connect to its node" — it is one-to-many, and
   picking the first would silently drop whatever the other carries. Whether to capture
   all of an application's nodes or to mix them is an open design point for T039.

Sink nodes carry no PID, as expected: a sink belongs to no single process. Capturing the
desktop mix means capturing a sink's monitor, and needs no correlation at all — which is
why it is sequenced first.

## R9: the compositor offers DMA-BUF only — measured 2026-08-08, and it blocks US1

Attempting a real capture through the finished pipeline fails at negotiation:

```text
PipeWire capture stream state old=Paused new=Error("no more input formats")
```

Asking the granted node what it actually offers (`pw-cli enum-params <id> EnumFormat`,
with a window shared from niri via xdg-desktop-portal-gnome):

```text
objects: 2
fmt=BGRx   modifier=MANDATORY size=1880x1446  framerate=0/1  maxFramerate=1..119999/1000
fmt=BGRA   modifier=MANDATORY size=1880x1446  framerate=0/1  maxFramerate=1..119999/1000
```

Three findings, in descending order of how much they cost:

1. **Every format carries a mandatory `modifier` property and there is no shm variant.**
   The property flags are `0x18` — `MANDATORY | DONT_FIXATE`. A client whose `EnumFormat`
   omits `modifier` cannot match a format that mandates it, which is exactly the error
   above. The modifier list does include `0` (`LINEAR`) and `0xFFFFFFFFFFFFFF`
   (`DRM_FORMAT_MOD_INVALID`, the implicit/no-modifier marker), so a client that
   *negotiates* modifiers can steer it, but one that ignores them cannot.

   This crate deliberately restricts buffer types to `MemPtr` and `MemFd`, with a comment
   recording why: a DMA-BUF buffer's memory is a GPU handle that `MAP_BUFFERS` cannot map,
   and a source that picks it delivers nothing at all. That restriction is correct for what
   the code can currently read — and it means **screen capture on this compositor requires
   DMA-BUF import**, which is spec 005's T010. That task was filed as an optimisation for
   the camera path. It is a hard prerequisite here.

2. **The frame rate is a fixed `0/1`, not a range.** In `PipeWire` that means *variable* —
   a compositor emits on damage, not on a clock. Our parameter offered a range starting at
   `1/1`, whose intersection with a fixed `0/1` is empty. Fixed independently of (1); it
   would have blocked negotiation on its own even with modifiers handled.

3. **Size was never the problem.** The first diagnosis guessed the 5120x1440 monitor
   exceeded the 4096 cap in the camera parameters. The share was a *window*, at 1880x1446,
   well inside it. The cap was raised anyway — an 8K bound is right for a screen where a
   camera bound is not — but it was not causal, and recording that here is the difference
   between a fixed bug and a coincidence.

**Consequence for the feature**: US1 cannot be demonstrated on this machine until DMA-BUF
import exists, or until a compositor/portal combination that offers an shm fallback is
used. Everything above the capture negotiation — routing, pipeline, publish, teardown,
frontend — is in place and independently tested; this is the one remaining link.

## R10: offering a modifier clears negotiation and is still not enough — measured 2026-08-08

R9 left one cheap route open: offer a `modifier` property listing only the two
non-hardware entries (`DRM_FORMAT_MOD_INVALID`, `DRM_FORMAT_MOD_LINEAR`) and see whether
the compositor falls back to a mappable buffer. It was tried, against the same real node.

Half of it works. `no more input formats` is gone and the format is agreed:

```text
PipeWire capture format negotiated width=1880 height=1446 encoding=Raw(Bgrx)
PipeWire capture stream state old=Paused new=Error("error alloc buffers: Invalid argument")
```

The failure simply moves one step later, to buffer allocation, and zero frames arrive
(`delivered rate: 0.0fps against 30 requested`). The compositor accepts our pixel format
and then allocates DMA-BUF regardless of the modifier offered; `MemPtr`/`MemFd` cannot
satisfy that.

**This settles the question R9 left open: DMA-BUF import is unavoidable on this
compositor.** The modifier property is kept anyway — it is a correct, confined step that
a screencast node genuinely requires to negotiate, and without it the DMA-BUF work would
fail earlier for a second, unrelated reason. Keeping it means the next attempt fails in
exactly one place instead of two.

## Open question carried into implementation

**Q**: Can the window chosen through the portal be correlated to that application's
PipeWire audio node?

R8 establishes the *audio* half: a PID is available on the node, for some applications.
What remains unverified is the *video* half — whether niri's Mutter ScreenCast
implementation exposes the chosen window's PID through the portal response at all. Without
that there is nothing to match the node's PID against, and the fallback applies regardless
of how good the audio-side handle is.
