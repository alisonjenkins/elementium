# Contract: Tauri IPC surface for screen sharing

**Created**: 2026-08-08

The boundary between the Element Web page (via the shims) and the Rust side. This is a
real external interface: the page is upstream code we patch, so anything here that
changes shape has to be matched in `frontend/src/shim/`.

Existing commands are listed with their current behaviour and the required change, so a
reviewer can see what is new versus what is being corrected.

---

## `get_capture_sources` *(exists; behaviour clarified)*

**Request**: none

**Response**: `CaptureSource[]` where `CaptureSource = { id: string, name: string, kind: "monitor" | "window" }`

**Contract change**: an empty array MUST mean "this platform delegates selection to a
system picker", not "no sources exist". The caller MUST NOT treat empty as an error and
MUST NOT substitute a default source id.

**Why it needs stating**: today the frontend responds to an empty list by passing
`sourceId: "default"`, which is a value no backend defines. On Wayland — where the list
is always empty by design — every share therefore starts with a meaningless source id.

---

## `get_display_media` *(exists; currently broken)*

**Request**: `{ sourceId: string, audio: boolean }`

`audio` is new. It carries the user's opt-in from FR-006/FR-008. Absent or `false` MUST
result in no audio capture being opened at all.

**Response**:

```json
{
  "videoTrackId": "video-<hex>",
  "audioTrackId": "audio-<hex> | null",
  "audioScope": "desktop_mix | application | null",
  "audioScopeFallback": false
}
```

**Contract change from today**: currently returns a bare `TrackId` string, and the caller
discards it. The response becomes an object because the audio outcome is not derivable by
the caller — in particular `audioScopeFallback: true` is what obliges the UI to disclose
that the desktop mix was captured when application audio was asked for.

**Guarantees the callee MUST provide**:

1. On success, a capture is running and frames for `videoTrackId` are retrievable via
   `get_video_frame` within a bounded time.
2. On user cancellation of the picker, the command fails with a distinguishable
   cancellation error — not a generic failure, and not a success with a dead track
   (FR-010, SC-007).
3. No capture, portal session or thread is left running if the command returns an error.

**Guarantee the caller MUST provide**: the returned `videoTrackId` is used to drive the
track it hands the page. Returning a track id and rendering something else is the present
defect and is what SC-008 forbids.

---

## `stop_track` *(exists; scope extended)*

**Request**: `{ trackId: TrackId }`

**Contract change**: stopping a screen share's video track MUST also stop that share's
audio track and close the portal session. The share is one thing to the user, so it is
one teardown (FR-009, SC-006).

Stopping the share's *audio* track alone MUST NOT stop the video.

---

## `get_video_frame` *(exists, unchanged)*

**Request**: `{ trackId: string }` → **Response**: frame bytes

Screen tracks use the same buffer and the same command as camera tracks. Listed here
because the frontend fix depends on it being true, and because the shared
`VideoFrameBuffer` is keyed by track id — which is why per-track keys (see data-model)
have to reach it.

---

## Browser-facing contract: `navigator.mediaDevices.getDisplayMedia`

What the shim must satisfy for Element Web, which is not our code:

- Returns a `MediaStream` whose video track is backed by real captured content.
- When `constraints.audio` is truthy, the stream includes an audio track; when it is
  not, it MUST NOT.
- The returned tracks' `stop()` reaches the native teardown. A page-side stop that leaves
  the native capture running is the leak SC-006 tests for.
- Picker cancellation surfaces as a rejected promise with `NotAllowedError`, which is
  what a browser does and therefore what upstream code already handles.

**Sizing note**: the video track must be sized from the first real frame *before*
`captureStream` is called. This is not a style preference — the camera path carries a
comment recording that resizing afterwards produces stride-mismatch banding that looks
like a capture fault but is not. The screen path must not rediscover it.

---

## Internal contract: `IoCommand` routing

Not an external interface, but the one contract change that everything else rests on, so
it is recorded where a reviewer will see it.

`IoCommand::WriteAudio` and `IoCommand::WriteVideo` currently carry no track identity,
and the peer connection holds one send mid per kind. Both MUST carry a `MediaTrackKey`,
and the I/O loop MUST route to the mid published for that key.

**Failure mode if skipped**: a second video track's frames are written to the camera's
mid. The SFU accepts them, counters advance at the sender, and no participant sees a
picture — the same silent-success failure already documented for codec mismatch in
`publish_video_track`'s doc comment. This is worth naming because it is the failure that
looks most like success.
