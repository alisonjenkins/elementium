# Tasks: Element Web upgrade and patch maintenance

**Spec**: [spec.md](spec.md)

Ordered so the upgrade is observable before it happens. Phases 1 and 2 change nothing
about which version we run; they make a broken version say so. Bumping the pin first would
mean debugging an upgrade with the same instruments that let it break quietly in the first
place.

The fork mechanism is last, and deliberately so — see the note at the end, which is the
sequencing question from the spec and the one decision here that is not mine to make.

## Phase 1: Make the build honest (SC5, SC6)

Nothing in this phase depends on the upgrade, and all of it is worth having regardless.

- [ ] T001 Make every step of `scripts/patch-element-web.sh` assert its own effect: the CSP `sed` must confirm the meta tag is gone, each `awk` injection must confirm the marker is present in the output, and each copy must confirm the destination exists. Exit non-zero with the step name when an assertion fails. Today all of them exit 0 having done nothing, which is how an upstream `index.html` change becomes a successful build and a broken application
- [ ] T002 [P] Fail loudly when `element-web-dist/widgets/element-call` is missing rather than skipping the widget injection with a message, in `scripts/patch-element-web.sh` — a release that stopped bundling Element Call must not produce an app that starts and cannot call
- [ ] T003 [P] Write the Element Web version, source (release or git), and the list of applied patches into `element-web-dist/.elementium-build.json` at patch time, and log it once at startup from `src-tauri/`. A bug report that does not say what was running costs a round trip to find out

## Phase 2: Prove the shims install (US2)

The baseline has to pass on v1.12.11 before the bump, or a failure after it cannot be
attributed.

- [ ] T004 [US2] Have each shim record that it installed, in `frontend/src/shim/index.ts` and each module: a `window.__elementium_shims` map of name to `{installed, detail}`. `detail` names what it replaced (for example `RTCPeerConnection`, `navigator.mediaDevices.getUserMedia`), so a shim that ran but attached to nothing is distinguishable from one that did not run
- [ ] T005 [US2] Playwright test in `frontend/tests/matrixrtc/shim-contract.spec.ts`: load the patched Element Web, assert every shim reports installed, in the main window **and** in the Element Call widget frame — the widget is a separate document with its own injection, and it is the half that carries the media
- [ ] T006 [US2] Assert the contracts the shims depend on, not just that they installed: the E2EE bridge sees a `setKey` worker message with `participantIdentity`/`key`/`keyIndex` during a real call, and `mxMatrixClientPeg` is reachable. These are upstream's internals and are what an upgrade actually threatens. Extend `frontend/tests/matrixrtc/shim-contract.spec.ts`
- [ ] T007 [US2] Negative control: with the injection deliberately removed from `index.html`, T005 must fail and name the missing shim. Run it once by hand and record the output in this file — a test that has never failed is not known to be able to

## Phase 3: The upgrade itself (US1)

- [ ] T008 [US1] Bump `ELEMENT_WEB_VERSION` to `v1.12.25` in `elementium.config.sh`, re-fetch, and run the Phase 2 checks. Record what fails; the spec's reading says nothing should, and that prediction is worth testing rather than assuming
- [ ] T009 [US1] Run the media measurement that feature 003 uses — `just call-peers` plus `just app-join` — and confirm audio both ways, both remote video tracks, keys exchanged, and zero decrypt failures. "The application starts" is not the bar, and was never the bar for this project
- [ ] T010 [US1] Run the full Playwright suite and the Rust workspace tests against the new version. The Playwright participants run real Element Web from `element-web-dist`, so an upstream change can break the *harness* as easily as the product
- [ ] T011 [US1] Fix whatever T008–T010 turn up, one commit per cause, and record each in `spec.md` as a finding — including the ones that turn out to be our own assumptions rather than upstream's changes

## Phase 4: Staying in sync is one command (US3)

- [ ] T012 [US3] `just element-web-sync <version>` in `justfile`: fetch the named release, re-apply patches and config, run the Phase 2 shim contract checks, and print a verdict. One command, and a report that names what broke rather than a build log to read
- [ ] T013 [US3] Have the sync report the upstream release notes range between the pinned version and the target, so the person running it can see what changed without leaving the terminal. Fourteen releases of blind diff is why this one waited so long
- [ ] T014 [P] [US3] Document the upgrade procedure in `docs/element-web.md`: how to bump, what to run, what to do when a shim contract fails, and which failures mean "upstream moved" rather than "we broke it"

## Phase 5: Carrying patches, and giving them back (US4, US5)

Blocked on nothing but the decision below. Every task here is dead weight until there is a
first patch to carry — and the mechanism is not known to work until one has gone through
it, which is the argument on the other side.

- [ ] T015 [US4] Repair `fetch_git()` in `scripts/fetch-element-web.sh` for the current upstream: pnpm rather than yarn, `nx build` from `apps/web` rather than a root `build` script, and the new output directory rather than `webapp/`. All three lines are wrong today, and this is the path both carrying and contributing need
- [ ] T016 [US4] Settle the Node version question by building: upstream's `.node-version` says 24, the dev shell provides 22.23.2, and `engines` says `>=22.18`. If 22 does not build it, add node 24 to `flake.nix` for the Element Web build only, so the rest of the workspace is undisturbed
- [ ] T017 [US4] Add fork and patch-branch settings to `elementium.config.sh`: the fork remote, the patch branch name, and the upstream tag it is currently rebased onto. The pinned upstream tag stays the single source of truth for what we are building against, whether or not a patch is applied
- [ ] T018 [US4] `just element-web-rebase <version>` in `justfile`: fetch the upstream tag into the source cache, rebase the patch branch onto it, and report per commit whether it applied, conflicted, or became empty. An empty commit means upstream took it — that is the signal a contribution landed, and it should be stated as such rather than left as a silent drop
- [ ] T019 [US5] `element-web-patches.md` at the repo root, generated from the patch branch rather than written by hand: one row per commit with its subject, why it exists, whether it is meant to go upstream, and the pull request link if it has been offered. Hand-written lists of patches stop being true
- [ ] T020 [US4] `just element-web-pr <commit>`: produce a branch from the current upstream tag with that one commit cherry-picked, ready to push and open as a pull request. The point of the whole arrangement is that this needs no translation step
- [ ] T021 [US4] Exercise the mechanism end to end with a real change rather than a placeholder — carry it, generate a PR branch from it, simulate upstream taking it, and confirm it drops out of the set on the next rebase without anyone editing anything. A patch workflow nobody has run is a patch workflow that does not work

## Phase 6: Close

- [ ] T022 Record in `spec.md` what the upgrade actually cost, against the three blockers predicted from reading. The value of that comparison is in the places the reading was wrong

## The decision this ordering assumes

Phases 1–4 stand alone and deliver the upgrade, the safety net, and the sync path with an
empty patch set. Phase 5 builds the machinery for carrying and contributing patches, and it
is placed last on the assumption that **the upgrade is worth having before the fork
exists**.

The argument against, from the spec's open question 3: a mechanism nobody has exercised is
a mechanism that does not work, and the first time it is needed will be the worst time to
find that out. T021 exists to answer that, but only once Phase 5 is reached.

Two things are still the user's to decide: where the fork lives (open question 1), and
whether Phase 5 moves ahead of Phase 3.
