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

- [X] T001 Make every step of `scripts/patch-element-web.sh` assert its own effect: the CSP `sed` must confirm the meta tag is gone, each `awk` injection must confirm the marker is present in the output, and each copy must confirm the destination exists. Exit non-zero with the step name when an assertion fails. Today all of them exit 0 having done nothing, which is how an upstream `index.html` change becomes a successful build and a broken application
- [X] T002 Fail loudly when `element-web-dist/widgets/element-call` is missing rather than skipping the widget injection with a message, in `scripts/patch-element-web.sh` — a release that stopped bundling Element Call must not produce an app that starts and cannot call
- [X] T003 Write a build record to `element-web-dist/.elementium-build.json` at patch time, and log it once at startup from `src-tauri/`: the Element Web version, the source (release or git), an ISO8601 UTC build timestamp, the list of applied patches, a fingerprint of `widgets/element-call/assets` (its Element Call version is not pinned separately and would otherwise drift unobserved — see the Assumptions in `spec.md`), and **whether the autojoin driver was injected**. That last field is not diagnostics: the injection carries a live access token and dials a call on startup, so a release build must refuse to proceed when it is set. A bug report that does not say what was running costs a round trip to find out

## Phase 2: Prove the shims install (US2)

The baseline has to pass on v1.12.11 before the bump, or a failure after it cannot be
attributed.

- [X] T004 [US2] Have each shim record that it installed, in `frontend/src/shim/index.ts` and each module: a `window.__elementium_shims` map of name to `{installed, detail}`. `detail` names what it replaced (for example `RTCPeerConnection`, `navigator.mediaDevices.getUserMedia`), so a shim that ran but attached to nothing is distinguishable from one that did not run
- [X] T005 [US2] Playwright test in `frontend/tests/matrixrtc/shim-contract.spec.ts`: load the patched Element Web, assert every shim reports installed, in the main window **and** in the Element Call widget frame — the widget is a separate document with its own injection, and it is the half that carries the media
- [X] T006 [US2] **Already covered, and left where it is.** The `setKey` message is asserted at runtime by `call-faults.spec.ts` (via `observeKeys`/`keysInstalled`) and `mxMatrixClientPeg` by its encryption precondition test — both in real calls, which is the only way to check either. Duplicating a three-participant call in the contract spec would cost 30s a run to re-measure what already runs. Recorded in `contracts/upstream-surface.md`. Original task follows. Assert the contracts the shims depend on, not just that they installed: the E2EE bridge sees a `setKey` worker message with `participantIdentity`/`key`/`keyIndex` during a real call, and `mxMatrixClientPeg` is reachable. These are upstream's internals and are what an upgrade actually threatens. Extend `frontend/tests/matrixrtc/shim-contract.spec.ts`
- [X] T007 [US2] Negative control: with the injection deliberately removed from `index.html`, T005 must fail and name the missing shim. Run it once by hand and record the output as a finding in `spec.md`, where this repository keeps its evidence — a test that has never failed is not known to be able to

## Phase 3: The upgrade itself (US1)

- [X] T008 [US1] **Done, and reverted.** Every assertion and all 21 Playwright tests passed on v1.12.25; Elementium itself could not establish a peer connection. Pin returned to v1.12.11 — see the 2026-08-08 finding in `spec.md`. Original task follows. Bump `ELEMENT_WEB_VERSION` to `v1.12.25` in `elementium.config.sh`, re-fetch, and run the Phase 2 checks. Record what fails; the spec's reading says nothing should, and that prediction is worth testing rather than assuming
- [X] T009 [US1] **Run, and it fails on v1.12.25**: 766 of 6,500 frames sent, no inbound audio, no remote video. Localised rather than merely observed — `setLocalDescription` is called 8 times on v1.12.11 and 0 times on v1.12.25. Original task follows. Run the media measurement that feature 003 uses — `just call-peers` plus `just app-join` — and confirm audio both ways, both remote video tracks, keys exchanged, and zero decrypt failures. "The application starts" is not the bar, and was never the bar for this project
- [X] T010 [US1] **21 passed on v1.12.25**, including every Element Call scenario. The harness and the participants are unaffected by the upgrade; only Elementium's own transport is. Original task follows. Run the full Playwright suite and the Rust workspace tests against the new version. The Playwright participants run real Element Web from `element-web-dist`, so an upstream change can break the *harness* as easily as the product
- [ ] T024 [US1] Bring the `RTCPeerConnection` shim up to what livekit-client now expects. It never calls `setLocalDescription` on v1.12.25, so our offer is never sent and the connection times out; the handshake reports `protocol: 17`, so the client has moved. Start by logging every method and property access on the shim and diffing the call sequence between the two versions — the failing call happens *before* `setLocalDescription`, and nothing currently records what it is. **This blocks T008**, and is a larger piece of work than the upgrade it blocks
- [ ] T011 [US1] Fix whatever T008–T010 turn up, one commit per cause, and record each in `spec.md` as a finding — including the ones that turn out to be our own assumptions rather than upstream's changes

## Phase 4: Staying in sync is one command (US3)

- [X] T012 [US3] `just element-web-sync <version>` in `justfile`: fetch the named release, re-apply patches and config, run the Phase 2 shim contract checks, and print a verdict. One command, and a report that names what broke rather than a build log to read
- [X] T013 [US3] Have the sync report the upstream release notes range between the pinned version and the target, so the person running it can see what changed without leaving the terminal. Fourteen releases of blind diff is why this one waited so long
- [X] T014 [P] [US3] Document in `docs/element-web.md`: how to bump a version, what to run, what to do when a shim contract fails, which failures mean "upstream moved" rather than "we broke it", and how to revert the pin when an upgrade cannot be fixed quickly (`fetch-element-web.sh` wipes `element-web-dist`, so the revert is a re-fetch rather than an undo). **Include the rule for where a change belongs** — host integration stays a runtime shim, a product change becomes a commit on the patch branch, packaging stays in the patch script — with the test for each, from `spec.md`. That rule is what makes the patch set stay small, and it is the first thing to be forgotten if it lives only in a spec

## Phase 5: Carrying patches, and giving them back (US4, US5)

Blocked on nothing but the decision below. Every task here is dead weight until there is a
first patch to carry — and the mechanism is not known to work until one has gone through
it, which is the argument on the other side.

- [X] T015 [US4] **Done and verified end to end**: cloned v1.12.25, `scripts/layered.sh`, `nx build` from `apps/web`, output taken from `apps/web/webapp`. Also un-shallowed the clone — a shallow one cannot be rebased onto another tag, which is the entire point of the patch branch. Repair `fetch_git()` in `scripts/fetch-element-web.sh` for the current upstream: pnpm rather than yarn, `nx build` from `apps/web` rather than a root `build` script, and the new output directory rather than `webapp/`. All three lines are wrong today, and this is the path both carrying and contributing need
- [X] T016 [US4] **Settled, and the answer is no change**: Node 22.23.2 builds v1.12.25 cleanly, twice. The prediction that Node 24 would be needed is withdrawn — see research.md. Original task follows. Settle the Node version question by building: upstream's `.node-version` says 24, the dev shell provides 22.23.2, and `engines` says `>=22.18`. If 22 does not build it, add node 24 to `flake.nix` for the Element Web build only, so the rest of the workspace is undisturbed
- [X] T017 [US4] Add fork and patch-branch settings to `elementium.config.sh`: the fork remote, the patch branch name, and the upstream tag it is currently rebased onto. The pinned upstream tag stays the single source of truth for what we are building against, whether or not a patch is applied
- [ ] T018 [US4] `just element-web-rebase <version>` in `justfile`: fetch the upstream tag into the source cache, rebase the patch branch onto it, and report per commit whether it applied, conflicted, or became empty. An empty commit means upstream took it — that is the signal a contribution landed, and it should be stated as such rather than left as a silent drop
- [ ] T019 [US5] `element-web-patches.md` at the repo root, generated from the patch branch rather than written by hand: one row per commit with its subject, why it exists, whether it is meant to go upstream, and the pull request link if it has been offered. Hand-written lists of patches stop being true
- [ ] T020 [US4] `just element-web-pr <commit>`: produce a branch from the current upstream tag with that one commit cherry-picked, ready to push and open as a pull request. The point of the whole arrangement is that this needs no translation step
- [ ] T021 [US4] Exercise the mechanism end to end with a real change rather than a placeholder: carry it on the patch branch, generate a PR branch from it with T020, then **simulate upstream taking it by committing the same change onto a local branch standing in for the upstream tag, and rebasing onto that** — confirming the commit drops out by patch-id with nobody editing anything. A patch workflow nobody has run is a patch workflow that does not work

## Phase 6: Close

- [ ] T022 Record in `spec.md` what the upgrade actually cost, against the three blockers predicted from reading. The value of that comparison is in the places the reading was wrong
- [X] T023 [US4] **Answered: a public fork on the user's account** — `alisonjenkins/element-web`, branch `elementium`. Public is not a preference: Element Web is AGPL-3.0 and this project declares AGPL-3.0-or-later, so a shipped source patch is a modified AGPL work whose source has to be available to whoever receives it. Two things follow that no tooling can arrange: upstream requires a **CLA** rather than a DCO, signed once and personally; and this repository has no `LICENSE` file despite declaring one in `Cargo.toml`. Original task follows. Agree with the user where the Element Web fork lives before anything in Phase 5 assumes it. It is a repository under their account, which makes it an external system we do not create unprompted, and it decides where CI for the patch branch runs. `spec.md` open question 1; nothing downstream of it can be settled here

## The decision this ordering assumes

Phases 1–4 stand alone and deliver the upgrade, the safety net, and the sync path with an
empty patch set. Phase 5 builds the machinery for carrying and contributing patches, and it
is placed last on the assumption that **the upgrade is worth having before the fork
exists**.

The argument against, from the spec's open question 3: a mechanism nobody has exercised is
a mechanism that does not work, and the first time it is needed will be the worst time to
find that out. T021 exists to answer that, but only once Phase 5 is reached.

Two things are still the user's to decide: where the fork lives — now T023, rather than an
assumption Phase 5 makes quietly — and whether Phase 5 moves ahead of Phase 3.
