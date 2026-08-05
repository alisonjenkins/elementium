# Implementation Plan: Clippy Lint Remediation

**Branch**: `001-clippy-lint-remediation` | **Date**: 2026-08-05 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-clippy-lint-remediation/spec.md`

## Summary

`[workspace.lints.clippy]` in root `Cargo.toml` now denies pedantic, nursery, and a curated
panic/cast/slice-safety list across all 8 workspace crates. Baseline `cargo clippy --workspace
--all-targets --keep-going` (2026-08-05, inside `nix develop`) flags ~740 errors in 5 crates.
This plan fixes them crate-by-crate, largest/highest-risk first, favoring safe rewrites over
`#[allow]`, verified by a clean workspace-wide clippy + test run at the end.

## Technical Context

**Language/Version**: Rust 1.93.1 (via `rust-overlay`, pinned in `flake.nix`), edition 2024

**Primary Dependencies**: N/A — no new dependencies; existing crates only (see root `Cargo.toml`:
tokio, serde, thiserror/anyhow, str0m, cpal/opus, vpx-encode, nokhwa/turbojpeg, xcap, tauri)

**Storage**: N/A

**Testing**: `cargo test --workspace` (existing suites); `cargo clippy -p <crate> --all-targets`
per crate, `cargo clippy --workspace --all-targets` for final verification

**Target Platform**: Linux desktop (Tauri app); build requires `nix develop` (provides
`clang`/`mold` per `.cargo/config.toml` linker config — plain `cargo` on this host fails with
`linker \`clang\` not found`)

**Project Type**: Desktop app (Tauri) + supporting library crates — single Cargo workspace, no
frontend/backend split relevant to this feature

**Performance Goals**: N/A (lint remediation, not a performance feature) — must not regress
existing hot-path performance in `elementium-codec`/`elementium-media` (audio/video encode);
where a safe rewrite has measurable cost, prefer a scoped justified `#[allow]` per FR-005 instead

**Constraints**: Zero blanket `#[allow(...)]` at crate/module level (FR-006); every scoped
`#[allow]` needs a one-line justification (FR-005); no downgrading of the 15 configured deny lint
groups (FR-006); behavior-preserving fixes only unless a fix is a genuine bug fix, in which case
call it out explicitly (FR-004)

**Scale/Scope**: 5 crates to remediate (`elementium-codec` 228+267, `elementium-media` 104+104,
`elementium-e2ee` 62+85, `elementium-keyring` 13+23, `elementium-screen` 12+12 lib+test errors —
counts from the 2026-08-05 `--keep-going` baseline); 3 crates (`src-tauri`, `elementium-types`,
`elementium-webrtc`) already clean and checked only in final verification

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

`.specify/memory/constitution.md` is still the unfilled template (no ratified principles) — no
project-specific gates apply. No violations to justify; Complexity Tracking section omitted.

## Project Structure

### Documentation (this feature)

```text
specs/001-clippy-lint-remediation/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md        # Phase 1 output
└── tasks.md             # Phase 2 output (/speckit-tasks — not created by this command)
```

No `contracts/` — this feature has no external interface (API, CLI schema, wire format); the
"contract" being satisfied is the clippy lint config itself, already captured in `data-model.md`.

### Source Code (repository root)

```text
Cargo.toml                          # workspace root — [workspace.lints.clippy] already set
crates/
├── elementium-types/                # 0 errors — verify only
├── elementium-codec/                # 228 lib + 267 test errors — P1
│   └── src/                        # exact file/line list captured in research.md
├── elementium-media/                # 104 lib+test errors — P2
├── elementium-screen/               # 12 lib+test errors — P2
├── elementium-webrtc/               # 0 errors — verify only
├── elementium-keyring/              # 13 lib + 23 test errors — P2
└── elementium-e2ee/                  # 62 lib + 85 test errors — P2
    └── src/lib.rs                   # already partially triaged (see research.md)
src-tauri/                           # 0 errors — verify only
```

**Structure Decision**: Existing workspace layout is unchanged. Each crate's `src/` is edited
in place; no new crates, files, or directories beyond the `specs/001-...` planning docs. Each
crate lands as one atomic, independently-clippy-clean unit of work per FR-007.

## Complexity Tracking

*No constitution violations — section not applicable.*
