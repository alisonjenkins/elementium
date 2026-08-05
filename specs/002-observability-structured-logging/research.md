# Research: Observability & Structured Logging

## Decision: JSON output via `tracing-subscriber`'s built-in `.json()` formatter

**Decision**: Use `tracing_subscriber::fmt().json()` (feature already gated behind the `json`
Cargo feature of `tracing-subscriber`) as the production log layer in `src-tauri/src/main.rs`,
replacing the current plain `fmt()` call. Add `"json"` to the `tracing-subscriber` feature list in
the workspace `Cargo.toml` (currently `features = ["env-filter"]`).

**Rationale**: No new dependency — `tracing-subscriber` is already a workspace dep, and its `json`
feature is maintained in-tree by the `tokio-rs/tracing` project, so it stays version-compatible
automatically. Satisfies FR-001 (structured, machine-parseable output) directly. `EnvFilter`
(already used) composes with the JSON layer unchanged, satisfying FR-006 without new work.

**Alternatives considered**:
- `tracing-bunyan-formatter` — more opinionated bunyan-style JSON, adds a new dependency for
  marginal gain over the built-in formatter; rejected, no clear gap per spec Assumptions.
- Keep plain-text `fmt()` and post-process with a separate parser — rejected, defeats FR-001's
  "machine-parseable output" requirement at the source.
- Full OpenTelemetry export (`tracing-opentelemetry` + collector) — explicitly out of scope per
  spec Assumptions (no external aggregation backend required by this feature).

## Decision: Correlation ID via a `#[tracing::instrument]` span field, not a manually-threaded parameter

**Decision**: Introduce a `CorrelationId` newtype (String-backed, likely in `elementium-types`
since it's already the shared-types crate all others depend on) and attach it as a *span* field —
via `tracing::info_span!("call", correlation_id = %id)` entered at the point a call/track/session
starts (e.g. `get_user_media`, `livekit_connect`, `e2ee_init`) — rather than passing it as an
explicit function parameter through every call in the chain. Every `tracing::event!`/`info!`/
`warn!`/`error!` emitted while that span is entered automatically inherits the field (this is
`tracing`'s core span-inheritance mechanism, not custom plumbing).

**Rationale**: Matches spec FR-002 ("consistent across every crate that flow touches") without
invasive signature changes to already-lint-clean code from feature 001 — spans cross thread
boundaries via `tracing::Span::in_scope`/`Instrument::instrument` on spawned futures/threads,
which the codebase already uses `std::thread::spawn` for in several capture pipelines (verified in
`src-tauri/src/commands/media_devices.rs`, e.g. `audio_capture_loop`/`camera_pipeline_loop` run on
`std::thread::spawn`). Threading the span (via `.instrument()` for async, or capturing
`Span::current()` and re-entering with `.enter()` for the spawned thread closures) is a smaller,
more localized change than adding a `correlation_id: &str` parameter to every function on every
call path.

**Alternatives considered**: Explicit `correlation_id: CorrelationId` parameter threaded through
every function signature — rejected as unnecessarily invasive (touches far more call sites,
including ones with no other reason to change) and easy to silently forget to pass on new code
paths, whereas span inheritance is structural (a bug is "forgot to enter/instrument the span",
which is visible as a missing field rather than a wrong value).

## Decision: Fallback ID for pre-call events

**Decision**: Generate one process-lifetime "app instance ID" (a `CorrelationId`, e.g. a UUIDv4
generated once at `main()` startup) and enter it as a root span before any call/session exists.
Every event before a call-scoped span is entered still inherits this root span's field, satisfying
FR-010, and once a call-scoped span is entered, it can just add its own more specific
`correlation_id` field that shadows the root one for that scope (tracing span fields override by
scope nesting, not by explicit clearing).

**Rationale**: Simplest mechanism that satisfies "even pre-call events remain grouped and
searchable" (spec Edge Cases) with zero per-callsite decision-making — nothing has to check "do I
have a call ID yet?"; the root span is just always there.

**Alternatives considered**: `Option<CorrelationId>` at every call site, `None` before a call
starts — rejected, produces a mix of null/present values in the same JSON field across the log
stream, harder to query/filter (SC-001/SC-002 want uniform correlation), and pushes a null-check
onto every call site instead of the subscriber setup.

## Decision: Test-capture fixture via a custom `tracing_subscriber::Layer`, no new dependency

**Decision**: Implement a small `Vec<Mutex<...>>`-backed custom `Layer` (in a new
`elementium-observability` crate or as a `#[cfg(test)]`-only module shared via a `dev-dependencies`
path — decision on crate-vs-module left to the planning-to-tasks handoff since it affects the
`Project Structure` section, see data-model.md's `Log capture fixture` entity) that records each
`on_event` call's fields into an in-memory `Vec<CapturedEvent>` a test can inspect after running
the code under test with `tracing::subscriber::with_default(test_subscriber, || { ... })`.

**Rationale**: `tracing_subscriber::Layer` is already a workspace dependency's public trait — no
version-compatibility risk, no new supply-chain surface. `tracing::subscriber::with_default` is a
thread-local override scoped to the closure, so tests using it run safely in parallel (each test
gets its own subscriber, satisfying spec Story 2's "deterministic, no flakiness" acceptance
criterion) without needing `#[serial]`-style test-ordering crates.

**Alternatives considered**: `tracing-test` crate (dev-dependency, wraps similar functionality
with a `#[traced_test]` attribute macro) — viable and slightly more ergonomic, but its default
capture is text-based (asserts on formatted log lines via string matching) rather than structured
field assertions, which is a weaker fit for FR-004 ("assert on event name/level/fields", not
string-match a formatted line); `tracing-mock` — more powerful expectation-DSL but heavier API
surface than this feature's 2 initial regression tests justify. Both remain viable future upgrades
if the pattern needs more than field-value assertions later; noted as a non-blocking alternative,
not adopted now to minimize new dependencies per the spec's "build on what's there" steer.

## Decision: Regression test targets (satisfying FR-005/SC-003)

**Decision**: Two regression tests, matching the two real bugs fixed during feature 001's clippy
remediation:
1. `elementium-webrtc`: simulate `encrypt_frame` returning `None` (no key set) in the outbound
   frame-write path (`livekit/transport.rs`'s `pc_io_loop`, `engine.rs`'s `io_loop`/
   `encrypt_or_drop`) and assert a structured "frame dropped" warning event is emitted with a
   `reason` field (e.g. `reason="no_key"` or similar) — proving the fail-open bug (silently
   sending plaintext) cannot regress silently; a plain return-value test can't distinguish
   "dropped safely" from "sent as plaintext" without this event.
2. `src-tauri` (or wherever `resample_44100_to_48000` ends up — currently
   `src-tauri/src/commands/media_devices.rs`): call it with `channels = 0` and assert either (a)
   no panic occurs (the existing `channels.max(1)` guard) AND (b) a structured event fires noting
   the anomalous zero-channels input was clamped, so a maintainer sees *that this happened* in
   production logs rather than the guard silently masking a real upstream device-enumeration bug
   forever.

**Rationale**: Directly satisfies FR-005's explicit requirement to cover these two bugs, and both
demonstrate the Story 2 pattern (assert on emitted event, not just the return value/absence of
panic) concretely enough to serve as the template for future regression tests per spec Assumptions
("establishes the pattern... for future call sites to follow").

## Decision: Secret redaction — convention + grep-based scan, not a type-system guarantee

**Decision**: (a) Audit `elementium-keyring`/`elementium-e2ee`/`src-tauri/src/commands/secrets.rs`
+ `e2ee.rs` call sites that currently might construct debug/log output including secret values,
converting them to log presence/absence or a redacted marker only (e.g. `has_key: bool` instead of
the key bytes); (b) add a `cargo test`-driven scan step (grep-equivalent, run against captured
log-fixture output in the new regression tests plus a dedicated "no secrets in structured event
fields" smoke test using the same test-capture fixture) that fails if a known-sensitive field name
pattern appears with non-redacted content, satisfying SC-005's "verified by an automated scan".

**Rationale**: Rust has no compile-time taint tracking, so a "MUST NEVER log secrets" requirement
can only be enforced by code review discipline plus a runtime/test-time check — the test-capture
fixture built for Story 2 is directly reusable for this, no extra machinery needed.

**Alternatives considered**: A custom `Sensitive<T>` wrapper type that panics/redacts on
`Debug`/`Display` — more robust long-term but a larger surface change (touches every type that
might hold a secret) than this feature's scope; noted as a possible follow-up, not required to
satisfy FR-007/SC-005 as written.
