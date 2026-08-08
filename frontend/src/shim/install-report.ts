/**
 * Proof that each shim attached to something, rather than merely having run.
 *
 * "The page loaded" is not evidence. A shim that executed and attached to nothing looks
 * identical from outside, and that is exactly the failure an Element Web upgrade produces:
 * upstream moves a global, our replacement quietly lands on nothing, and the application
 * starts and cannot use the microphone.
 *
 * So this records `installed` from a *predicate evaluated after setup*, not from the setup
 * function returning. See `specs/007-element-web-upgrade/contracts/shim-install.md`.
 *
 * `detail` names an API and never a value. The E2EE bridge sees raw key material, and this
 * map is read by tests and may be copied into logs.
 */

export interface ShimInstall {
  installed: boolean;
  /** What was replaced, e.g. `RTCPeerConnection`, `Worker.prototype.postMessage`. */
  detail: string;
  /**
   * True when this shim deliberately declined because the environment does not need it —
   * the secret storage shim outside Tauri, for instance. Distinct from `installed: false`,
   * which means it tried and did not attach. Collapsing the two would make the test either
   * fail in a browser or pass over a real fault.
   */
  skipped?: boolean;
  /** Present only when `installed` is false. */
  reason?: string;
}

interface ShimWindow {
  __elementium_shims?: Record<string, ShimInstall>;
}

function store(): Record<string, ShimInstall> {
  const w = window as unknown as ShimWindow;
  w.__elementium_shims ??= {};
  return w.__elementium_shims;
}

/**
 * Whether `fn` is still the browser's own implementation.
 *
 * A native function stringifies to `function x() { [native code] }`; ours does not. It is a
 * blunt test, and it is the right one here: the question is not "is this our specific
 * function" but "has anything replaced the built-in", which survives our own refactors.
 */
export function isPatched(fn: unknown): boolean {
  return typeof fn === "function" && !/\[native code\]/.test(Function.prototype.toString.call(fn));
}

/**
 * Run `setup`, then record whether `verify` says it took effect.
 *
 * A throwing setup is recorded as a failure rather than propagating: one shim failing to
 * install should leave the others installed and the failure legible, not replace a
 * diagnosable page with a blank one.
 */
export function install(
  name: string,
  detail: string,
  setup: () => void,
  verify: () => boolean,
  applicable: () => boolean = () => true,
): void {
  if (!applicable()) {
    store()[name] = {
      installed: false,
      skipped: true,
      detail,
      reason: "not applicable in this environment",
    };
    return;
  }
  try {
    setup();
  } catch (e) {
    store()[name] = { installed: false, detail, reason: `threw: ${String(e)}` };
    console.error(`[Elementium] shim ${name} threw during install`, e);
    return;
  }
  const ok = verify();
  store()[name] = ok
    ? { installed: true, detail }
    : { installed: false, detail, reason: "ran, but the target was not replaced" };
  if (!ok) {
    console.error(`[Elementium] shim ${name} ran but did not attach to ${detail}`);
  }
}

/** Record a shim whose install is a value being present rather than a function replaced. */
export function record(name: string, detail: string, ok: boolean): void {
  store()[name] = ok
    ? { installed: true, detail }
    : { installed: false, detail, reason: "value not present after setup" };
}
