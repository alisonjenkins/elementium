# Quickstart: Validating screen and application sharing

**Created**: 2026-08-08

How to prove the feature works, in the order the evidence becomes available. Each section
states what it proves and what a pass looks like — deliberately, because the failure this
feature exists to fix (a black rectangle) is invisible to any test that only checks a
track exists.

---

## Prerequisites

A Wayland session with a working ScreenCast portal. Verify before blaming the code:

```sh
# Session and compositor
echo "$XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP"          # expect: wayland niri

# Which backend serves ScreenCast
busctl --user list | grep -E 'Mutter.ScreenCast|portal.desktop'

# PipeWire is up and shows audio nodes
pw-dump | grep -c media.class
```

If `org.gnome.Mutter.ScreenCast` is absent, window selection (US2) is unavailable on this
machine and a US2 failure is environmental, not a defect. Record which backend was present
when reporting any result — see research R7.

All Rust commands run inside the dev shell:

```sh
nix develop -c cargo test --workspace
nix develop -c cargo clippy --workspace -- -D warnings
```

---

## Level 1 — Routing carries track identity (no camera, no portal, no call)

**Proves**: R2's blocker is removed. Everything else is blocked on this, so it is worth
having a test that fails loudly before any UI exists.

```sh
nix develop -c cargo test -p elementium-webrtc
```

**Pass**: a test publishes two video tracks with different `MediaTrackKey`s and asserts
that frames written for one appear on that track's mid and not the other's.

**Why this level exists separately**: routing to the wrong mid produces advancing sender
counters and no picture anywhere. If that is only discovered in a call, it will be
mistaken for an encode or E2EE fault, which is a day lost. Catch it where the assertion
is cheap.

---

## Level 2 — Screen frames reach the encoder (portal, no call)

**Proves**: the capture path works end to end up to the encoder, without needing two
endpoints.

```sh
nix develop -c cargo run -p elementium-media --example capture_attribution -- --screen
```

Requires a human to accept the portal picker. Unbounded by design — a person is reading
a dialog.

**Pass**: frame counters advance, geometry matches the chosen monitor, and the negotiated
encode target is logged. `unusable=0`.

**Watch for**: a monitor is not a 1280x720 webcam. If the negotiated target or the encoder
rejects the geometry, that surfaces here rather than as a black remote view.

**Also look at the picture**, which counters cannot do for you:

```sh
CAPTURE_DUMP=/tmp/share.pgm nix develop -c cargo run -p elementium-media \
  --example capture_attribution -- --screen
```

A DMA-BUF read with the wrong stride, or a tiled buffer read as if it were linear,
delivers frames at the right rate and the right size, full of noise. Every counter above
passes in that state.

**Recorded 2026-08-08** (niri, `xdg-desktop-portal-gnome`, one shared window):

| | |
|---|---|
| negotiated | 1880x1446 `Raw(Bgrx)`, DMA-BUF |
| frames received | 56–64 over ~15s |
| delivered rate | 3.8–4.3fps against 30 requested |
| `unusable` | 0 |
| dumped frame | the shared window, sharp, correctly strided |

The delivered rate is *not* a fault and this is the entry worth reading twice. A
compositor emits on damage, not on a clock — it advertises `framerate=0/1` for exactly
that reason — so a mostly-static window produces a few frames a second and a scrolling one
produces thirty. Judging this path by fps against the requested rate, the way the camera
path is judged, would report a healthy share as broken.

### Full-monitor geometry (T044), measured 2026-08-08

Run the Level 2 command and choose the **whole screen** at the picker, not a window. On a
5120x1440 ultrawide:

| | |
|---|---|
| negotiated | 5120x1440 `Raw(Bgrx)`, DMA-BUF |
| frames received | 23 over 14.0s |
| delivered rate | 1.6fps against 30 requested |
| dumped frame | the full desktop, sharp, correctly strided |

Nothing about the larger geometry needed handling: the same DMA-BUF path that reads a
1880x1446 window reads a 7.4-megapixel monitor, because the extent is derived from
`stride x height` rather than assumed.

Ignore the example's "process CPU per frame" at this rate — with 23 frames in 14 seconds it
is process startup divided by a handful of frames, not the cost of a frame.

The encoder's half is asserted separately, without needing a person at a picker:

```sh
nix develop -c cargo test -p elementium-codec --test full_monitor_geometry
```

VP8 initialises at 5120x1440 and produces a keyframe from the first frame, and a frame of
different geometry is refused rather than misread. That is the failure worth ruling out: an
encoder that accepted a resized frame against its old configuration would read the planes
at the wrong stride and emit a sheared picture at a perfectly healthy frame rate.

**One finding to be aware of rather than to fix now.** `bitrate_for` in
`src-tauri/src/commands/media_devices.rs` targets ~0.1 bits per pixel per frame at 30fps
and clamps to 4000kbps. A 5120x1440 monitor asks for ~22Mbps by that rule, so **the clamp
binds and a full ultrawide share gets 4Mbps**. For a desktop that is usually fine — screen
content is highly compressible and a damage-driven screencast rarely reaches 30fps, so the
real bits-per-frame is several times the nominal figure. It would *not* be fine for a share
of full-motion video, which is the case to measure before raising it.

---

## Level 3 — Teardown leaves nothing behind

**Proves**: SC-006. Passed 2026-08-08 — see the measured table below. (Written when the
code still failed it: `get_display_media` used to drop its capturer handle with the capture
still running.)

```sh
nix develop -c cargo run -p elementium-media --example capture_attribution -- --screen --cycles 10
```

One portal grant, ten open/close cycles, then the process **holds for 20s** so the graph
can be inspected while it is still alive — process exit would clean up a leak rather than
reveal it. During the hold:

```sh
pw-dump | grep -c elementium-capture                     # PipeWire objects we still own
ps -o nlwp= -p "$(pgrep -f examples/capture_attribution)" # threads, in the *binary*
```

**Measured 2026-08-08**: 0 objects and 1 thread after ten cycles.

**Both metrics were validated before being trusted**, because a counter that reads zero in
every state proves nothing:

| | idle | while capturing | after 10 cycles |
|---|---|---|---|
| `elementium-capture` objects | 0 | 1 | 0 |
| threads in the binary | 1 | 2 | 1 |

Note the `pgrep`: an earlier reading of "1 thread while capturing" was the `timeout`/`cargo`
wrapper, not the example. Measuring the wrong process is the easiest way to get a clean
result here, and it looks identical to a real one.

**Scope, stated plainly**: this measures *our* capture teardown — stream, thread, node —
across ten cycles of one portal grant. It does not measure ten portal sessions; that
teardown is `ShareSession::close` and its `Drop` backstop, which logs
`screencast portal session closed` on each share.

**Why by hand and by count**: a leak of one is invisible; a leak of ten is obvious. This
is why the criterion is ten cycles rather than one.

### A shared window disappearing (T028), measured 2026-08-08

```sh
# one throwaway window, shared and then killed mid-capture
setsid foot --title=SHARE-TEST-WINDOW sh -c 'while true; do date; sleep 1; done' &
echo $!                     # kill this pid a few seconds into streaming
nix develop -c cargo run -p elementium-media --example capture_attribution -- --screen
```

**Pass**: the run logs `the captured PipeWire node was removed` and prints
`source failed during the run: true`. A control run with the window left open prints
`false`.

**What this rules out, and why the obvious fix is wrong**: closing the window does not
error the stream. The observed transition is Streaming -> Paused -> Streaming, after which
no frame ever arrives — and *that is also what a healthy share of a static window looks
like*, since a compositor emits on damage. A frame-stall timeout would therefore end
legitimate shares of a still document. The node's removal from the registry is the only
unambiguous signal, and the id is recycled quickly (seen reused for an unrelated Link),
so it must be watched rather than polled for.

### Capture latency (SC-002), measured 2026-08-08

```sh
# a window whose contents are driven through a fifo, so the change time is known exactly
mkfifo $F/pipe
setsid foot --title=LATENCY-WINDOW sh -c "...; while true; do sh -c 'cat < $F/pipe'; done" &

CAPTURE_LATENCY=1 CAPTURE_SECONDS=30 \
  nix develop -c cargo run -p elementium-media --example capture_attribution -- --screen
# then write a full screen of '#' and a full screen of ' ' alternately, recording each
# write time, and compare against the run's PICTURE_CHANGED_AT lines
```

Ten alternating changes, milliseconds from writing to the window to the change appearing in
a captured frame:

| min | median | mean | max |
|---|---|---|---|
| 309 | 333 | 384 | 761 |

**Scope, stated plainly.** This is the *capture* half: terminal render, compositor damage,
`PipeWire` delivery, and our decode and queueing. It does not include encode, the SFU, the
network, or the receiver's jitter buffer and decoder, so it is a lower bound on SC-002's
end-to-end second rather than a measurement of it. The margin is wide enough — a third of a
second against a budget of one — that the remaining path would have to be slower than the
capture to breach it, but that has not been measured and is not claimed.

The single 761ms outlier is worth keeping in view rather than averaging away: at 30fps the
capture is not the limit, but a damage-driven source can coalesce updates, and the tail is
what a user notices.

---

## Level 4 — A remote participant sees the screen (the actual feature)

**Proves**: US1, and it is the only thing that does.

**Passed 2026-08-08**, attended, on niri / `xdg-desktop-portal-gnome`, E2EE on:

```sh
ATTENDED=1 pnpm exec playwright test -g "sustained, increasing decoded frame count"
```

`framesDecoded` at the browser, sampled every 5s across 30s:

```text
[6, 14, 22, 30, 38, 46, 54]
```

Attended by design — the portal picker needs a person, and the test skips rather than fails
without `ATTENDED=1`, because a red test nobody can make green in CI gets ignored and then
protects nothing.

**The failure this found, worth knowing before the next one:** the first run had the
publisher sending 900 packets with 0% loss while the receiver decoded nothing, because the
SFU was sending PLIs (27 of them) and never got a keyframe back. Every counter on the
sending side looked healthy. The app handles PLI properly — the event reaches
`src-tauri/src/commands/webrtc.rs`, which sets the pipeline's keyframe flag — but a caller
driving `LiveKitRoom` directly does not get that event, so the publisher example asks for a
keyframe on a timer instead.

```sh
just test-browser
```

**Pass**: the receiving endpoint's `framesDecoded` **increases over at least 30 seconds**.

**Not a pass**: a track exists; a track is `live`; one frame arrived. The bug being fixed
produces a perfectly valid track carrying nothing, so track existence proves nothing. The
assertion must be on a counter advancing over time.

Manual confirmation, once automated confirmation passes: share a monitor, move a window
on it, and watch the change appear at the other endpoint.

---

## Level 5 — Window scoping is real (US2)

**Proves**: FR-005 and SC-003, which are privacy properties rather than features.

**Measured 2026-08-08, and it passes.** Run it with two scripted windows and compare the
captured pixels, rather than by eye:

```sh
# two windows that each sit still until their flag file appears, then fill themselves
setsid foot --title=SHARE-A-1 sh -c '...tick until $F/goa, then fill with #...' &
setsid foot --title=SHARE-B-1 sh -c '...tick until $F/gob, then fill with @...' &

CAPTURE_DUMP_SERIES=/tmp/sc3_ CAPTURE_SECONDS=40 \
  nix develop -c cargo run -p elementium-media --example capture_attribution -- --screen
# pick SHARE-A-1, then: touch $F/gob at t+8s, touch $F/goa at t+24s
```

Mean absolute pixel difference of each dumped frame against the first:

| ~t | event | mean abs diff | pixels differing |
|---|---|---|---|
| 0–20s | the *other* window fills itself at t+8s | **0.00** | **0.0%** |
| 25s | the *shared* window fills itself at t+24s | 0.07 | 0.0% |
| 30s | " | 4.56 | 2.9% |
| 35s | " | 4.81 | 3.1% |

**The second half is not decoration, it is what makes the first half mean anything.** A
comparison that cannot detect change would report 0.00 against a frozen capture, a black
frame, or a stream that died twenty seconds ago. The control shows the same measurement
moving when the *shared* window changes, so the zero above is a real negative.

**Two traps, both hit while doing this:**

- **A completely static window produces no frames at all.** A compositor emits on damage, so
  a window that never changes never gets captured, and the receiver's video element sits at
  `readyState=0` while the share is working perfectly. The scripted windows tick a
  two-character clock for exactly this reason — far below the comparison's resolution, but
  enough to keep frames flowing.
- **Give the windows unique titles per run.** A stale window from an earlier attempt is
  indistinguishable in the picker, and picking one yields a capture that ticks forever and
  never responds to the flag files — which reads exactly like "the change did not leak".

**Not automatable in the browser harness**, and this was measured rather than assumed:
Playwright's Chromium takes the workspace on a tiling compositor, the scripted windows stop
being composited, and capture starves — 7 packets sent against 44 keyframe requests. Hence
the check lives at the capture side, where nothing competes for the screen.

**Why the negative test is the test**: confirming the chosen window appears proves only
that capture works. Confirming that the *unchosen* window does not appear is the whole
claim. A backend that silently falls back to full-monitor capture passes the positive
test and fails the user.

---

## Level 6 — Share audio (US3)

**Passed 2026-08-08**, attended, with a 440Hz tone playing through the desktop sink:

```sh
ATTENDED=1 pnpm exec playwright test -g "share audio is received"
```

At the browser, over a settled 10s window on the `screen_share_audio` track:

| packetsReceived | packetsLost | concealedSamples | totalSamplesReceived |
|---|---|---|---|
| 500 | 0 | **0** | 480000 |

Zero concealment is the number that matters: a decrypt failure, a jitter-buffer stall or a
malformed payload all show up as concealment while `packetsLost` stays at zero, which is
how a stream can look healthy on the wire and sound like noise.

**Covered: the delivery half.** SC-004 also asks that the microphone stay independently
audible and mutable, which needs a concurrent microphone publisher this harness does not
have. That half is untested and the test says so rather than implying otherwise.

**Two harness faults this run cost, both worth recognising again:**

- The publisher prints its `AUDIO_SENT` total when it *exits*, and the test kills it after
  measuring — so asserting on that total failed a run whose audio was flowing perfectly
  (501 packets, zero concealed). It now asserts on an `AUDIO_FLOWING` marker printed on the
  first packet. A harness fault that reads as a product fault is expensive.
- Leftover publishers from earlier failed runs hold their SFU connections; kill them before
  blaming a `CONNECTION_TIMEOUT`.


**Proves**: FR-006 through FR-008, SC-004 and SC-005.

Play a known tone from the shared application, with the microphone also live.

**Pass**:

- Receiver's audio contains both the tone and speech.
- Muting the microphone silences speech only; the tone continues.
- Stopping the share stops the tone; the microphone is unaffected.

**The opt-out check (SC-005), which must be done from outside the process**:

```sh
pw-dump | python3 -c "import json,sys; [print(o['id'], (o.get('info') or {}).get('props',{}).get('node.name')) for o in json.load(sys.stdin) if 'Stream/Input/Audio' in str(o)]"
```

Start a share **without** requesting audio and confirm no new input stream belonging to
this application appears. Reading the code and concluding no stream was opened is not
sufficient evidence for a privacy claim — the audio graph is the authority.

**Measured 2026-08-08, video-only capture**: twelve audio streams present in the graph
before, the same twelve — by id, not merely by count — during a running screen capture. No
stream was created.

Stated precisely, because this is a privacy claim and the difference matters: this
exercised the *capture path* via the Level 2 example, which never requests audio at all. It
demonstrates that capturing a screen does not itself open an audio stream. It does **not**
yet exercise the `audio: false` branch of `get_display_media`, which is the guard a user's
opt-out actually flows through. That check still needs the running app.

**Scope disclosure**: when application audio was requested and the desktop mix was
captured instead, confirm the response carries `audioScopeFallback: true` and that this
is surfaced to the user. Silent over-capture is the failure mode this guards.

---

## Level 7 — X11 parity (US4)

Run the share flow under an X11 session.

**Pass**: either a working share, or a specific and accurate failure naming what is
missing. A silent black rectangle is a fail — that is the bug this feature exists to
remove, and reintroducing it on a less-used path still reintroduces it.

---

## Regression gates before commit

```sh
nix develop -c cargo clippy --workspace -- -D warnings   # must be 0
nix develop -c cargo test --workspace
just test-frontend
```

The workspace denies `unwrap_used`, `expect_used`, `indexing_slicing`,
`arithmetic_side_effects`, `as_conversions`, `panic` and caps functions at 100 lines.
`camera_pipeline_loop` is already near that cap, so generalising it will need extraction
rather than addition — plan for that rather than discovering it at commit time.
