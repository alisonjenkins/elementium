# Data Model: Clippy Lint Remediation

No runtime data model — this feature edits code, not data. The two "entities" from the spec are
tracking constructs for the work itself:

## Lint Violation

Represents one clippy error at a specific location, tracked to zero per crate.

| Field | Type | Notes |
|---|---|---|
| crate | enum | One of the 8 workspace members |
| file | path | e.g. `crates/elementium-e2ee/src/lib.rs` |
| line | int | May drift as earlier fixes in the same file shift line numbers — re-run clippy per crate before fixing, don't rely on the 2026-08-05 baseline line numbers verbatim |
| rule | string | e.g. `clippy::as_conversions` |
| target | enum | `lib` or `test` (from `--all-targets`) |
| resolution | enum | `rewritten` (safe fix applied) \| `allowed` (scoped `#[allow]` + justification comment) — see FR-005 |

Lifecycle: `open` → `rewritten` or `allowed` → verified absent from a re-run of
`cargo clippy -p <crate> --all-targets`.

## Workspace Crate

The unit of remediation and independent verification (spec Key Entities, FR-007).

| Field | Type | Notes |
|---|---|---|
| name | string | `elementium-codec`, `elementium-media`, `elementium-e2ee`, `elementium-keyring`, `elementium-screen`, plus `src-tauri`/`elementium-types`/`elementium-webrtc` (verify-only, 0 baseline errors) |
| baseline_lib_errors | int | From 2026-08-05 `--keep-going` run |
| baseline_test_errors | int | From 2026-08-05 `--keep-going` run |
| priority | enum | P1 (`elementium-codec`) \| P2 (other 4 flagged crates) \| P3 (verify-only, full workspace) |
| status | enum | `not_started` → `in_progress` → `clean` (own `cargo clippy -p <name> --all-targets` exits 0 and `cargo test -p <name>` passes) |

No contracts/ artifacts — no external API, wire format, or CLI schema is introduced or changed by
this feature.
