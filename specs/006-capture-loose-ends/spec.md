# Feature Specification: Capture loose ends

**Created**: 2026-08-07
**Status**: Backlog

## Why this exists

Findings from debugging the capture path that are real, small, and would otherwise
be lost. None is urgent; each was observed rather than imagined, and each would cost
someone an hour to rediscover.

They are grouped because they share a property: every one is a case where the
software works and the *evidence about it* is wrong or missing, which is the kind of
defect that makes the next fault take twice as long to find.

## User Scenarios

### US1 (P2) — A log line says what actually happened

Diagnostic messages describe the condition they were emitted for.

**Independent test**: read the capture warnings emitted during an MJPEG session and
check each against what the code did.

### US2 (P2) — The frame rate shortfall has a cause

Capture runs at the requested rate, or the reason it does not is named.

**Independent test**: run a camera for 12 seconds at 30fps and read the counters.

### US3 (P3) — A camera that vanishes is handled tidily

Unplugging a camera mid-call degrades predictably.

## Success Criteria

- **SC1**: No capture warning describes a condition other than the one that occurred.
- **SC2**: Captured frame rate is within 5% of the requested rate with a camera that
  can sustain it, or the counters say which stage dropped the difference.

## Known items

**A misleading message.** MJPEG frames that fail to decode are reported as
`PipeWire buffer too small for the negotiated geometry`, which describes the raw
path and not this one. Observed twice per camera start. The buffer is fine; the
decode failed.

**The frame-rate shortfall is unexplained.** Measured 29.3fps against a requested
30, with a median inter-frame gap of exactly 33.4ms — so the pacing is right and a
handful of frames go missing. The counters to attribute them (`offered`,
`rate_limited`, `queue_full`, `unusable`) are in place but have not been read with a
camera attached, because the camera was disconnected mid-investigation.

**PipeWire buffer-type negotiation is now explicit** and fixed a total failure, but
only `MemPtr` and `MemFd` are declared. A source that can offer only DMA-BUF will
now fail to negotiate rather than failing later — better, but still a failure. It
becomes a non-issue if DMA-BUF import lands (feature 005, T010).

**The camera's node id changes** when it is re-enumerated, and a stale id sends
capture to a node that no longer exists. Handled by falling through, but the failure
is noisier than it needs to be.
