# Elementium Constitution

## Core Principles

### I. Errors Name Their Cause (NON-NEGOTIABLE)

Every fallible function returns an error type of its own: one enum per **failure surface**,
one variant per distinct thing that can fail, and each variant carries the underlying error
as a source.

A failure surface is usually a single function. It may be shared by two or more only when
both of these hold:

- *every* caller handles *every* variant identically — `write_audio` and `write_video` fail
  in the same ways and each caller drops the frame and continues; and
- **the error still names which operation produced it.** A shared enum that does not is a
  step backwards from a string, because `RTP write failed` in a log no longer says whether
  the audio or the video path failed, and the two have entirely different consequences.
  Carry the operation as a field — a small `enum` of the operations that share the surface,
  rendered in the `Display` — so the type stays shared while the origin stays legible.

Anything less means separate enums. This replaces a stricter "one enum per function", which
manufactured the near-duplicate code the principle exists to prevent — but the relaxation is
conditional, and the second condition above was added after the first attempt at sharing
lost exactly the information a reader would need.

- **No stringly errors.** `ok_or("something went wrong")`, `map_err(|e| format!(...))` and
  `Box<dyn Error>` at a fallible boundary all destroy the cause. A caller cannot match on a
  string, and a reader cannot tell which of five failure modes occurred.
- **The source is preserved**, via `#[source]` (or `#[from]`), so the chain can be walked.
  A variant that swallows its cause is as bad as a string.
- **Variants are specific.** If a function can fail in five ways, its enum has five
  variants. "Other" and "Unknown" are not variants; they are an admission that the enum was
  not thought through.
- **An error that is returned is logged or handled by its caller, never both dropped and
  silent.** A silent `Err` is invisible twice: absent from the log and absent from the
  behaviour until something downstream misbehaves.
- **A function that receives an error returns one.** If something fallible told you why it
  failed, that reason belongs in a `Result` for the caller to act on or report. Anything that
  converts it to an absence — `Result::ok`, `.ok()?`, `let _ = fallible()`, `if let Ok(v)`
  with no `else` — discards a diagnosis someone else has already paid to produce. The absence
  then travels: a caller sees `None` and cannot say whether the thing was missing, malformed
  or refused, and neither can the log line it writes.

  This binds a private helper as much as a public boundary. A helper returning `Option` where
  it could return `Result` has destroyed the information before its caller had the chance to
  report it, and the caller's warning can then only describe the input as a whole.
  `"1920 x 1080".parse::<u32>()` says precisely which character is wrong; collapse that to
  `None` and someone who typed one stray space must guess which of their two numbers was
  rejected.

  Two good reasons not to, and they are the only ones that need no argument:

  - **There is no diagnosis to carry.** A `strip_prefix` that did not match, a lookup that
    found nothing, a `next()` on an empty iterator. Nothing was diagnosed, so `Option` is the
    honest type and no cause is being thrown away.
  - **The cause cannot be propagated from where it arises** — a `Drop` impl, a best-effort
    debug feature, a callback with a fixed signature. Then drop it *loudly*: log it at the
    point of discard, with a comment saying why it cannot be returned.

  Any other reason is one to state in a comment at the call site, not to leave implied.

This principle is not aesthetic. On 2026-08-09 a single
`ok_or("No pending offer to match answer")?` — a bare string, with no source, no variant and
no tracing call — froze the signalling state of every call, tore the connection down every
fifteen seconds, and read so much like a third-party library's message that it was
attributed to livekit-client for several hours while seven builds were shipped against the
wrong diagnosis. The cost of that one line was an afternoon.

### II. Instrumentation Distinguishes Cause From Consequence

A log line exists to answer a question that could not otherwise be answered.

- **Counters over adjectives.** "Dropping frame" says a symptom. `derived=4 forwarded=0
  ipc_failures=0` says whose fault it is. Prefer instrumentation that separates "we were
  never asked" from "we were asked and refused".
- **A silent early return in code under investigation is a defect.** If a function declines
  to act, it says so, or the log cannot distinguish "never ran" from "ran and declined" —
  which need opposite fixes.
- **Do not over-claim in the message.** A warning that asserts where the fault lies will be
  believed. If the code cannot know, the message must not say.
- **Throttle, do not flood.** A condition that is normal during startup, reported per frame,
  buries the one that is not.

### III. Tests Must Be Shown To Fail

A test that has never failed proves nothing about the bug it claims to pin.

- Write the test, run it against the unfixed code, watch it fail, then fix. Where that is
  impractical, revert the fix afterwards and confirm the failure.
- **A stub may not be more permissive than the thing it stands for.** `MediaStreamTrack` was
  stubbed as constructible; browsers throw. No test could catch the resulting crash, and one
  shipped.
- The comment on a test says which regression it pins and what it cost, not what the code
  does.

### IV. Secrets Never Reach The Log

Logs are written to disk and attached to bug reports.

- Never: key material, access tokens, SDP bodies (they carry ICE credentials and DTLS
  fingerprints), or the contents of signalling payloads.
- Always acceptable: kinds, sizes, counts, indices, non-secret fingerprints, and redacted
  URLs.
- Anything written to disk is created with mode 0600 and refuses to follow an existing path.

### V. The Diagnosis Precedes The Fix

Changing code because a symptom moved is not debugging.

- A fix names the mechanism it addresses, in evidence: a file and line, or a log line with a
  timestamp. "This might be it" is a hypothesis to test, not a commit.
- When a fix does not work, the first question is whether the diagnosis was wrong, not what
  else to change. Seven consecutive builds on 2026-08-09 each took the newest symptom as the
  specification; the result oscillated rather than converged.
- Verify the thing under test is the thing running. A whole test round was spent on a
  snapshot that predated the fix being tested.

## Rust Standards

The workspace denies `pedantic`, `nursery`, `unwrap_used`, `expect_used`, `indexing_slicing`,
`arithmetic_side_effects`, `as_conversions`, `panic`, `missing_const_for_fn`,
`too_many_lines` (100) and `too_many_arguments` (7). These are not negotiable per-file.

- Reach for an extracted helper or a grouping struct before an `#[allow]`.
- An `#[allow]` that is genuinely warranted is scoped to the item and carries a comment
  saying why.
- Never place code between an existing `#[allow]` and the item it belongs to; it silently
  transfers the exemption.

## Development Workflow

- **Atomic commits.** One logical change each; reverting one alone leaves the tree building.
- **Commit messages explain why**, including what the bug cost and how it was found. The
  next person debugging this will read the log before the code.
- **Comments say why, never what.** A comment restating the code is noise; a comment
  recording the fault a line prevents is the most valuable thing in the file.
- **Never squash.** Rebase-and-merge, or a merge commit.

## Governance

This constitution supersedes convenience. A change that violates a principle is either
rewritten or the principle is amended deliberately — not waived in passing.

Amendments are made when a failure teaches something general. Each principle here was
written after it was paid for.

**Version**: 1.3.0 | **Ratified**: 2026-08-09 | **Last Amended**: 2026-08-10
