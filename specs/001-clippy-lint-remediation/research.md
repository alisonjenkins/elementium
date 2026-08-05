# Research: Clippy Lint Remediation

No `NEEDS CLARIFICATION` markers in Technical Context — this is a well-scoped internal
code-quality task against a codebase already read this session. Findings below stand in for the
usual "unknowns → research tasks" phase.

## Decision: Remediation order

**Decision**: `elementium-codec` → `elementium-media` → `elementium-e2ee` → `elementium-keyring`
→ `elementium-screen` → full-workspace verification.

**Rationale**: Matches spec priority (P1 codec by volume/risk, P2 the rest). Within P2, order by
descending error count as a proxy for effort, except `elementium-e2ee` and `elementium-keyring`
are bumped ahead of `elementium-screen` despite lower counts because they handle cryptographic
key material (spec User Story 2 rationale) — correctness there outweighs volume.

**Alternatives considered**: Smallest-first (screen → keyring → e2ee → media → codec) to build
momentum — rejected, spec explicitly prioritizes by risk/volume, not ease.

## Decision: Fix patterns per violation category

Categories from the 2026-08-05 `--keep-going` baseline (counts are workspace-wide, top offenders
first), with the concrete safe rewrite to apply:

| Category | Count | Fix pattern |
|---|---|---|
| `as_conversions` | 82 | `u8 as f32` → `f32::from(x)`; narrowing numeric casts → `u8::try_from(x)` / `.map_err(...)` or `?` into the function's existing `Result` return; where the function is infallible today, prefer a `Result`-returning refactor over `#[allow]` |
| `arithmetic_side_effects` | 78 | `a - b` on lengths/indices → `a.checked_sub(b)` propagated via `Option`/`Result`, or restructure to avoid the subtraction (e.g. iterate instead of index-math) |
| `indexing_slicing` | 56 | `frame[i]` → `frame.get(i)` propagated as `Option`; `&frame[a..b]` → `frame.get(a..b)` |
| `mul_add suggestions` (nursery `suboptimal_flops`) | 30 | `a * b + c` → `a.mul_add(b, c)` where it's float math on a hot path; mechanical |
| `missing_errors_doc` | 23 | Add `/// # Errors` section describing when the `Result` is `Err`, on every `pub fn` returning `Result` that lacks one |
| `unwrap_used` | 21 | `.unwrap()` on `Result`/`Option` → propagate via `?` if the fn returns `Result`/`Option`; convert fn to return `Result` if it doesn't and the failure is real; scoped `#[allow(clippy::unwrap_used)]` with justification only where structurally infallible (e.g. `Regex::new` on a literal) |
| missing `#[must_use]` | 28 (20+8) | Add `#[must_use]` to pure accessor/builder methods and functions per clippy's suggestion |
| `slicing` | 20 | Same as indexing_slicing — `.get(range)` |
| float↔int casts | 36 (18+18) | `f32 as u8` → explicit clamp + `as` replaced with a documented conversion helper, or restructure to avoid lossy cast; these are genuinely lossy so a scoped `#[allow]` with justification is acceptable where the truncation is intentional (e.g. quantizing a normalized float to a byte) — must be justified per-site, not blanket |
| `u8`→`f32` casts | 16 | `u8 as f32` → `f32::from(x)` (infallible, mechanical) |
| `could_be_const_fn` | 15 | Add `const` to the fn signature where clippy confirms it's possible |
| missing doc backticks | 14 | Wrap type/API names in backticks in doc comments (e.g. `LiveKit` E2EE → `` `LiveKit` `` E2EE) |
| `expect_used` | 25 (13+12) | Same treatment as `unwrap_used` |
| `redundant_clone` | 4 | Remove the clone; verify with `cargo test` that ownership still works |
| `wildcard imports` | 2 | Expand to explicit import list |
| one-offs (too-similar names, too many single-char bindings, fn too long, `Drop` early-drop, `match_same_arms`, `manual_let_else`, `unchecked_time_subtraction`, redundant `continue`, single-pattern `match`→`if let`) | ~15 | Apply clippy's own suggested rewrite per diagnostic; each is a 1-line mechanical fix per the compiler output already captured in this session's `cargo clippy --keep-going` run |

**Rationale**: Every category above has a clippy-suggested rewrite in the tool output already
captured this session (see conversation history / re-run `cargo clippy -p <crate> --all-targets`
inside `nix develop` per crate to regenerate exact file:line locations before starting each
crate's work — the baseline run's full text was captured but line numbers may drift slightly as
earlier fixes in the same file shift later lines).

**Alternatives considered**: Blanket `#[allow(clippy::pedantic)]` at crate root — rejected by
spec FR-005/FR-006 (defeats the purpose of turning the lints on).

## Decision: `elementium-e2ee` — concrete example already triaged this session

**Decision**: Use `crates/elementium-e2ee/src/lib.rs` (frame decrypt/encrypt path, ~lines
307-450 in the baseline) as the reference pattern for the rest of the crate and as a template for
similar frame-parsing code in `elementium-codec`/`elementium-media`:
- `Ok(None) => continue, Err(_) => continue` → merge to `Ok(None) | Err(_) => continue`
- `let ring = match ... { Some(r) => r, None => return Ok(None) }` → `let Some(ring) = ... else { return Ok(None) }`
- `frame[frame.len() - 1]` → `frame.last().copied()` propagated as `Option`
- `self.inner.read().unwrap()` → propagate lock-poison as an error variant (RwLock poisoning is a
  real failure mode in a multi-threaded E2EE key-manager — not a case for `#[allow]`)
- `hk.expand(...).expect(...)` on a fixed-size HKDF output → these two ARE structurally infallible
  (HKDF expand only fails if output length exceeds `255 * hash_len`, and both call sites use a
  16-byte fixed output) — scoped `#[allow(clippy::expect_used)]` with a one-line comment citing
  the RFC 5869 output-length bound is appropriate here per spec Edge Cases guidance

**Rationale**: Demonstrates both remediation modes (real fix vs. justified scoped allow) called
for by FR-005, on code already read this session.
