# Quickstart: Validating Clippy Lint Remediation

## Prerequisites

- `nix develop` shell (provides `clang`/`mold` needed by `.cargo/config.toml`; plain `cargo`
  on this host fails with `linker \`clang\` not found`)
- Workspace root `Cargo.toml` already has `[workspace.lints.clippy]` configured, and all 8
  member crates already have `[lints]\nworkspace = true` (done 2026-08-05, prerequisite to this
  feature, not part of it)

## Per-crate validation (run after each crate's remediation, per FR-007)

```bash
nix develop --command bash -c 'cargo clippy -p elementium-codec --all-targets'
nix develop --command bash -c 'cargo test -p elementium-codec'
```

Repeat for `elementium-media`, `elementium-e2ee`, `elementium-keyring`, `elementium-screen` in
that priority order. Expected: exit code 0, zero errors, all tests pass (SC-001, SC-002).

## Final full-workspace validation (User Story 3 / FR-003)

```bash
nix develop --command bash -c 'cargo clippy --workspace --all-targets'
nix develop --command bash -c 'cargo test --workspace'
```

Expected: both exit 0. This also re-checks the 3 crates with a 0-error baseline
(`src-tauri`, `elementium-types`, `elementium-webrtc`) haven't regressed.

## Checking for suppression instead of fixes (SC-003)

```bash
git diff main... -- '*.rs' | grep -n '#\[allow(clippy::'
```

Every hit must be function/expression-scoped (not `#![allow(...)]` at module/crate top) and
immediately preceded by a one-line comment explaining why a safe rewrite wasn't used. Any
`#![allow(clippy::pedantic)]` / `#![allow(clippy::nursery)]` or similar blanket suppression at
crate or module level is a spec violation (FR-006) and must be reverted in favor of real fixes.
