# Tasks: Observability & Structured Logging

**Input**: Design documents from `/specs/002-observability-structured-logging/`
**Prerequisites**: plan.md, research.md, data-model.md, quickstart.md

**Tests**: Requested explicitly — this feature's whole point (Story 2, FR-004/FR-005) is
regression tests asserting on structured events, so test tasks are included throughout, not just
at the end.

**Organization**: Grouped by user story from spec.md (P1 correlated structured logs, P2 test
assertions on events, P3 runtime verbosity + cross-layer correlation).

## Format: `[ID] [P?] [Story] Description`

All work happens inside `nix develop --command bash -c '...'`. `cargo clippy --workspace
--all-targets` must stay clean throughout (feature 001's deny-level config, non-negotiable
regression check on every task that touches `.rs` files).

---

## Phase 1: Setup

- [X] T001 Add `"json"` to the `tracing-subscriber` feature list in `/home/ali/git/elementium/Cargo.toml` (`features = ["env-filter", "json"]`)
- [X] T002 Add a `CorrelationId` newtype (UUIDv4-backed `String`, `Display`/`Clone`/`Debug`) to `crates/elementium-types/src/` — check whether the `uuid` crate needs adding to `[workspace.dependencies]` in `/home/ali/git/elementium/Cargo.toml`, or generate via an existing dependency's RNG if one is already present; prefer `uuid` with the `v4` feature if none exists, since it's a common, small, well-audited dependency
- [X] T003 Verify `cargo clippy --workspace --all-targets` and `cargo test --workspace` remain clean after T001-T002 (baseline check before feature work begins)

---

## Phase 2: Foundational (blocking prerequisites for all user stories)

**⚠️ No user story implementation should begin until this phase is complete.**

- [X] T004 Switch `src-tauri/src/main.rs`'s `tracing_subscriber::fmt()` call to `tracing_subscriber::fmt().json()`, keeping the existing `EnvFilter` composition unchanged
- [X] T005 In `src-tauri/src/main.rs`, generate one `CorrelationId` at process startup (before `tauri::Builder` runs) and enter it as a root `tracing::info_span!("app_instance", correlation_id = %id)` for the remainder of `main()`'s scope, satisfying FR-010's pre-call fallback
- [X] T006 Confirm (manually, via `cargo run -p elementium` + `jq`) that stdout log lines are now valid JSON and every line carries a `correlation_id` field per quickstart.md's Story 1 section
- [X] T007 Run `cargo clippy --workspace --all-targets` and `cargo test --workspace` inside `nix develop`; fix any regressions from T004-T005 before proceeding

**Checkpoint**: JSON structured output + a always-present correlation ID field exist. User stories can now build on this.

---

## Phase 3: User Story 1 - Diagnose a live bug from structured logs alone (Priority: P1) 🎯 MVP

**Goal**: Every crate on a call's path (capture → encode → WebRTC → E2EE) emits structured events sharing that call's correlation ID, with enough fields to diagnose a failure without reading source.

**Independent Test**: Trigger a camera-start with an invalid device ID (per quickstart.md); confirm structured, correlated, diagnosable log output.

- [X] T008 [US1] In `src-tauri/src/commands/media_devices.rs`'s `get_user_media`, mint a `CorrelationId` per call (scope: `call`) and enter a `tracing::info_span!("call", correlation_id = %id, ...)` around the audio/video track-start logic; ensure the spawned `std::thread::spawn` closures for `audio_capture_loop`/`camera_pipeline_loop` inherit it (capture `tracing::Span::current()` before spawning, `.enter()` inside the closure)
- [X] T009 [US1] In `crates/elementium-media/src/audio_capture.rs` and `crates/elementium-media/src/camera.rs`, replace/augment existing log calls on the capture start/stop/failure paths with structured events carrying `device_id`, `requested_format`, and (on failure) the error's concrete type/variant — satisfying FR-003
- [X] T010 [US1] In `crates/elementium-codec/src/opus_codec.rs` and `crates/elementium-codec/src/vpx_codec.rs`, add structured error events on encode/decode failure paths carrying the error variant and relevant input dimensions/sizes (correlation ID inherited automatically from the enclosing span set up in T008-T009's call chain — no new parameter threading needed)
- [X] T011 [US1] [P] In `src-tauri/src/commands/webrtc.rs` and `crates/elementium-webrtc/src/engine.rs`/`peer_connection.rs`, mint/enter a `CorrelationId`-bearing span at peer-connection creation (`create_peer_connection`) and add structured lifecycle events (ICE state changes, connection established/closed) carrying that ID
- [X] T012 [US1] [P] In `src-tauri/src/commands/livekit.rs` and `crates/elementium-webrtc/src/livekit/*.rs`, mint/enter a `CorrelationId`-bearing span at `livekit_connect` (scope: `session`) and add structured events for connect/disconnect/subscribe lifecycle
- [X] T013 [US1] [P] In `src-tauri/src/commands/e2ee.rs` and `crates/elementium-e2ee/src/lib.rs`, add structured events for key-set/key-clear (presence/absence only, never the key value — FR-007) and for the existing frame-drop-on-no-key path, inheriting whatever call/session span is active
- [ ] T014 [US1] Manually validate quickstart.md's Story 1 section end-to-end: trigger a real camera-start failure, confirm every event from `enumerate_devices` through the camera thread's failure shares one `correlation_id`, confirm the failure event's fields are enough to diagnose without reading source (NOT YET DONE — this session's environment is headless/no display, can't drive the Tauri GUI to trigger a real getUserMedia call; the span-inheritance wiring was verified by code review + the T006 startup-log check, but a real failure trigger needs a machine with a display/camera)
- [X] T015 [US1] Run `cargo clippy --workspace --all-targets` and `cargo test --workspace` inside `nix develop`; fix any regressions from T008-T013

**Checkpoint**: Story 1 independently complete — a maintainer can diagnose a real failure from structured, correlated logs alone.

---

## Phase 4: User Story 2 - Write a test that asserts on observed behavior (Priority: P2)

**Goal**: A reusable in-process log-capture test fixture, plus 2 regression tests proving the pattern against real previously-fixed bugs.

**Independent Test**: The 2 regression tests pass today and fail if their underlying fix is reverted (per quickstart.md's Story 2 section).

- [ ] T016 [US2] Implement the custom `tracing_subscriber::Layer`-based log-capture fixture (per research.md's Test-capture fixture decision) — `Vec<Mutex<CapturedEvent>>`-backed, `on_event` hook records name/level/fields — as a small internal crate `crates/elementium-observability-test/` or a shared `#[cfg(test)]` module reachable from both `elementium-webrtc` and `src-tauri` (pick whichever avoids duplicate code; if a shared crate, add it to `[workspace.dependencies]`/members in `/home/ali/git/elementium/Cargo.toml` as `[dev-dependencies]` for consumers)
- [ ] T017 [US2] [P] Add assertion helpers to the fixture from T016: `find_event(name) -> Option<&CapturedEvent>`, `CapturedEvent::field(key) -> Option<&FieldValue>`, and a way to assert a field's string/bool/int value without manual `HashMap` lookups in every test
- [ ] T018 [US2] Write regression test 1 in `crates/elementium-webrtc/src/` (test module near `pc_io_loop`/`encrypt_or_drop`): simulate `encrypt_frame` returning `None` (no key set) on the outbound frame-write path, run it under the T016 fixture via `tracing::subscriber::with_default`, and assert a structured "frame dropped" warning event was emitted with a `reason` field — confirm it fails if `encrypt_or_drop`'s drop-and-warn logic is reverted to `unwrap_or(data)`
- [ ] T019 [US2] Write regression test 2 near `resample_44100_to_48000` in `src-tauri/src/commands/media_devices.rs`: call it with `channels = 0` under the T016 fixture, assert (a) no panic (existing `channels.max(1)` guard holds) and (b) a structured event fires noting the anomalous zero-channels input was clamped — confirm it fails if that event is removed
- [ ] T020 [US2] [P] Write a dedicated secret-redaction test using the T016 fixture: exercise `elementium-e2ee`'s key-set path and `elementium-keyring`'s secret-access path under capture, then assert no captured event's fields contain the literal secret/key value (only presence/absence booleans) — satisfying SC-005's "automated scan" requirement structurally rather than via grep
- [ ] T021 [US2] Run `cargo clippy --workspace --all-targets` and `cargo test --workspace` inside `nix develop`; confirm T018-T020 pass, then temporarily revert each underlying fix one at a time to confirm each test fails as expected (manual verification step per quickstart.md, not a permanent code change)

**Checkpoint**: Story 2 independently complete — the observability-driven-test pattern is proven against 2 real bugs and reusable for future ones.

---

## Phase 5: User Story 3 - Runtime verbosity + cross-process-boundary correlation (Priority: P3)

**Goal**: Per-crate/module log verbosity changes via env var without rebuild; correlation IDs consistent across the Tauri command layer and `elementium-webrtc`.

**Independent Test**: `RUST_LOG=elementium_webrtc=debug` changes only that crate's verbosity; a `src-tauri` and an `elementium-webrtc` log line for the same call share a `correlation_id` value (per quickstart.md's Story 3 section).

- [ ] T022 [US3] Manually validate quickstart.md's Story 3 section: launch with `RUST_LOG=elementium_webrtc=debug,info`, confirm only `elementium_webrtc::*` targets emit `DEBUG`-level events via `jq` filtering (this should already work given `EnvFilter` was untouched in T004 — this task is verification, not new code, unless a gap is found)
- [ ] T023 [US3] If T022 reveals any crate/module where correlation ID isn't actually reaching a cross-boundary log line (e.g. a spawned thread or async task that lost the span), fix the specific gap found — file path TBD by T022's findings, likely in `crates/elementium-webrtc/src/` or `src-tauri/src/commands/`
- [ ] T024 [US3] Add a brief note to `src-tauri/README.md` or a new `docs/observability.md` (create if no `docs/` convention exists — check first) documenting the `RUST_LOG` env var pattern and the `correlation_id` field for future maintainers, since this is the "make it substantially easier to debug" payoff the whole feature is for
- [ ] T025 [US3] Run `cargo clippy --workspace --all-targets` and `cargo test --workspace` inside `nix develop`; final regression check

**Checkpoint**: SC-001 through SC-005 all satisfied; feature complete.

---

## Dependencies & Execution Order

- **Setup (T001-T003)** → blocks Phase 2
- **Foundational (T004-T007)** → blocks all user story phases (JSON output + root correlation ID must exist first)
- **Phase 3 (US1, T008-T015)**: T008 (call-scoped span in `get_user_media`) blocks T009-T010 (capture/codec structured events need a span to inherit from); T011 and T012 are independent of T008-T010 and of each other (`[P]`, different call paths — peer connection vs. LiveKit session); T013 depends on whichever span (call/session) is active at the point E2EE is invoked, so logically follows T008/T012 but touches disjoint files (`[P]`-eligible in practice, ordered here for clarity)
- **Phase 4 (US2, T016-T021)**: T016 blocks T017-T020 (fixture must exist before tests use it); T017 (P), T018, T019, T020 (P) — T018/T019 touch disjoint crates so are parallel to each other too, only not marked `[P]` because both depend on T016/T017 landing first in sequence in this doc
- **Phase 5 (US3, T022-T025)**: strictly after Phase 3 (needs real cross-crate correlation to exist) and Phase 4 (fixture pattern established); T023 conditional on T022's findings

## Parallel Example: Phase 3

```text
T008 → T009 → T010                          (call span origin + capture/codec, sequential — same call chain)
T011                                          [P] (peer connection lifecycle, independent file set)
T012                                          [P] (LiveKit session lifecycle, independent file set)
T013                                          [P] (E2EE events, independent file set)
```

## Implementation Strategy

**MVP = Setup + Foundational + Phase 3 (US1)**: JSON structured output with correlation IDs across
the real call path is independently valuable — a maintainer can already debug from logs alone
before any test-fixture work exists — and matches spec's explicit P1 priority.

**Incremental delivery**: Setup → Foundational → Phase 3 (US1) → Phase 4 (US2) → Phase 5 (US3).
Each phase's changes land as their own atomic commit(s) per this session's git-strategy mandate
(one logical change per commit, reverting any single commit leaves the workspace compiling and
`cargo clippy --workspace --all-targets` clean).
