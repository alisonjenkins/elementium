/**
 * Parses the coded JSON envelope Tauri commands now build for their `Result<T, String>`
 * failure, so a shim can branch on a stable `code` instead of matching prose.
 *
 * See `specs/BACKLOG-2026-08-09-errors.md` item X2 and `src-tauri/src/commands/mod.rs`'s
 * `IpcErrorEnvelope`, which is the Rust side of exactly this shape:
 * `{"code": "...", "message": "...", "id": "..."}`. Only a handful of commands are
 * converted to it -- the ones whose caller (this shim) plausibly acts differently per
 * variant, not every fallible command -- so this module has to keep working for a plain,
 * non-JSON string too: a command that has not been converted yet, or a backend built
 * before this existed, still rejects with bare prose.
 */

/** The parsed shape of a coded IPC error. */
export interface IpcErrorEnvelope {
  code: string;
  message: string;
  id: string;
}

/**
 * Parse whatever a failed `invoke()` rejected with into a coded envelope, or `null` when
 * it is not one.
 *
 * `null` covers three cases deliberately conflated, because a shim's `catch` block treats
 * them identically -- as "no code to branch on, fall back to generic handling": the
 * rejection was not a string at all (a `TypeError` from Tauri's own plumbing, say), it was
 * a string that is not JSON (an unconverted command's plain message), or it was JSON that
 * does not have the envelope's shape (a future, incompatible schema). A malformed or absent
 * envelope must never throw out of this function -- that would turn "this command wasn't
 * converted yet" into a crash in the caller's `catch` block, which is worse than the
 * un-coded error it was trying to enrich.
 */
export function parseIpcError(rejection: unknown): IpcErrorEnvelope | null {
  if (typeof rejection !== "string") return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(rejection);
  } catch {
    return null;
  }
  if (
    typeof parsed === "object" &&
    parsed !== null &&
    typeof (parsed as Record<string, unknown>).code === "string" &&
    typeof (parsed as Record<string, unknown>).message === "string" &&
    typeof (parsed as Record<string, unknown>).id === "string"
  ) {
    return parsed as IpcErrorEnvelope;
  }
  return null;
}

/**
 * The human-readable text to log or show for a failed `invoke()`, whether or not it parsed
 * as a coded envelope.
 */
export function ipcErrorMessage(rejection: unknown): string {
  const envelope = parseIpcError(rejection);
  if (envelope) return envelope.message;
  if (rejection instanceof Error) return rejection.message;
  return String(rejection);
}

/**
 * The one code-to-`DOMException`-name mapping evidenced in the shipped Element Call bundle
 * (`element-web-dist/widgets/element-call/assets/index-ZYqhOGev.js`) rather than guessed.
 *
 * `getDisplayMedia`'s own DOM contract makes `NotAllowedError` what a real browser throws
 * when the user declines the share prompt, and the bundle's webrtc-adapter shim carries the
 * same name in its own permission-error normalisation table -- both point at the same
 * exception for the same condition. Grepping the bundle for `setRemoteDescription`,
 * `createOffer`/`createAnswer`, `addIceCandidate` and data-channel `send()` found no place
 * that branches on `DOMException.name` for any of them (`PCTransport.setMungedSDP` logs and
 * rethrows generically; `dataChannelForKind(...).readyState === "open"` is polled before a
 * send is attempted, never caught after); `OperationError` does not appear in the bundle at
 * all. So only `picker_cancelled` is mapped here -- every other code this module produces
 * reaches the page as a plain `Error`, not a fabricated exception name nothing is known to
 * check.
 */
const CODE_TO_DOM_EXCEPTION: Readonly<Record<string, string>> = {
  picker_cancelled: "NotAllowedError",
};

/**
 * Build the rejection a shim should actually throw/reject with for a failed `invoke()`,
 * given the coded envelope this module can parse plus a caller-supplied fallback message
 * for the case where there was no envelope to parse (an unconverted command, or a plain
 * string from an older backend).
 */
export function ipcErrorToRejection(rejection: unknown, fallbackMessage: string): Error {
  const envelope = parseIpcError(rejection);
  if (!envelope) return new Error(fallbackMessage);
  const domName = CODE_TO_DOM_EXCEPTION[envelope.code];
  if (domName) return new DOMException(envelope.message, domName);
  return new Error(envelope.message);
}
