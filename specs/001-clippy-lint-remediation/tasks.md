# Tasks: Clippy Lint Remediation

**Input**: Design documents from `/specs/001-clippy-lint-remediation/`
**Prerequisites**: plan.md, research.md, data-model.md, quickstart.md

**Tests**: Not requested as a separate TDD phase — existing `cargo test` suites are the
regression check (FR-004), run per crate and again at the end (quickstart.md).

**Organization**: Tasks grouped by user story from spec.md (P1 codec, P2 the other four flagged
crates, P3 full-workspace verification), matching plan.md's crate-by-crate structure.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Different crates/files, no dependency — can run in parallel
- All work happens inside `nix develop --command bash -c '...'` (plan.md Target Platform)

---

## Phase 1: Setup

- [ ] T001 Confirm baseline: run `nix develop --command bash -c 'cargo clippy --workspace --all-targets --keep-going'` and save output to `/tmp/clippy-baseline.txt` for reference — line numbers in research.md may have drifted since 2026-08-05

---

## Phase 2: Foundational

*None — `[workspace.lints.clippy]` and per-crate `[lints]\nworkspace = true` are an already-completed prerequisite (spec Assumptions), not part of this feature's work.*

---

## Phase 3: User Story 1 - Codec crate is clean (Priority: P1) 🎯 MVP

**Goal**: `cargo clippy -p elementium-codec --all-targets` exits 0; `cargo test -p elementium-codec` passes.

**Independent Test**: `nix develop --command bash -c 'cargo clippy -p elementium-codec --all-targets'`

- [X] T002 [US1] Re-run `cargo clippy -p elementium-codec --all-targets` inside `nix develop` to get current file:line list for `crates/elementium-codec/src/`
- [X] T003 [US1] Fix all `as_conversions` violations in `crates/elementium-codec/src/` per research.md fix pattern (`f32::from`/`try_from`, or propagate as `Result`)
- [X] T004 [US1] Fix all `arithmetic_side_effects` violations in `crates/elementium-codec/src/` (`checked_sub`/`checked_add` etc., propagated)
- [X] T005 [US1] Fix all `indexing_slicing` violations in `crates/elementium-codec/src/` (`.get()`/`.get_mut()` propagated as `Option`)
- [X] T006 [US1] Apply `mul_add` rewrites for `suboptimal_flops` hits in `crates/elementium-codec/src/`
- [X] T007 [US1] Fix `unwrap_used`/`expect_used` violations in `crates/elementium-codec/src/` (propagate via `?`, or scoped justified `#[allow]` only where structurally infallible per research.md)
- [X] T008 [US1] Add missing `# Errors`/`# Panics` doc sections and `#[must_use]` attributes flagged in `crates/elementium-codec/src/`
- [X] T009 [US1] Fix remaining one-off lints (float/int casts, doc backticks, `could_be_const_fn`, redundant clones, wildcard imports, etc.) in `crates/elementium-codec/src/`
- [X] T010 [US1] Re-run `cargo clippy -p elementium-codec --all-targets` inside `nix develop`; repeat T003-T009 for any remaining errors (including test-target-only errors surfaced now)
- [X] T011 [US1] Run `cargo test -p elementium-codec` inside `nix develop`; fix any regressions introduced by T003-T009

**Checkpoint**: `elementium-codec` is independently clippy-clean and test-green (SC-004).

---

## Phase 4: User Story 2 - Media, E2EE, keyring, screen crates are clean (Priority: P2)

**Goal**: `elementium-media`, `elementium-e2ee`, `elementium-keyring`, `elementium-screen` each
pass `cargo clippy -p <crate> --all-targets` and `cargo test -p <crate>`.

**Independent Test**: `nix develop --command bash -c 'cargo clippy -p elementium-media -p elementium-e2ee -p elementium-keyring -p elementium-screen --all-targets'`

### elementium-media

- [ ] T012 [P] [US2] Re-run `cargo clippy -p elementium-media --all-targets` inside `nix develop` for current file:line list in `crates/elementium-media/src/`
- [ ] T013 [US2] Fix all flagged violations in `crates/elementium-media/src/` per research.md fix patterns (depends on T012)
- [ ] T014 [US2] Re-run clippy for `elementium-media`; repeat T013 until clean (depends on T013)
- [ ] T015 [US2] Run `cargo test -p elementium-media` inside `nix develop`; fix any regressions (depends on T014)

### elementium-e2ee

- [ ] T016 [P] [US2] Re-run `cargo clippy -p elementium-e2ee --all-targets` inside `nix develop` for current file:line list in `crates/elementium-e2ee/src/lib.rs`
- [ ] T017 [US2] Apply the frame decrypt/encrypt fixes already triaged in research.md (`Ok(None) | Err(_) => continue`, `let...else`, `.last().copied()`, `RwLock` poison → propagated error, justified `#[allow(clippy::expect_used)]` on the two HKDF `.expect()` calls) in `crates/elementium-e2ee/src/lib.rs` (depends on T016)
- [ ] T018 [US2] Fix remaining flagged violations elsewhere in `crates/elementium-e2ee/src/` (depends on T016)
- [ ] T019 [US2] Re-run clippy for `elementium-e2ee`; repeat T017-T018 until clean (depends on T017, T018)
- [ ] T020 [US2] Run `cargo test -p elementium-e2ee` inside `nix develop`; fix any regressions (depends on T019)

### elementium-keyring

- [ ] T021 [P] [US2] Re-run `cargo clippy -p elementium-keyring --all-targets` inside `nix develop` for current file:line list in `crates/elementium-keyring/src/`
- [ ] T022 [US2] Fix all flagged violations in `crates/elementium-keyring/src/` per research.md fix patterns (depends on T021)
- [ ] T023 [US2] Re-run clippy for `elementium-keyring`; repeat T022 until clean (depends on T022)
- [ ] T024 [US2] Run `cargo test -p elementium-keyring` inside `nix develop`; fix any regressions (depends on T023)

### elementium-screen

- [ ] T025 [P] [US2] Re-run `cargo clippy -p elementium-screen --all-targets` inside `nix develop` for current file:line list in `crates/elementium-screen/src/`
- [ ] T026 [US2] Fix all flagged violations in `crates/elementium-screen/src/` per research.md fix patterns (depends on T025)
- [ ] T027 [US2] Re-run clippy for `elementium-screen`; repeat T026 until clean (depends on T026)
- [ ] T028 [US2] Run `cargo test -p elementium-screen` inside `nix develop`; fix any regressions (depends on T027)

**Checkpoint**: All 4 crates independently clippy-clean and test-green (SC-004). The four
sub-tracks (media / e2ee / keyring / screen) are mutually parallel — no shared files.

### elementium-webrtc (discovered during Phase 5 verification — not in original baseline)

- [ ] T028b [US2] Re-run `cargo clippy -p elementium-webrtc --all-targets` inside `nix develop` for current file:line list in `crates/elementium-webrtc/src/` (252 lib + 255 lib-test errors found 2026-08-05 during T029, missed by the original `--keep-going` baseline capture)
- [ ] T028c [US2] Fix all flagged violations in `crates/elementium-webrtc/src/` per research.md fix patterns (depends on T028b)
- [ ] T028d [US2] Re-run clippy for `elementium-webrtc`; repeat T028c until clean (depends on T028c)
- [ ] T028e [US2] Run `cargo test -p elementium-webrtc` inside `nix develop`; fix any regressions (depends on T028d)

---

### src-tauri (discovered during Phase 5 verification — blocked by missing frontendDist until now)

- [X] T028f [US2] Fix two mechanical pedantic violations in `src-tauri/src/main.rs` and `src-tauri/src/commands/media_devices.rs` (similar_names, needless_raw_string_hashes)
- [X] T028g [US2] Create a local (gitignored) `element-web-dist/` placeholder so `tauri::generate_context!()` stops proc-macro-panicking and the rest of `src-tauri` can be clippy-checked (2026-08-05: 117 more errors surfaced once unblocked — `unreachable!` (22), significant-drop-tempories (12), pass-by-value (10), `as_conversions` (9), doc backticks (9), `arithmetic_side_effects` (9), `unwrap_used` (7), and ~1-4 each of a long tail — full breakdown in the T029 verification run)
- [X] T028h [US2] Re-run `cargo clippy -p elementium --all-targets` inside `nix develop` for current file:line list in `src-tauri/src/` (depends on T028g)
- [X] T028i [US2] Fix all flagged violations in `src-tauri/src/` per research.md fix patterns; for the 2 lints that fire inside `tauri::generate_context!()` itself (`large_stack_frames`, `exit`) in `main.rs`, a scoped `#[allow]` directly on the `.run(tauri::generate_context!())` call is correct since the macro expansion isn't our code (depends on T028h)
- [X] T028j [US2] Re-run clippy for `src-tauri`; repeat T028i until clean (depends on T028i)
- [X] T028k [US2] Run `cargo test -p elementium` inside `nix develop`; fix any regressions (depends on T028j)

## Phase 5: User Story 3 - Full workspace passes clean, config holds long-term (Priority: P3)

**Goal**: `cargo clippy --workspace --all-targets` and `cargo test --workspace` both pass clean.

**Independent Test**: `nix develop --command bash -c 'cargo clippy --workspace --all-targets'`

- [X] T029 [US3] Run `cargo clippy --workspace --all-targets` inside `nix develop`; confirm zero errors, including `src-tauri`, `elementium-types`, `elementium-webrtc` still at 0 (depends on Phase 3, Phase 4)
- [X] T030 [US3] Run `cargo test --workspace` inside `nix develop`; confirm 100% pass (depends on T029)
- [X] T031 [US3] Run `git diff main... -- '*.rs' | grep -n '#\[allow(clippy::'` per quickstart.md; confirm every hit is scoped (not module/crate-level) and has a one-line justification comment (SC-003) (depends on T030)

**Checkpoint**: SC-001 through SC-004 all satisfied.

---

## Dependencies & Execution Order

- **Setup (T001)** → informs Phase 3/4 but doesn't block them starting
- **Phase 3 (US1, T002-T011)**: sequential within itself (same crate, same files) — no [P]
- **Phase 4 (US2, T012-T028)**: the 4 crate sub-tracks are parallel to each other (`[P]` on each
  track's first task); within a sub-track, sequential
- **Phase 5 (US3, T029-T031)**: strictly after Phase 3 AND Phase 4 complete; sequential within itself
- Phase 3 and Phase 4 have no file overlap (different crates) and MAY run in parallel with each
  other if capacity allows, though spec priority (P1 before P2) suggests finishing US1 first

## Parallel Example: Phase 4

```text
Track A: T012 → T013 → T014 → T015   (elementium-media)
Track B: T016 → T017 → T018 → T019 → T020   (elementium-e2ee)
Track C: T021 → T022 → T023 → T024   (elementium-keyring)
Track D: T025 → T026 → T027 → T028   (elementium-screen)
```
All four tracks can be dispatched concurrently (e.g. 4 parallel agents/sessions) — no shared files.

## Implementation Strategy

**MVP = Phase 3 (US1) only**: `elementium-codec` clean is independently valuable and verifiable
(largest error count, highest real-world panic risk) and can ship/review as its own commit before
touching the other crates.

**Incremental delivery**: Phase 3 → Phase 4 (4 crates, parallelizable) → Phase 5 (verification).
Each crate change lands as its own atomic commit per FR-007 / this session's git-strategy mandate
(one crate = one logical change = one commit, reverting any single crate's fixes leaves the
workspace compiling).
