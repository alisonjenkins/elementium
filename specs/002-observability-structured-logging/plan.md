# Implementation Plan: Observability & Structured Logging

**Branch**: `002-observability-structured-logging` | **Date**: 2026-08-05 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/002-observability-structured-logging/spec.md`

## Summary

The workspace already depends on `tracing` 0.1 + `tracing-subscriber` 0.3 (`env-filter` feature),
and `src-tauri/src/main.rs` already installs a plain-text `fmt()` subscriber gated by
`EnvFilter`. This feature: (1) switches the subscriber to a structured (JSON) formatting layer so
output is machine-parseable by default, (2) establishes a correlation-ID convention (call/track/
session ID, with an app-instance-ID fallback) threaded through spans across the call/capture/
encode/E2EE/keyring code paths named in the spec, (3) adds an in-process log-capture test fixture
(via `tracing-subscriber`'s `Layer` + a `Vec`-backed sink, or the `tracing-test`/`tracing-mock`
crate) so tests can assert on emitted events, (4) writes 2+ regression tests against real bugs
already fixed in this session (E2EE fail-open, zero-channels divide) using that fixture, and (5)
audits the touched paths for lint-level enforcement that secrets never appear in log fields.

## Technical Context

**Language/Version**: Rust 1.93.1 (via `rust-overlay`, pinned in `flake.nix`), edition 2024

**Primary Dependencies**: `tracing` 0.1 (already present, workspace dep) — add a JSON formatting
layer via `tracing-subscriber`'s built-in `.json()` formatter (already available since
`tracing-subscriber` is already a workspace dep, no new external crate strictly required); for the
test-capture fixture, either `tracing-subscriber`'s own `Layer`/`Registry` composition (no new
dependency) or the lightweight `tracing-test` / `tracing-mock` dev-dependency crate — decision
captured in research.md

**Storage**: N/A (log output to stdout/stderr by default, consistent with current behavior; no
new persistent storage introduced)

**Testing**: `cargo test --workspace` (existing suites, extended with new regression tests using
the log-capture fixture); `cargo clippy --workspace --all-targets` must stay clean per the
workspace's existing deny-level lint config (already enforced by feature 001, unrelated feature,
must not regress)

**Target Platform**: Linux desktop (Tauri app); build requires `nix develop` per existing project
convention (unchanged by this feature)

**Project Type**: Desktop app (Tauri) + supporting library crates — single Cargo workspace

**Performance Goals**: Per FR-008/spec Edge Cases — level-gated event emission must add
negligible overhead on hot paths (per-frame audio/video) when the relevant level is disabled;
`tracing`'s macros already short-circuit on level checks before evaluating field expressions, so
this is a call-site discipline requirement (don't format strings before the macro call), not a
new mechanism to build

**Constraints**: No key material/secrets/tokens in any log field at any level (FR-007, spec Edge
Cases) — enforced by convention + the regression/scan approach in SC-005, not a compiler-level
guarantee (Rust has no taint tracking); logging failures must not crash the app (FR-009) —
`tracing-subscriber`'s layers already degrade gracefully (a broken writer doesn't panic the
caller) but this must be verified for whatever JSON layer is chosen; existing `cargo clippy`
deny-level config (from feature 001) must remain clean throughout

**Scale/Scope**: Touches `src-tauri/src/main.rs` (subscriber setup) plus the call/capture/encode/
E2EE/keyring code paths across `elementium-media` (capture), `elementium-codec` (encode),
`elementium-webrtc` (engine/transport), `elementium-e2ee` (encrypt/decrypt), `elementium-keyring`
(secret access — presence/absence only per FR-007), and `src-tauri/src/commands/*` (the Tauri
command layer entry points where call/track/session IDs originate or arrive from the frontend).
Does not require a full-codebase logging audit (spec Assumptions) — establishes the pattern on
these paths, not every existing log call site.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

`.specify/memory/constitution.md` is still the unfilled template (no ratified principles) — no
project-specific gates apply, same as feature 001. No violations to justify.

## Project Structure

### Documentation (this feature)

```text
specs/002-observability-structured-logging/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md        # Phase 1 output
└── tasks.md             # Phase 2 output (/speckit-tasks — not created by this command)
```

No `contracts/` directory — the "contract" this feature establishes is the structured-event field
schema (correlation ID field name/type, standard field names for error type/input) captured in
`data-model.md`, not a network/API contract.

### Source Code (repository root)

```text
Cargo.toml                          # workspace root — tracing/tracing-subscriber deps already present;
                                     # may add a JSON formatter feature flag or a small test-capture dev-dependency
src-tauri/
├── src/main.rs                     # subscriber setup -> switch fmt() to structured/JSON layer
└── src/commands/
    ├── media_devices.rs            # call/track ID origination -> attach correlation ID to spans
    ├── webrtc.rs                   # peer connection lifecycle -> correlation ID
    ├── livekit.rs                  # session lifecycle -> correlation ID
    ├── e2ee.rs                     # key set/clear events -> presence-only logging (no key material)
    └── secrets.rs                  # keyring access -> presence-only logging (no secret values)
crates/
├── elementium-media/src/           # capture start/stop/failure events, correlation ID propagated in
├── elementium-codec/src/           # encode/decode error events (already touched by feature 001's lint fixes)
├── elementium-webrtc/src/          # engine/transport lifecycle events, correlation ID origin point for calls
├── elementium-e2ee/src/            # encrypt/decrypt frame-drop and lock-poison events (already has the
│                                    # fail-open bug fix from feature 001 to regression-test here)
├── elementium-keyring/src/         # secret access presence/absence events only
└── elementium-types/src/           # possibly: shared CorrelationId/SessionId newtype if one doesn't exist
```

**Structure Decision**: Existing workspace layout unchanged. Work lands crate-by-crate similar to
feature 001, but each crate's change here is additive (new spans/fields on existing call paths)
rather than a lint-fix sweep, so commits are organized by capability (subscriber/JSON layer →
correlation ID plumbing → test-capture fixture → regression tests → redaction audit) rather than
strictly by crate, since correlation ID plumbing inherently crosses crate boundaries in one
logical change per flow (e.g. "camera capture correlation ID" touches both `elementium-media` and
`src-tauri/src/commands/media_devices.rs` together).

## Complexity Tracking

*No constitution violations — section not applicable.*
