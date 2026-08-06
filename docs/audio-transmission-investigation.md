# Outbound audio investigation

Status as of 2026-08-07. Written down because the useful output of this work is
mostly *negative* results, and they are expensive to re-derive.

## The symptom

Remote peers hear the local user as "robotic", "breaking up", and latterly
"barely any of my audio gets through — I heard a tiny bit of me speaking".
Confirmed by the user joining the same call from a second device and hearing
themselves, so the fault is in what Elementium transmits or in how the far end
reconstructs it, not in one peer's playback.

## What is measured clean

All of these were measured, not argued:

| Stage | Evidence |
|---|---|
| Microphone | raw capture dumped to disk, clean in an editor |
| Resample/reframe | encoder input identical in shape to raw capture |
| Opus encode | loopback correlates r=0.953 at exactly the algorithmic delay |
| Opus config | 48kHz mono, FEC on, bitrate pinned; bitstream asserted in tests |
| E2EE framing | byte-identical round trip; interop tests build frames livekit's way |
| Key rotation | every index 0..63 decrypts, including past the ring size |
| RTP timing | 50.0 packets/sec at the socket, no clock drift |
| RTCP loss | 0% measured from peers' receiver reports |
| Rust → SFU → Rust | delivery ratio 1.000 (`audio_layer_bisection.rs`) |
| Rust → SFU → Chromium | 500/500 packets, 0 concealed, 0 stretched, when it connects |

## Hypotheses tested and rejected

Each of these was believed, tested, and disproved. They are listed so they are
not re-tried.

- **Send-side jitter / io_loop clumping.** Real (~10% of packets clumped, ±20ms)
  and worth fixing, but ordinary network-grade jitter that NetEq absorbs. Fixing
  the capture buffer removed it with no audible change.
- **RTCP Sender Report wallclock jitter.** Real defect, fixed (wallclock now
  derived from the media timeline). No audible change.
- **Stereo at 48kbps.** Real waste, fixed (mono). No audible change.
- **RED payload confusion.** str0m has no RED support at all, so it is never
  negotiated. Dead.
- **Opus DTX.** Never enabled; libopus defaults it off. Dead.
- **Late key delivery.** Element Call already delays a new key by `useKeyDelay`
  (5s) so peers receive it first, and the shim's `Worker.prototype.postMessage`
  hook preserves that timing. Reproduced in the browser harness: recovers
  cleanly. Dead.
- **livekit's key invalidation** (`hasInvalidKeyAtIndex`, tolerance 10). Reads as
  though it latches, but measured: recovers. Dead.
- **Advertising more ICE candidates.** Tried; measurably *worse* (failures went
  from ~half to ~three quarters of runs), because each candidate is another pair
  to check and the SFU removes participants that have not connected in time.

A false alarm worth remembering: an early browser measurement showed 89% of audio
concealed and was reported as reproducing the bug. It did not. Two measurement
faults caused it — stats read once, possibly while the track was still starting,
and only the delta recorded, so a window with no audio was indistinguishable from
a window of destroyed audio. Both are fixed in the harness.

## Bugs found and fixed

1. **ICE `Disconnected` treated as fatal.** str0m documents the state as
   recoverable and has no terminal state at all. We set `alive = false` and the
   I/O loop exited, so one transient blip ended the session permanently. Now a
   30s grace period.
2. **`AddTrackRequest.cid` did not match the offer's msid track id.** The SFU
   pairs a published track with an m-line by matching those two; livekit-client
   gets it right for free because its cid *is* the `MediaStreamTrack.id`. We
   invented both independently, so association fell through to the server's
   match-by-kind fallback — which works until ordering or track counts change.
3. **Re-offering appended duplicate m-lines** for a kind that already had one.

## The open lead

`Rust publisher → SFU → Chromium` fails roughly half the time, while
`browser → SFU → browser` through the same SFU passes 4/4 — including with the
publisher's join delayed to match ours. So the fault is in what we publish.

Two distinct failure modes are visible in the SFU's own logs:

- `rtc/room.go:1449 removing participant without connection` for
  `rust-publisher`. Neither of our transports reaches `connected` from the SFU's
  side; its connection timeout fires and removes the participant, taking the
  published track with it. Our own logs say `Connected` because that is str0m's
  view — we believe we have a valid pair while the SFU never saw us.
- Subscription starts (`subscribing to new track` present) but the browser still
  reports no `inbound-rtp`.

**This is the most promising explanation for the field symptom.** A publisher
that is silently removed mid-call, with something above rejoining, produces
exactly "parts of what I say get through" while every sender-side metric stays
clean — we continue encoding, encrypting and writing to the socket throughout.

### Where to look next

- Why the SFU never observes our ICE as connected while str0m does. Compare the
  candidate pair we nominate against the one the SFU expects; the asymmetry
  suggests we consider a pair valid on our outbound checks alone.
- `set_local_description` (`src-tauri/src/commands/webrtc.rs`) is a no-op, so
  livekit-client's munged SDP never reaches str0m and the SFU's model of the
  session can diverge from ours with nothing to reconcile it.
- The declared client protocol version is 9; livekit-client negotiates 17. The
  SFU gates behaviour on this.

## Running the harness

Rust layers (1–3 hermetic, 4–5 need the SFU):

```bash
docker run -d --name elementium-test-livekit --network host \
    livekit/livekit-server --dev --bind 0.0.0.0
cargo test -p elementium-webrtc --test audio_layer_bisection -- --ignored --test-threads=1
```

Browser receive path (needs the same SFU):

```bash
cd frontend && pnpm exec playwright test
PUBLISHER_LOG=info pnpm exec playwright test -g "plain Opus"   # with publisher logs
```

The SFU's own logs are the third source of truth and were decisive several times:

```bash
docker logs elementium-test-livekit 2>&1 | grep "<room-name>"
```
