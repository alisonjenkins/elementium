# Quickstart: Validating Observability & Structured Logging

## Prerequisites

- `nix develop` shell (unchanged project convention)
- Feature 001 (clippy deny-level lints) already merged — this feature's new code must also pass
  `cargo clippy --workspace --all-targets` clean

## Story 1: Structured, correlated logs from a real failure

```bash
nix develop --command bash -c 'RUST_LOG=info cargo run -p elementium 2>&1 | tee /tmp/elementium.log'
# trigger a camera-start with an invalid/nonexistent device from the UI, then Ctrl-C
jq -r 'select(.fields.correlation_id != null) | .fields.correlation_id' /tmp/elementium.log | sort -u
# pick one correlation_id, filter the full timeline for it:
jq -c --arg cid "<paste-a-correlation_id>" 'select(.fields.correlation_id == $cid)' /tmp/elementium.log
```

Expected: every line is valid JSON (`jq` doesn't error); the filtered timeline shows the call's
lifecycle in order across whichever crates it touched (SC-001, SC-002).

## Story 2: Test-driven assertions on emitted events

```bash
nix develop --command bash -c 'cargo test -p elementium-webrtc encrypt_frame_none_drops_not_leaks'
nix develop --command bash -c 'cargo test -p elementium resample_zero_channels_logs_anomaly'
```

Expected: both pass. To confirm they're real regression guards (not just passing trivially),
temporarily revert the relevant fix (e.g. change `encrypt_or_drop` back to `unwrap_or(data)`) and
re-run — the test must fail.

## Story 3: Runtime verbosity control + cross-layer correlation

```bash
nix develop --command bash -c 'RUST_LOG=elementium_webrtc=debug,info cargo run -p elementium 2>&1 | tee /tmp/elementium-debug.log'
# confirm only elementium_webrtc target lines show DEBUG level:
jq -r 'select(.level == "DEBUG") | .target' /tmp/elementium-debug.log | sort -u
```

Expected: only `elementium_webrtc::*` targets appear at DEBUG; other crates stay at their default
(INFO or above).

## Secret-redaction check (SC-005)

```bash
nix develop --command bash -c 'cargo test --workspace 2>&1 | tee /tmp/full-test-run.log'
nix develop --command bash -c 'cargo test -p elementium-e2ee -p elementium-keyring -- --nocapture 2>&1' \
  | grep -iE 'key[_-]?material|secret[_-]?value|token[_-]?raw' \
  && echo "FAIL: possible secret leaked in log output" || echo "OK: no obvious secret leakage"
```

Expected: `OK`. This is a coarse smoke check (grep-based) — the actual enforcement mechanism is
the dedicated redaction test using the log-capture fixture (see research.md's Secret redaction
decision), which asserts structurally rather than by string-matching output.
