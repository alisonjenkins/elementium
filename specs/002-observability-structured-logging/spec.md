# Feature Specification: Observability & Structured Logging

**Feature Branch**: `002-observability-structured-logging`

**Created**: 2026-08-05

**Status**: Draft

**Input**: User description: "Add observability to enable observability-driven and test-driven development. It will make it substantially easier to debug and fix bugs if we have this. The logging should use structured logging."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Diagnose a live bug from structured logs alone (Priority: P1)

As a maintainer investigating a user-reported bug (e.g. "camera doesn't start" or "call drops after 30s"), I need every subsystem the call path touches (screen/camera capture, codec, WebRTC engine, E2EE, keyring, Tauri command layer) to emit structured, correlated log events, so I can reconstruct what happened — in what order, with what data — without adding `println!`/ad-hoc logging and rebuilding.

**Why this priority**: This is the direct pain point named in the request ("substantially easier to debug and fix bugs"); every other story builds on this event stream existing.

**Independent Test**: Trigger a real failure path (e.g. start a camera pipeline with an invalid device ID) with logging enabled, and confirm the resulting log lines are structured (parseable as JSON or an equivalent key-value format, not free-form text), include a stable correlation identifier (e.g. session/call/track ID) present across every event from `enumerate_devices` through the camera thread's failure, and include enough fields (error type, device ID, requested format) to diagnose the failure without reading source code.

**Acceptance Scenarios**:

1. **Given** structured logging is enabled at `info` level, **When** a user starts a call (audio+video capture → encode → WebRTC send), **Then** every crate on that path emits at least one structured event carrying a shared correlation ID for that call/track.
2. **Given** a capture or encode operation fails, **When** the failure occurs, **Then** a structured error event is emitted with the error's type/variant and the specific input that caused it (not just "an error occurred").
3. **Given** a maintainer has a raw log capture from a bug report, **When** they filter it by the correlation ID for the affected call, **Then** they see a complete, ordered timeline of that call's lifecycle across all crates it touched.

---

### User Story 2 - Write a test that asserts on observed behavior, not just return values (Priority: P2)

As a developer fixing or preventing a bug, I need to write a test that asserts a specific structured event was (or wasn't) emitted with specific fields — e.g. "encrypting a frame with no key set logs a `frame_dropped` warning event with `reason=no_key`" — so I can pin down behavior that a plain return-value assertion can't capture (e.g. "this failed silently vs. failed loudly", "this took the fallback path vs. the primary path").

**Why this priority**: This is the "test-driven development" half of the request; it depends on Story 1's structured events existing but is a distinct capability (capturing and asserting on them in tests) with its own value once P1 lands.

**Independent Test**: Write one test per already-fixed regression this session (e.g. the fail-open E2EE encrypt bug, the zero-channels divide-by-zero) that captures emitted tracing events during the test and asserts the expected event/field was present; confirm each test fails if the underlying fix is reverted.

**Acceptance Scenarios**:

1. **Given** a test harness for capturing structured log events within a single test's scope, **When** a test invokes a function that emits a tracing event, **Then** the test can assert on the event's name, level, and field values without parsing raw stdout text.
2. **Given** the fail-open E2EE bug fixed in `elementium-webrtc` (silently sending plaintext when `encrypt_frame` returns `None`), **When** a regression test simulates that condition, **Then** the test asserts a structured "frame dropped, not sent" event was emitted (and fails today if that behavior regresses).
3. **Given** an existing `cargo test` run for a crate with new observability tests, **When** run inside `nix develop`, **Then** the new tests pass deterministically (no flakiness from timing/log ordering) and don't require external services.

---

### User Story 3 - Turn up log verbosity without redeploying, and correlate across process boundaries (Priority: P3)

As a maintainer supporting a released build, I need to change log verbosity for a specific subsystem at runtime (or via a config/env var without a rebuild), and have log events from the Rust backend, the WebRTC/LiveKit signaling layer, and (where applicable) the frontend correlate under the same session/call identifiers, so I can get deep diagnostics from a user's machine without shipping a debug build.

**Why this priority**: Valuable for supporting real users post-fix, but the app can already be debugged locally via Stories 1–2 without this; it's the "make it substantially easier" polish layer, not the core capability.

**Independent Test**: Set an env var (e.g. `RUST_LOG=elementium_webrtc=debug`) at launch and confirm only that crate's verbosity changes without a rebuild; confirm a log line from the Tauri command layer and a log line from `elementium-webrtc` for the same call share a correlation ID field with the same value.

**Acceptance Scenarios**:

1. **Given** the app is running, **When** launched with an env-var log filter targeting one crate/module at a `debug` level, **Then** only that crate's events increase in verbosity; other crates remain at their default level.
2. **Given** a call spans the Tauri command layer and `elementium-webrtc`, **When** logs from both are inspected, **Then** both carry the same correlation ID for that call.

### Edge Cases

- What happens when logging itself fails (e.g. disk full if writing to a file, or a subscriber panics)? → Logging failures MUST NOT crash or otherwise degrade the application; they are swallowed/reported through a secondary path (e.g. stderr) and never propagate as an application-level error.
- How does the system handle extremely high-frequency events (e.g. per-audio-frame or per-video-frame events at 30-48kHz-equivalent rates)? → Per-frame-level detail MUST NOT be logged at default verbosity (would flood output and hurt real-time performance); it's only available at an explicit elevated verbosity for the specific subsystem being debugged, and structured logging calls on the hot path MUST have negligible overhead when their level is disabled (i.e. use the logging library's lazy/level-gated evaluation, not always-computed strings).
- What happens to sensitive data (encryption keys, credentials, tokens) that a subsystem might otherwise be tempted to log for debugging? → MUST NEVER appear in any log event at any verbosity level, including debug/trace. Key material, passwords/secrets from the keyring, and raw session tokens are treated as never-loggable; logging code MUST reference them only by presence/absence or a non-reversible identifier (e.g. "key set: true", not the key itself).
- How does correlation work for operations that don't yet have a call/track/session (e.g. device enumeration before any call starts)? → Use a fallback scope of app-instance/session-startup id, so that even pre-call events remain grouped and searchable rather than orphaned.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every workspace crate that currently performs any logging (all 8: `elementium-codec`, `elementium-media`, `elementium-screen`, `elementium-webrtc`, `elementium-keyring`, `elementium-e2ee`, `elementium-types` if applicable, and `src-tauri`) MUST emit only structured log events (field-value pairs attached to a named event/span), not free-form interpolated strings, for any new or touched logging call site introduced by this feature. Existing free-form `tracing::info!("some {var}")`-style calls that already carry no meaningful fields beyond a message MAY be left as-is unless they sit on a path this feature specifically instruments (see Assumptions).
- **FR-002**: Every logged operation that is part of a user-initiated flow (starting/stopping a call, capturing a device, encoding a frame, connecting to LiveKit, encrypting/decrypting with E2EE) MUST carry a correlation identifier (call ID, track ID, or session ID as appropriate) attached to its structured event/span, consistent across every crate that flow touches.
- **FR-003**: Structured error events MUST include the error's concrete type/variant and the specific input or state that produced it, not just a generic message.
- **FR-004**: The system MUST provide a way to capture emitted structured log events within the scope of a single automated test (in-process, no external log aggregator required) and assert on event name/level/fields.
- **FR-005**: At least one regression test MUST exist per bug fixed via observability tooling going forward as a working example of Story 2's pattern; this feature itself MUST ship with regression tests for the two bugs fixed in the prior clippy-remediation work (E2EE fail-open, zero-channels divide) that assert on emitted structured events, not just return values.
- **FR-006**: Log verbosity MUST be controllable per-crate/per-module at process launch without recompiling (env var or config file), consistent with the existing `tracing-subscriber` + `EnvFilter` dependency already in the workspace.
- **FR-007**: The system MUST NEVER emit encryption keys, secret-store contents, or raw session/auth tokens in any log event at any verbosity level.
- **FR-008**: Structured logging calls on any per-frame/high-frequency hot path MUST be level-gated so that when the relevant verbosity is disabled, the cost of an event is negligible (no unconditional formatting/allocation).
- **FR-009**: A logging/tracing failure (e.g. subscriber panic, sink unavailable) MUST NOT crash the application or propagate as an application error.
- **FR-010**: Events emitted before any call/session exists (e.g. device enumeration, app startup) MUST still carry a fallback correlation identifier (e.g. an app-instance/startup ID) rather than being emitted uncorrelated.

### Key Entities

- **Structured log event**: A single emitted tracing event — name, level, timestamp, and a set of typed key-value fields (including the correlation ID field). The unit both human readers and automated tests operate on.
- **Correlation ID**: An identifier (call ID / track ID / session ID / fallback app-instance ID) attached to every event belonging to the same logical operation, used to reconstruct a timeline across crates and process boundaries.
- **Log capture (test fixture)**: An in-process mechanism a test uses to collect the structured events emitted during its own execution, for assertion purposes.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given only a structured log capture from a failed call (no source code access, no debugger), a maintainer unfamiliar with the specific crate involved can identify which subsystem and input caused the failure in under 5 minutes.
- **SC-002**: 100% of the correlation-ID-bearing events for a single call/track share the same identifier value across every crate that call/track touches, verified by an automated check.
- **SC-003**: At least 2 regression tests exist that assert on structured event output for previously-fixed real bugs (the E2EE fail-open and zero-channels cases), and both fail if the underlying fix is reverted.
- **SC-004**: Toggling log verbosity for one crate/module takes effect without a rebuild (env var/config change + relaunch only).
- **SC-005**: Zero instances of key material, secret-store contents, or raw tokens appear in log output across the full existing test suite and the new regression tests, verified by an automated scan of captured log output.

## Assumptions

- "Structured logging" means field-value pairs attached to named tracing events/spans (as already supported by the `tracing` crate already in the workspace), renderable as JSON via a `tracing-subscriber` JSON formatting layer — not a wholesale replacement of `tracing` with a different logging framework, per the user's explicit steer to build on what's there.
- This feature does not require standing up an external log aggregation/observability backend (e.g. OpenTelemetry collector, Loki, Datadog); "observability" here means structured, correlated, testable log output plus runtime-adjustable verbosity — export to an external system is out of scope for this feature and can be layered on later since `tracing`'s ecosystem supports it.
- Metrics (counters/histograms) and distributed tracing spans exported to a backend are out of scope; this feature is scoped to structured *logging* plus the spans needed for correlation within a single process/log stream, matching the user's explicit ask ("logging should use structured logging").
- "Frontend" observability (browser/Element Call iframe console logging) is out of scope beyond what already exists (the console-bridge IPC forwarding already in `src-tauri/src/main.rs`), unless a follow-up need is identified — this feature focuses on the Rust workspace where the user's crate list points.
- Not every existing `tracing::info!`/`tracing::warn!` call site in the codebase needs to be retrofitted; this feature instruments the user-initiated call/capture/encode/E2EE/keyring paths named in the user stories and establishes the pattern (test fixture, correlation ID convention, redaction rules) for future call sites to follow, rather than doing a full-codebase logging audit in one pass.
- Build environment remains `nix develop` per the existing project convention (clang/mold linker requirement unchanged by this feature).
