# Data Model: Observability & Structured Logging

## Structured Log Event

The unit both human readers (via JSON log lines) and automated tests (via the capture fixture)
operate on. Not a Rust struct that gets instantiated directly — it's the shape `tracing`'s macros
produce, described here for schema/contract purposes (this is this feature's "contract" in lieu
of an external API).

| Field | Type | Notes |
|---|---|---|
| `timestamp` | RFC3339 string | Added automatically by the JSON formatter layer |
| `level` | enum | `TRACE`/`DEBUG`/`INFO`/`WARN`/`ERROR`, standard `tracing::Level` |
| `target` | string | Module path, automatic |
| `message` | string, optional | Present for `event!`-style calls with a `"..."` message; some events may be field-only |
| `correlation_id` | string (UUID or similar) | See `CorrelationId` below — present on every event via span inheritance (FR-002, FR-010) |
| *(event-specific fields)* | varies | e.g. `error_type`, `device_id`, `reason`, `has_key: bool` — never raw secret values (FR-007) |

## CorrelationId

New newtype, likely added to `elementium-types` (already the shared cross-crate types crate).

| Field | Type | Notes |
|---|---|---|
| value | String (UUIDv4-formatted) | Generated once per logical scope: once at process startup (the fallback/root ID) and once per call/track/session start |
| scope | enum (conceptual, not necessarily a literal field) | `app_instance` (root, spec Edge Cases fallback) \| `call` \| `track` \| `session` — determines when a new one is minted vs. reused |

Lifecycle: minted at the start of the relevant scope (`main()` for `app_instance`; `get_user_media`/
`livekit_connect`/etc. for `call`/`session`) → entered as a `tracing` span field → inherited by
every event emitted while that span (or a child span) is active → the span exits when the scope
ends (call/session teardown), after which new events fall back to whatever span is active (a
still-running parent call, or the root `app_instance` span if none).

## Log Capture Fixture (test-only)

| Field | Type | Notes |
|---|---|---|
| `events: Vec<CapturedEvent>` | in-memory | Populated by the custom `Layer`'s `on_event` hook during the scope of `tracing::subscriber::with_default` |
| `CapturedEvent.name`/`level`/`fields: HashMap<String, FieldValue>` | struct | What a test asserts against (FR-004) |

Lifecycle: constructed fresh per test (no shared/global state → parallel-test-safe per research.md)
→ installed via `with_default` for the duration of the code under test → inspected via assertion
helpers (e.g. `capture.find_event("frame_dropped")`, `.assert_field("reason", "no_key")`) → dropped
at test end, no cleanup required.

No `contracts/` directory — the schema above (event field names, `CorrelationId` semantics) *is*
this feature's contract, consumed by (a) whatever downstream log-viewing/filtering a maintainer
uses (spec Story 1/3, out of this feature's build scope — just needs valid JSON) and (b) the test
fixture (Story 2, built by this feature).
