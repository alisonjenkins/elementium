# Implementation Plan: Element Web upgrade and patch maintenance

**Branch**: `007-element-web-upgrade` | **Date**: 2026-08-08 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `specs/007-element-web-upgrade/spec.md`

## Summary

Move from Element Web v1.12.11 to v1.12.25, and put in place the two things whose absence
is the real subject: a way to know the shims still work on a version we did not build, and
a way to carry a change to Element Web that can also be offered upstream.

The approach has three parts, and the division between them is the design:

1. **Assert instead of assuming.** Every build-time patch confirms its own effect, and every
   shim reports whether it installed. This lands *before* the version moves, so a failure
   after the bump has something to be compared against.
2. **Upgrade against that instrumentation**, measured with feature 003's media checks rather
   than by the application appearing to start.
3. **Carry patches as commits on a rebased fork branch**, so that carrying a change and
   contributing it are the same operation. Verified in research: a rebase drops a patch once
   upstream takes it verbatim.

## Technical Context

**Language/Version**: TypeScript (shims, Playwright tests), Bash (build scripts), Rust
(startup logging of the build record). Node 22.23.2 in the dev shell; **Node 24 for the
Element Web build only** — see research R2.

**Primary Dependencies**: Element Web v1.12.25 (release tarball, and the upstream git tree
when a patch is carried); its bundled Element Call widget; livekit-client 2.21.0 inside
that widget; pnpm and nx for building upstream from source.

**Storage**: none. Two small generated files: `element-web-dist/.elementium-build.json`
(build record) and `element-web-patches.md` (patch manifest, generated from the branch).

**Testing**: Playwright (`frontend/tests/matrixrtc/`) for the shim contract; the existing
feature-003 media measurement (`just call-peers` + `just app-join`) for the upgrade itself;
`cargo test --workspace` for anything the Rust side touches.

**Target Platform**: Linux desktop (Tauri + WebKitGTK). The upgrade is platform-independent;
the media verification is not, and is done where the app runs.

**Project Type**: desktop application with a vendored third-party web frontend.

**Performance Goals**: none for this feature. The media measurements are correctness gates
(all frames sent, zero decrypt failures), not performance targets.

**Constraints**:

- `element-web-dist/` is git-ignored and rebuilt from scratch by `fetch-element-web.sh`.
  Nothing may be hand-edited there and expected to survive.
- The release tarball stays the default source. Building from upstream git is only required
  once a patch is carried, so a developer who carries none never pays for it.
- The autojoin injection carries a live access token. Anything that records or reports build
  state must treat its presence as a release blocker, not as a diagnostic.

**Scale/Scope**: nine shim modules (~2,700 lines) whose install must be provable; one build
script to make assertive; two `just` recipes for sync, three for the patch workflow; one
upstream version bump spanning fourteen releases.

## Constitution Check

*GATE: must pass before Phase 0 research. Re-checked after Phase 1 design.*

**Skipped — the constitution is an unfilled template.** `.specify/memory/constitution.md`
still contains `[PRINCIPLE_1_NAME]`, `[GOVERNANCE_RULES]` and the rest of the placeholders,
so there are no principles to gate against. Reporting "no violations" from an empty rulebook
would be misleading, so this is recorded as skipped instead.

The standing mandates in `CLAUDE.md` were used in its place, and two bear on this plan:

| Mandate | Effect on the design |
|---|---|
| Never mutate an external system unprompted | Creating a fork under the user's account is T023, and it blocks T017. Nothing in Phase 5 may assume the fork exists |
| ISO8601 UTC in all files, logs and reports | The build record carries a UTC timestamp (T003) |

Both were satisfied by the analysis pass rather than by this plan, and are restated here so
a later reader does not have to reconstruct why T023 exists.

**Post-design re-check**: unchanged. No design decision below conflicts with either mandate.

## Project Structure

### Documentation (this feature)

```text
specs/007-element-web-upgrade/
├── spec.md              # Feature specification
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output — the two generated files' schemas
├── quickstart.md        # Phase 1 output — how to validate the upgrade
├── contracts/           # Phase 1 output — shim and upstream contracts
│   ├── shim-install.md
│   ├── upstream-surface.md
│   └── cli.md
├── checklists/
│   └── requirements.md
└── tasks.md             # 23 tasks across 6 phases
```

### Source Code (repository root)

```text
elementium.config.sh              # upstream pin; gains fork + patch-branch settings (T017)
scripts/
├── fetch-element-web.sh          # release download; git mode repaired for the monorepo (T015)
└── patch-element-web.sh          # injection; every step made assertive (T001, T002, T003)
justfile                          # element-web-sync / -rebase / -pr recipes (T012, T018, T020)
flake.nix                         # Node 24 for the Element Web build only (T016)
frontend/src/shim/                # nine modules; each reports its install (T004)
├── index.ts
├── webrtc-shim.ts
├── media-devices.ts
├── secret-storage.ts
├── e2ee-bridge.ts
├── console-bridge.ts
├── membership-log.ts
├── livekit-bridge.ts
└── canvas-track.ts
frontend/tests/matrixrtc/
└── shim-contract.spec.ts         # new: proves the shims installed (T005, T006, T007)
src-tauri/src/                    # logs the build record at startup (T003)
docs/element-web.md               # new: upgrade procedure and the classification rule (T014)
element-web-patches.md            # new: generated patch manifest (T019)
element-web-dist/                 # git-ignored, rebuilt; gains .elementium-build.json
```

**Structure Decision**: no new project or module. This feature edits build scripts, adds one
Playwright spec, adds an install-reporting line to each existing shim, and adds two
generated files. The vendored frontend stays where it is, git-ignored and rebuilt, because
checking in a third-party build output would make every upgrade a review of a hundred
thousand minified lines.

## Design Decisions

### D1: The three homes for a change

From the spec, restated here because it is the decision the rest depends on.

| Kind of change | Home | Test |
|---|---|---|
| Host integration | Runtime shim in `frontend/src/shim/` | "Would this make sense only in a browser with our native backend?" |
| Product change | Commit on the fork's patch branch | "Would I be willing to open the pull request today?" |
| Packaging | `scripts/patch-element-web.sh` | "Is this about how the artefact is assembled, not what it does?" |

Documented in `docs/element-web.md` by T014, not left in the spec — a rule that lives only
in a spec is a rule that gets forgotten the first time someone adds a shim that should have
been a patch.

### D2: Prove the install, not the load

A shim that ran but attached to nothing is the failure this feature exists to catch, and it
is indistinguishable from success at the level of "the page loaded". So the report is
`{installed, detail}` per module, where `detail` names what was replaced. Asserted in the
main window **and** in the Element Call widget frame, which is a separate document with its
own injection and is the half that carries the media.

### D3: Assert at build time, fail the build

The current `sed` and `awk` steps exit 0 when they match nothing. Each becomes an assertion
on its own postcondition. This is deliberately cruder than parsing the HTML: the failure
mode being fixed is silence, and a grep for the marker after the fact catches it without
introducing a parser that itself needs maintaining.

### D4: Patches as commits, not as a patch series

Chosen over a Debian-style `patches/*.patch` directory because carrying and contributing
become one mechanism with no translation step. Research R3 confirms a carried commit drops
out on rebase once upstream takes it verbatim, and identifies the case that does not — a
change amended in review conflicts instead, and needs `git rebase --skip`. T018's report
therefore has three outcomes, not two.

### D5: The release tarball stays the default

Building upstream from source costs a pnpm install of a monorepo and an nx build. That is
worth paying when a patch is carried and not before, so `ELEMENT_WEB_SOURCE=release` remains
the default and the git path is repaired but not made mandatory.

## Complexity Tracking

No constitution violations to justify — the gate was skipped, not passed with exceptions.

One complexity is worth naming even though nothing forced it: this feature introduces a
second repository (the fork) into a project that currently has one. The alternative was the
patch-series directory, which keeps everything here at the cost of making contribution
manual. The trade was made deliberately in D4, and it is the thing to revisit first if the
patch workflow turns out to cost more than it saves.
