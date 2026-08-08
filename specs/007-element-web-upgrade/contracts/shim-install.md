# Contract: shim install reporting

**Consumed by**: `frontend/tests/matrixrtc/shim-contract.spec.ts` (T005)
**Produced by**: each module in `frontend/src/shim/` (T004)

The contract exists because "the page loaded" is not evidence that a shim did anything. A
shim that ran and attached to nothing looks identical from outside, and that is the failure
this feature is built to catch.

## Shape

```ts
interface ShimInstall {
  /** Whether this shim completed its installation. */
  installed: boolean;
  /**
   * What it replaced, named precisely enough to tell "ran and attached" from
   * "ran and found nothing to attach to".
   * e.g. "RTCPeerConnection", "navigator.mediaDevices.getUserMedia",
   *      "Worker.prototype.postMessage", "Storage.prototype.getItem"
   */
  detail: string;
  /**
   * True when the shim deliberately declined because the environment does not need it —
   * the secret storage shim outside Tauri, for instance. Distinct from `installed: false`,
   * which means it tried and did not attach.
   */
  skipped?: boolean;
  /** Present only when `installed` is false: why not. */
  reason?: string;
}

declare global {
  interface Window {
    __elementium_shims: Record<string, ShimInstall>;
  }
}
```

## Required keys

One per module, named after the module rather than the API it patches, so the map reads as
"which of our things is missing":

`console-bridge`, `secret-storage`, `webrtc`, `media-devices`, `e2ee-bridge`,
`membership-log`, `livekit-bridge`

Seven, not eight. `canvas-track.ts` was in the first draft of this contract and does not
belong: it is a helper used by the WebRTC shim, with no global of its own to attach to.
Reporting an install for it would have meant inventing one.

## Rules

- Every key is present after `index.ts` finishes, whether or not the shim succeeded. A
  missing key means the module did not run at all, which is a different fault from
  `installed: false` and must not be reportable as the same thing.
- `installed: false` carries a `reason`, and `skipped: true` when declining was correct.
  The secret storage shim bails out without Tauri IPC, which is right in a plain browser and
  must not read as a failure to attach — collapsing the two would make the test either fail
  in Chromium or pass over a real fault.
- **Verified, not self-reported.** `installed` comes from a predicate evaluated *after*
  setup returns, not from setup having run. The predicate asks "is this still the browser's
  own implementation", by stringifying the target and looking for `[native code]` — blunt,
  and right, because the question is whether anything replaced the built-in rather than
  whether it is our particular function. A first attempt compared the class name, which
  passes in a dev server and fails on every minified build: the wrong way round for a
  release gate.
- The map is set in **both** documents — the main window and the Element Call widget frame.
  They are separately injected, and the widget is the half that carries the media.
- **No key material, tokens, or payloads.** `detail` names an API, never a value. The
  E2EE bridge in particular sees raw key material, and this map is read by tests and may be
  copied into logs.

## Failure the contract must catch

The negative control (T007): with the injection removed from `index.html`, the map is absent
entirely and the test fails naming the document it was missing from — not a generic
timeout.
