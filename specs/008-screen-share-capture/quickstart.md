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

---

## Level 3 — Teardown leaves nothing behind

**Proves**: SC-006. Cheap to run, and the current code fails it — `get_display_media`
drops its capturer handle with the capture still running.

```sh
# Start and stop a share ten times, then check what survived
pw-dump | grep -c '"node.name"'      # before
# ... ten start/stop cycles ...
pw-dump | grep -c 'node.name'        # after: must match
```

**Pass**: node count returns to baseline, no `wayland-screencast` threads remain, no
portal session is left open.

**Why by hand and by count**: a leak of one is invisible; a leak of ten is obvious. This
is why the criterion is ten cycles rather than one.

---

## Level 4 — A remote participant sees the screen (the actual feature)

**Proves**: US1, and it is the only thing that does.

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

1. Share a single window.
2. Change content in a **different** window — play a video, resize something.
3. Confirm the receiver sees no change.

**Pass**: nothing from outside the chosen window reaches the receiver.

**Why the negative test is the test**: confirming the chosen window appears proves only
that capture works. Confirming that the *unchosen* window does not appear is the whole
claim. A backend that silently falls back to full-monitor capture passes the positive
test and fails the user.

---

## Level 6 — Share audio (US3)

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
