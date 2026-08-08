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
`membership-log`, `livekit-bridge`, `canvas-track`

## Rules

- Every key is present after `index.ts` finishes, whether or not the shim succeeded. A
  missing key means the module did not run at all, which is a different fault from
  `installed: false` and must not be reportable as the same thing.
- `installed: false` carries a `reason`. A shim that declines to install because the
  environment does not need it is a legitimate outcome; one that fails silently is not.
- The map is set in **both** documents — the main window and the Element Call widget frame.
  They are separately injected, and the widget is the half that carries the media.
- **No key material, tokens, or payloads.** `detail` names an API, never a value. The
  E2EE bridge in particular sees raw key material, and this map is read by tests and may be
  copied into logs.

## Failure the contract must catch

The negative control (T007): with the injection removed from `index.html`, the map is absent
entirely and the test fails naming the document it was missing from — not a generic
timeout.
