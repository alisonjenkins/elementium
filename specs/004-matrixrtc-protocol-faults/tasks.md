# Tasks: MatrixRTC protocol faults

**Spec**: [spec.md](spec.md)

Phase 1 is measurement. Both faults are currently described in anecdotes, and an
anecdote cannot tell a fix from a coincidence.

## Phase 1: Make the faults observable

- [X] T001 [US1] Log the interval between a key being installed and the first frame it successfully decrypts, per participant, in `crates/elementium-e2ee/src/lib.rs` — the quantity US1 is about, and nothing currently reports it
- [X] T002 [US3] Log every key that reaches the bridge but is *not* installed, with the reason, in `frontend/src/shim/e2ee-bridge.ts` (already logs failures to recover material; add the parked and out-of-order cases visible from the JS side)
- [X] T003 [US2] Log membership changes alongside key rotations, so a silence can be attributed to a specific join or leave rather than to "something happened"

## Phase 2: Reproduce, in the environment that now exists

- [X] T004 [US1] Playwright test in `frontend/tests/matrixrtc/`: three participants join an Element Call room in sequence; assert the third decodes audio from the other two within SC1's bound
- [X] T005 [US2] Playwright test: three participants; one leaves; assert the remaining two keep decoding each other's audio within SC2's bound
- [X] T006 [US2] Playwright test: three participants; a fourth joins; same assertion (a joiner triggers a rotation only when the key is over ten seconds old, so the test must wait — otherwise it passes for the wrong reason). **Passes**: all three who were already in the call, and the arrival, hear everyone. The test also asserts the rotation *happened*, and that assertion was checked by deleting the wait, where it fails — so the ten-second rule is confirmed rather than assumed
- [X] T007 [US1][US2] Drive Element Call itself rather than livekit-client directly, so the rotation policy under test is the real one

## Phase 3: Fix what the reproductions show

- [X] T008 [US1] Establish whether we honour `useKeyDelay` when adopting a newly distributed key for *encryption*. **We do**, and it is not by design so much as by where we tap in: `e2ee-bridge.ts` intercepts the `setKey` message posted to livekit's worker, which Element Call only sends after its own delay (`delayBeforeUse` → `setTimeout(..., useKeyDelay)`, default **5000ms**, not the 1000ms assumed when this was written). We adopt exactly when livekit-client would. Suspect removed
- [ ] T012 [US1][US2] Decide what to do about a rotation whose key does not reach a peer within `useKeyDelay`, which is the fault that survives T008. Five seconds is a guess about to-device latency, and when the guess is wrong the peer latches the index and hears nothing until the key lands. A late key does recover the index (see T010), so this is about the length of the gap, not a permanent loss. Options worth costing: raise the delay, defer adoption until distribution is acknowledged, or re-send at a fresh index on suspicion. See the 2026-08-07 finding in `specs/003-call-media-faults/spec.md`
- [X] T009 [US2] Nothing to fix, and now for a stronger reason: with T013 done, Elementium has been in a call with these participants and neither fault appeared. Original note follows. Nothing to fix yet from T004/T005: all four Element Call scenarios pass, so the fault is not Element Call and Matrix alone on this stack. The apparent reproduction was the harness reusing a device id with a discarded crypto store — see the finding in spec.md. Next step is to put Elementium in a call against these Playwright participants, which is now possible
- [X] T013 [US1][US2] Reproduce with Elementium as one participant and Playwright as the other two, using `just dev-test-env`. That is the one configuration not yet tested, and the only one the user has ever seen the fault in. **Run, and it does not reproduce**: `just call-peers` plus `just app-join`, three minutes, 7,250 outbound frames all sent, 14,500 inbound decoded, no decrypt failure, both peers hearing 2/2 throughout. See the 2026-08-08 finding in `specs/003-call-media-faults/spec.md`
- [ ] T010 [US3] For any key that dies inside `RTCEncryptionManager`, decide whether it is ours to work around or upstream's to fix, and record which. One answer is already in: livekit's failure-count latch stops retrying a key index after `failureTolerance` failures, but installing a key at that index clears the count, so a late key does revive it. The latch amplifies a persistent failure into permanent silence rather than causing one on its own

## Phase 4: Confirm

- [ ] T011 [US1][US2] Both reproductions pass against the fix, and still fail against the commit before it
