# Tasks: MatrixRTC protocol faults

**Spec**: [spec.md](spec.md)

Phase 1 is measurement. Both faults are currently described in anecdotes, and an
anecdote cannot tell a fix from a coincidence.

## Phase 1: Make the faults observable

- [X] T001 [US1] Log the interval between a key being installed and the first frame it successfully decrypts, per participant, in `crates/elementium-e2ee/src/lib.rs` — the quantity US1 is about, and nothing currently reports it
- [ ] T002 [US3] Log every key that reaches the bridge but is *not* installed, with the reason, in `frontend/src/shim/e2ee-bridge.ts` (already logs failures to recover material; add the parked and out-of-order cases visible from the JS side)
- [ ] T003 [US2] Log membership changes alongside key rotations, so a silence can be attributed to a specific join or leave rather than to "something happened"

## Phase 2: Reproduce, in the environment that now exists

- [ ] T004 [US1] Playwright test in `frontend/tests/matrixrtc/`: three participants join an Element Call room in sequence; assert the third decodes audio from the other two within SC1's bound
- [ ] T005 [US2] Playwright test: three participants; one leaves; assert the remaining two keep decoding each other's audio within SC2's bound
- [ ] T006 [US2] Playwright test: three participants; a fourth joins; same assertion (a joiner triggers a rotation only when the key is over ten seconds old, so the test must wait — otherwise it passes for the wrong reason)
- [ ] T007 [US1][US2] Drive Element Call itself rather than livekit-client directly, so the rotation policy under test is the real one

## Phase 3: Fix what the reproductions show

- [X] T008 [US1] Establish whether we honour `useKeyDelay` when adopting a newly distributed key for *encryption*. **We do**, and it is not by design so much as by where we tap in: `e2ee-bridge.ts` intercepts the `setKey` message posted to livekit's worker, which Element Call only sends after its own delay (`delayBeforeUse` → `setTimeout(..., useKeyDelay)`, default **5000ms**, not the 1000ms assumed when this was written). We adopt exactly when livekit-client would. Suspect removed
- [ ] T012 [US1][US2] Decide what to do about a rotation whose key does not reach a peer within `useKeyDelay`, which is the fault that survives T008. Five seconds is a guess about to-device latency, and when the guess is wrong the peer latches the index permanently. Options worth costing: raise the delay, defer adoption until distribution is acknowledged, or re-send at a fresh index on suspicion. See the 2026-08-07 finding in `specs/003-call-media-faults/spec.md`
- [ ] T009 [US2] Fix whatever T005/T006 show, file path unknown until then
- [ ] T010 [US3] For any key that dies inside `RTCEncryptionManager`, decide whether it is ours to work around or upstream's to fix, and record which. One answer is already in: the failure-count latch is **livekit's**, and reportable as such — `setKey` for a remote participant does not call `resetKeyStatus`, so a key that arrives after the index is latched cannot revive it. We must still work around it, because we cannot ship a patched livekit into Element Call

## Phase 4: Confirm

- [ ] T011 [US1][US2] Both reproductions pass against the fix, and still fail against the commit before it
