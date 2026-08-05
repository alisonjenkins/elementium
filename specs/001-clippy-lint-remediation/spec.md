# Feature Specification: Clippy Lint Remediation

**Feature Branch**: `001-clippy-lint-remediation`

**Created**: 2026-08-05

**Status**: Draft

**Input**: User description: "Fix all clippy pedantic/nursery/deny lint violations across the elementium workspace so `cargo clippy --workspace --all-targets` passes clean under the newly configured deny rules (pedantic, nursery, unwrap_used, expect_used, indexing_slicing, arithmetic_side_effects, unreachable, unimplemented, unchecked_time_subtraction, todo, string_slice, panic_in_result_fn, panic, exit, as_conversions)."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Codec crate is clean (Priority: P1)

As a maintainer, I need `elementium-codec` — the largest offender (228 lib + 267 test errors) — to build clean under `cargo clippy --workspace --all-targets` so the crate carrying the most panic-prone indexing/casting code (audio/video encode paths) is verifiably safe.

**Why this priority**: Largest error count, and encode/decode paths run on untrusted/variable media data — panics here are the most likely real-world crash source.

**Independent Test**: Run `cargo clippy -p elementium-codec --all-targets` inside `nix develop`; zero errors.

**Acceptance Scenarios**:

1. **Given** the workspace lint config from `Cargo.toml`, **When** `cargo clippy -p elementium-codec --all-targets` runs, **Then** it exits 0 with no errors.
2. **Given** the remediated crate, **When** its existing test suite runs (`cargo test -p elementium-codec`), **Then** all tests still pass (no behavior regressions introduced by lint fixes).

---

### User Story 2 - Media, E2EE, keyring, screen crates are clean (Priority: P2)

As a maintainer, I need the remaining four flagged crates (`elementium-media`, `elementium-e2ee`, `elementium-keyring`, `elementium-screen`) to build clean under the same deny rules, since E2EE and keyring code handle cryptographic material where silent truncation/panics are a security concern.

**Why this priority**: Smaller error counts than codec, but `elementium-e2ee` and `elementium-keyring` touch cryptographic key material — correctness there is high-stakes even though volume is lower.

**Independent Test**: Run `cargo clippy -p elementium-media -p elementium-e2ee -p elementium-keyring -p elementium-screen --all-targets` inside `nix develop`; zero errors.

**Acceptance Scenarios**:

1. **Given** the workspace lint config, **When** clippy runs against each of these four crates with `--all-targets`, **Then** each exits 0 with no errors.
2. **Given** each remediated crate, **When** its existing test suite runs, **Then** all tests still pass.

---

### User Story 3 - Full workspace passes clean, config holds long-term (Priority: P3)

As a maintainer, I need `cargo clippy --workspace --all-targets` to pass clean across every crate (including `src-tauri`, `elementium-types`, `elementium-webrtc`, which had zero flagged errors in the baseline run but must not regress), so the deny-level config is fully enforced and CI can rely on it without carve-outs.

**Why this priority**: Confirms no crate was missed and the config is self-sustaining — lowest priority only because it's a verification/integration step that depends on P1 and P2 being done first.

**Independent Test**: Run `cargo clippy --workspace --all-targets` inside `nix develop` from a clean checkout; zero errors, zero `#[allow(...)]` blanket suppressions added to silence rather than fix.

**Acceptance Scenarios**:

1. **Given** all crates remediated, **When** `cargo clippy --workspace --all-targets` runs, **Then** it exits 0 with no errors and no warnings escalate to errors unexpectedly.
2. **Given** the full workspace test suite, **When** `cargo test --workspace` runs, **Then** all tests pass with no regressions attributable to lint fixes.

---

### Edge Cases

- What happens when a genuine `as` cast, index, or arithmetic op is provably safe (e.g., cast preceded by an explicit bounds check)? → Prefer a safe rewrite (`try_from`, `.get()`, checked arithmetic) over `#[allow]`; use a scoped `#[allow(clippy::x)]` with a one-line justification comment only when a safe rewrite is not practical (e.g., hot-path SIMD-adjacent code where the safe form regresses performance measurably).
- How does the fix handle `unwrap()`/`expect()` calls on values that are structurally guaranteed to be `Some`/`Ok` (e.g., HKDF expand into a fixed-size buffer that cannot fail for that key size)? → Convert to a returned `Result`/`Option` propagated to the caller where the function can fail, or a scoped `#[allow(clippy::expect_used)]` with justification where the call sits in a `const`/init path with no caller to propagate to.
- What happens when missing `# Errors` / `# Panics` doc sections apply to `pub` functions that are effectively internal (`pub(crate)` would be more correct)? → Prefer narrowing visibility to `pub(crate)`/private where the function is not part of the crate's public API; only add doc sections where the function is genuinely public.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `cargo clippy -p elementium-codec --all-targets` MUST exit 0 with zero errors under the workspace's deny-level lint config.
- **FR-002**: `cargo clippy -p elementium-media --all-targets`, `-p elementium-e2ee`, `-p elementium-keyring`, and `-p elementium-screen` MUST each exit 0 with zero errors.
- **FR-003**: `cargo clippy --workspace --all-targets` MUST exit 0 with zero errors across all eight workspace members (`src-tauri`, `elementium-types`, `elementium-codec`, `elementium-media`, `elementium-screen`, `elementium-webrtc`, `elementium-keyring`, `elementium-e2ee`).
- **FR-004**: `cargo test --workspace` MUST pass with no test regressions introduced by lint remediation (behavior-preserving fixes only, unless a fix is itself a genuine bug fix — in which case it MUST be called out explicitly, not silently folded in).
- **FR-005**: Fixes MUST prefer safe rewrites (checked arithmetic, `.get()`/`.get_mut()`, `try_from`/`From`, `let...else`, `#[must_use]`, doc sections) over `#[allow(...)]` suppressions. Any `#[allow(...)]` added MUST be scoped to the smallest unit (function/expression, not module/crate) and carry a one-line comment explaining why a safe rewrite wasn't used.
- **FR-006**: No lint category from the configured deny list (pedantic, nursery, `unwrap_used`, `expect_used`, `indexing_slicing`, `arithmetic_side_effects`, `unreachable`, `unimplemented`, `unchecked_time_subtraction`, `todo`, `string_slice`, `panic_in_result_fn`, `panic`, `exit`, `as_conversions`) MUST be downgraded or disabled workspace-wide to make the count go down artificially.
- **FR-007**: Remediation MUST proceed crate-by-crate in priority order (codec → media/e2ee/keyring/screen → full-workspace verification), each crate landing as its own reviewable unit of work.

### Key Entities

- **Lint violation**: A single clippy error at a specific file/line, tagged with a rule name (e.g., `clippy::as_conversions`) and crate. Tracked to completion per the counts captured in the 2026-08-05 baseline run (see Assumptions).
- **Workspace crate**: One of the 8 members of the Cargo workspace; the unit of remediation and independent verification.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `cargo clippy --workspace --all-targets` inside `nix develop` completes with exit code 0 and zero reported errors (down from the 2026-08-05 baseline of ~740 errors across 5 flagged crates).
- **SC-002**: `cargo test --workspace` passes at 100% after remediation, matching or exceeding the pre-remediation pass rate.
- **SC-003**: Zero blanket crate- or module-level `#[allow(...)]` attributes are added to silence any of the 15 configured deny lint groups; any function/expression-level `#[allow]` that remains is individually justified.
- **SC-004**: Each of the 5 flagged crates (`elementium-codec`, `elementium-media`, `elementium-e2ee`, `elementium-keyring`, `elementium-screen`) lands as an independently reviewable, independently clippy-clean unit of work.

## Assumptions

- Baseline violation counts (2026-08-05, `cargo clippy --workspace --all-targets --keep-going` inside `nix develop`): `elementium-codec` 228 lib + 267 test errors; `elementium-media` 104 lib+test errors; `elementium-e2ee` 62 lib + 85 test errors; `elementium-keyring` 13 lib + 23 test errors; `elementium-screen` 12 lib+test errors. `src-tauri`, `elementium-types`, `elementium-webrtc` had zero flagged errors in this run and are in scope only for the final zero-regression check (FR-003).
- The `[workspace.lints.clippy]` config in root `Cargo.toml` and the `[lints]\nworkspace = true` opt-in already added to all 8 member crates (this session, 2026-08-05) are correct and final — this feature is about fixing violations, not further changing which lints are enabled.
- `nix develop` is the canonical build environment (provides `clang`/`mold` required by `.cargo/config.toml`); remediation and verification both run inside it.
- No functional/behavioral changes are in scope beyond what's needed to satisfy the lints (e.g., replacing a panicking index with a `Result`-returning `.get()` may require threading an error return through callers — that's in scope; adding new features is not).
- This is an internal code-quality task with no end-user-facing UI; "user" in the stories above means the crate maintainer running clippy/CI, not an app end-user.
