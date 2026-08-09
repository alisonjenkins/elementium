# The robotic audio, 2026-08-09

The longest-running complaint in this project: the far end hears the speaker as "very
breaky uppy and robotty". Intermittent, not constant, and it has outlived several fixes —
the mono fold, the −6 dB level, automatic gain, the startup hold, the publish path.

This is the first time it has been measured rather than reasoned about.

## What the numbers say

From one call, 116,500 audio packets, using instrumentation that already existed and that
nobody had read. Stamped immediately after `send_to` returns, so it is the last point the
application controls, not a proxy for it.

| | |
|---|---|
| mean gap between packets | **20 ms** — the average rate is exactly right |
| gaps over 30 ms (`late`) | 31–50 per 250-packet window, worst 71 — **12 to 28 per cent** |
| gaps under 5 ms (`clumped`) | 4–17 per window, worst 36, `min_gap_ms` 0 |
| worst gap | 43 ms at the socket, 73 ms upstream |

A fifth of frames arrive a whole period late, and others arrive back-to-back. That is the
jitter-buffer-underrun signature, and underruns are concealed by packet loss concealment,
which is what "robotic" sounds like.

**Where it comes from is narrower than it first looked.** Over the same call the capture
side reported a worst gap of 66 ms but `burst_frames` never above 2 — so capture delivers
smoothly and occasionally stalls, while the socket clumps heavily. The bunching is
introduced *between* the capture loop and the socket, not by capture. That rules out the
first proposed cause and points at one place: `drain_io_commands` pulls everything queued
each iteration, `poll_once` hands it all to str0m, and str0m runs with a null pacer, so a
brief stall anywhere becomes a burst.

## Open

- [ ] **A1. Audio is sent in bursts after any stall.** A pacer at the engine's write
  boundary, deliberately minimal: it acts only when there is a backlog, never delays a frame
  that is already late — re-timing a late frame onto the grid only makes it later — and
  gives up and flushes if the backlog grows, because holding one is worse than a burst.
  `ELEMENTIUM_AUDIO_PACING=0` disables it, because this cannot be validated offline and the
  next call should be able to compare.

- [x] **A2. FIXED. A device that does not open at 44.1 or 48 kHz is handed to Opus unresampled.**
  The rate is mapped to the nearest Opus-supported value, but the resample that would make
  that true fires only for the exact pair 44100 → 48000. A device at 96000, 32000 or 22050
  falls through and its samples reach a 48 kHz encoder unchanged — wrong speed, wrong pitch,
  indistinguishable from the complaint. Not what happened on the observed call (that device
  opened at 48000, and capture and Opus agreed), so this is a real fault that is not the
  fault we were chasing.

## Considered, and not the cause

- **The gain stage.** A sudden transient can step the gain by up to 30 per cent between one
  20 ms buffer and the next, with no intra-buffer ramp, and the clipping ceiling is a hard
  per-buffer clamp. Audible in principle on plosives; it does not produce 30 ms gaps.
- **Mono folding.** The divisor changes discretely when a channel's decayed peak crosses the
  liveness threshold, so a level step is possible. Same objection: it cannot make a packet
  late.
- **The bounded send channel.** 256 frames is about five seconds of headroom; it would need
  the sink to stall outright, and nothing observed does.
- **The 5 ms idle sleep in the capture loop.** Real but bounded: it can add a few
  milliseconds of variance, not thirty.
