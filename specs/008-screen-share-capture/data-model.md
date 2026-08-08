# Phase 1 Data Model: Screen and application sharing, with audio

**Created**: 2026-08-08

The feature is about media flowing, not records persisting — nothing here is stored
across runs. What follows is the in-memory state the feature introduces or reshapes, and
the invariants that make two simultaneous video tracks possible.

---

## MediaTrackKey

The identity that `IoCommand` currently lacks, and the change everything else depends on.

| Field | Type | Notes |
|---|---|---|
| kind | `audio` \| `video` | Which m-line family |
| source | `microphone` \| `camera` \| `screen_share` \| `screen_share_audio` | Matches LiveKit's `TrackSource` names exactly |

**Why these two fields and not a generated id**: `publish_track_inner` already derives
its `cid` as `{local_sid}-{kind}-{source}`, and the SFU pairs a published track to an
m-line by matching that `cid` against the offer's msid. Deriving the routing key from the
same two values means the key the I/O loop routes on and the key the SFU pairs on cannot
drift apart. A fresh UUID would need mapping to the cid anyway, and that mapping is
exactly where the drift would live.

**Invariant**: at most one active pipeline per `MediaTrackKey`. This is what makes
"camera and screen share at once" expressible (different keys) while "two cameras" stays
impossible (same key) — which matches both the hardware and the spec's one-share-at-a-time
assumption.

---

## PipelineHandle *(reshaped from `CameraPipelineHandle` / `AudioCaptureHandle`)*

| Field | Type | Notes |
|---|---|---|
| key | `MediaTrackKey` | Replaces "the camera slot" as identity |
| track_id | `String` | The `video-…` / `audio-…` id the frontend holds |
| stop_tx | `Sender<()>` | Unchanged |
| encode_tx | `Arc<Mutex<Option<Sender<IoCommand>>>>` | Unchanged; late-bound by adoption |
| keyframe_requested | `Arc<AtomicBool>` | Video only |
| active_codec | `Arc<ActiveCodec>` | Video only |

**Relationship change**: `MediaState.camera: Mutex<Option<…>>` and
`MediaState.audio_capture: Mutex<Option<…>>` become a single
`Mutex<HashMap<MediaTrackKey, PipelineHandle>>`.

**Why this is not gratuitous**: the two existing singletons already share a trait
(`CapturePipeline`, with `stop_sender` and `connection`) precisely because the code needed
to treat them uniformly. The map is that trait's natural container. The alternative —
adding `screen: Mutex<Option<…>>` and `screen_audio: Mutex<Option<…>>` — gives four
near-identical slots and four copies of every lifecycle operation.

**State transitions**:

```text
absent ──start──▶ running (encode_tx = None, "idle")
running(idle) ──adopt_idle_pipelines──▶ running (encode_tx = Some, "attached")
running ──stop──▶ absent
running ──restart──▶ running (connection inherited, see note)
```

The *restart* edge already exists and matters: `stop_pipeline_inheriting_connection`
carries the peer connection across a mid-call source change so the far end does not
freeze. Keyed pipelines must preserve this per key, not globally.

---

## ShareSession

New. Owns one user's act of sharing, and is what makes teardown complete rather than
approximate.

| Field | Type | Notes |
|---|---|---|
| backend | `X11` \| `WaylandPortal` | Chosen once; fixes R6's enumerate/start disagreement |
| source_kind | `Monitor` \| `Window` | What the picker returned, for diagnostics and audio scoping |
| video_node | PipeWire node id | From the portal, or the X11 equivalent |
| audio | `Option<ShareAudio>` | `None` when the user did not opt in |
| video_key | `MediaTrackKey` | Always `{video, screen_share}` |

**Lifecycle invariant** (this is SC-006): dropping a `ShareSession` stops the video
pipeline, stops the audio pipeline if present, and closes the portal session. Nothing
outlives it. The current code leaks in exactly this way — `get_display_media` constructs
a capturer, starts it, and drops the handle at end of scope with the capture still
running.

**Cardinality**: zero or one per application. The spec assumes one share at a time;
enforcing it here rather than by convention means a second start is a defined outcome
(refuse or replace) rather than two capturers fighting over a portal session.

---

## ShareAudio

| Field | Type | Notes |
|---|---|---|
| node_id | PipeWire node id | The sink monitor, or the application's own output stream |
| scope | `DesktopMix` \| `Application` | What was actually captured |
| announced_scope_mismatch | `bool` | True when the user asked for application audio and got the desktop mix |

**Why `announced_scope_mismatch` is data and not just a log line**: the spec requires
that falling back to the desktop mix is disclosed to the user, because sending more audio
than the user expected is a privacy failure. A flag the UI can read is the difference
between a disclosure and a log entry nobody sees. It exists because the fallback is
expected to be reachable — see the open question in research R4.

---

## CaptureSource *(existing, unchanged shape)*

`{ id, name, kind: monitor | window }`. Already defined in `elementium-types` and already
crossing to the frontend. On Wayland this list is deliberately empty — the portal is the
picker — and the frontend must treat empty as "ask the portal", not as "no sources".
That distinction is currently made by accident rather than explicitly.

---

## What is deliberately not modelled

- **Per-participant share state.** Receiving someone else's share is just an inbound
  track; the existing remote-track path handles it. Nothing here is about the receiving
  side.
- **Restore tokens.** The portal can persist a user's choice across sessions
  (`PersistMode`). The current code passes `DoNot` and this feature does not change it —
  re-prompting is the safe default and persisting a screen-capture grant deserves its own
  decision.
- **Simulcast layers for screen content.** Screen video has different rate/quality
  tradeoffs from camera video, but the encode path publishes a single layer today for
  both, and changing that is not in scope.
