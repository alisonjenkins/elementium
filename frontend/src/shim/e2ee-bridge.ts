/**
 * E2EE key bridge: forwards Element Call's frame-encryption keys to the Rust backend.
 *
 * Element Call encrypts call media with keys distributed over Matrix to-device messages.
 * In a browser those keys are consumed by livekit-client's E2EE Web Worker, which
 * encrypts/decrypts each frame via insertable streams. Elementium replaces the whole
 * WebRTC stack with a native (str0m) implementation, so those browser-side transforms
 * never touch our media — the encryption has to happen in Rust instead, which means Rust
 * needs the keys.
 *
 * Without this bridge the Rust side has no key, and `maybe_decrypt_event` passes inbound
 * frames through untouched. That hands still-encrypted bytes to the Opus decoder, which
 * renders ciphertext as *noise* rather than failing — the cause of a long-running
 * "digital screeching" bug.
 *
 * ## Where the keys are intercepted, and why here
 *
 * livekit-client's `E2eeManager` posts messages to its worker (`e2ee/E2eeManager.ts`):
 *
 * - `{kind: "init", data: {keyProviderOptions}}` when E2EE is set up
 * - `{kind: "setKey", data: {participantIdentity, isPublisher, key, keyIndex}}` per key
 *
 * `data.key` is a `CryptoKey` imported with `extractable: false` (see Element Call's
 * `MatrixKeyProvider`), so the raw bytes cannot be read back out of it. They *are*
 * available one step earlier, at the `crypto.subtle.importKey("raw", ...)` call that
 * creates it — so this module records material there and correlates it to the `CryptoKey`
 * when the `setKey` message goes past.
 *
 * Hooking `Worker.prototype.postMessage` (rather than the `RTCRtpScriptTransform`
 * constructor, as an earlier attempt did) is what makes this reliable: the `init` and
 * `setKey` messages are posted directly to the worker during setup, independently of
 * whether any stream transform is ever constructed.
 */

/**
 * Raw key material, keyed by the non-extractable `CryptoKey` derived from it.
 *
 * A `WeakMap` so entries disappear with their `CryptoKey` — key material is never held
 * alive by this bridge beyond the lifetime of the key it belongs to.
 */
const rawKeyMaterial = new WeakMap<CryptoKey, Uint8Array>();

/** Tauri command invoker, tolerant of the command being unavailable. */
function invokeTauri(cmd: string, args: Record<string, unknown>): void {
  try {
    const internals = (window as unknown as Record<string, unknown>)["__TAURI_INTERNALS__"] as
      | { invoke?: (cmd: string, args: unknown) => Promise<unknown> }
      | undefined;
    internals?.invoke?.(cmd, args)?.catch((e: unknown) => {
      console.warn(`[Elementium] E2EE IPC ${cmd} rejected:`, e);
    });
  } catch (e) {
    console.warn(`[Elementium] E2EE IPC ${cmd} unavailable:`, e);
  }
}

function toBytes(source: unknown): Uint8Array | null {
  if (source instanceof Uint8Array) return source;
  if (source instanceof ArrayBuffer) return new Uint8Array(source);
  if (ArrayBuffer.isView(source)) {
    const view = source as ArrayBufferView;
    return new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
  }
  return null;
}

/**
 * Record the raw bytes behind every `crypto.subtle.importKey("raw", ...)` call.
 *
 * This is the only point at which Element Call's key material is visible as bytes; by the
 * time it reaches the worker it is a non-extractable `CryptoKey`.
 */
function hookImportKey(): void {
  const subtle = globalThis.crypto?.subtle;
  if (!subtle?.importKey) return;

  const original = subtle.importKey.bind(subtle);
  const patched = function importKey(
    this: SubtleCrypto,
    format: string,
    keyData: unknown,
    algorithm: unknown,
    extractable: boolean,
    keyUsages: unknown,
  ): Promise<CryptoKey> {
    const result = (
      original as unknown as (...a: unknown[]) => Promise<CryptoKey>
    )(format, keyData, algorithm, extractable, keyUsages);

    if (format === "raw") {
      const bytes = toBytes(keyData);
      if (bytes) {
        // Copy: the caller's buffer may be reused or detached after this returns.
        const copy = new Uint8Array(bytes);
        result
          .then((key) => {
            rawKeyMaterial.set(key, copy);
          })
          .catch(() => {
            /* import failed; nothing to record */
          });
      }
    }
    return result;
  };

  (subtle as unknown as Record<string, unknown>)["importKey"] = patched;
}

interface SetKeyMessage {
  participantIdentity?: unknown;
  isPublisher?: unknown;
  key?: unknown;
  keyIndex?: unknown;
}

/** Whether `e2ee_init` has been sent; the worker is initialized once per room. */
let initSent = false;
/** Whether the local participant identity has been reported to Rust. */
let localIdentitySent = false;

/** How many `setKey` messages this bridge has forwarded, for the watchdog below. */
let keysForwarded = 0;

/**
 * How long after E2EE setup to wait before declaring that no key ever arrived.
 *
 * Generous: livekit's key rollout is gated on a Matrix membership update plus a
 * to-device round trip, so a few seconds is normal.
 */
const KEY_WATCHDOG_MS = 15_000;

/**
 * Complain loudly if E2EE is set up but no key ever shows up.
 *
 * Without this the two failure modes are indistinguishable in a log: the bridge being
 * broken, and Element Call never generating a key at all (which happens when
 * `RTCEncryptionManager.onMembershipsUpdate` never fires after join, e.g. because our own
 * membership event was already present so the membership list never "changed"). Both look
 * identical from Rust -- inbound frames dropped for want of a key -- but only one of them
 * is ours to fix.
 */
function startKeyWatchdog(): void {
  setTimeout(() => {
    if (keysForwarded > 0) return;
    console.error(
      `[Elementium] E2EE was initialized ${KEY_WATCHDOG_MS}ms ago but no encryption key ` +
        `has been produced. The native backend has no key, so ALL inbound media is being ` +
        `dropped. This is upstream of the bridge: Element Call's RTCEncryptionManager ` +
        `never rolled out a key (it only does so on a membership update after join).`,
    );
  }, KEY_WATCHDOG_MS);
}

function handleInit(data: Record<string, unknown>): void {
  const keyProviderOptions = data["keyProviderOptions"] ?? null;
  console.log("[Elementium] E2EE init intercepted", keyProviderOptions);
  initSent = true;
  invokeTauri("e2ee_init", { options: keyProviderOptions });
  startKeyWatchdog();
}

function handleSetKey(data: SetKeyMessage): void {
  const participant = typeof data.participantIdentity === "string" ? data.participantIdentity : "";
  const keyIndex = typeof data.keyIndex === "number" ? data.keyIndex : 0;
  const key = data.key;

  if (!(key instanceof CryptoKey)) {
    console.warn("[Elementium] E2EE setKey without a CryptoKey; cannot forward to native backend");
    return;
  }

  const material = rawKeyMaterial.get(key);
  if (!material) {
    // The key was imported before this bridge was installed, or by a path that did not
    // go through `importKey("raw", ...)`. Loud, because the consequence is silent:
    // Rust keeps no key for this participant and drops all their media.
    console.error(
      `[Elementium] E2EE key for "${participant}" (index ${keyIndex}) has no recorded raw ` +
        `material — native decryption for this participant will fail`,
    );
    return;
  }

  // livekit only sends `init` once per manager; if a key arrives first, make sure the
  // native context exists before the key lands on it.
  if (!initSent) handleInit({});

  if (data.isPublisher === true && !localIdentitySent && participant) {
    localIdentitySent = true;
    invokeTauri("e2ee_set_local_identity", { identity: participant });
  }

  keysForwarded += 1;
  console.log(
    `[Elementium] E2EE key forwarded to native backend: participant="${participant}" ` +
      `index=${keyIndex} len=${material.length}`,
  );
  invokeTauri("e2ee_set_key", {
    participant,
    keyIndex,
    keyMaterial: Array.from(material),
  });
}

/** Inspect one message bound for a Web Worker, forwarding E2EE keys if it is one. */
export function interceptE2eeWorkerMessage(message: unknown): void {
  if (!message || typeof message !== "object") return;
  const msg = message as Record<string, unknown>;
  const kind = msg["kind"];
  // livekit nests the payload under `data`; tolerate a flat shape too.
  const data = (
    msg["data"] && typeof msg["data"] === "object" ? msg["data"] : msg
  ) as Record<string, unknown>;

  if (kind === "init") {
    handleInit(data);
  } else if (kind === "setKey") {
    handleSetKey(data as SetKeyMessage);
  }
}

/**
 * Wrap `Worker.prototype.postMessage` so every message posted to any worker is inspected.
 *
 * Broad by design: livekit constructs its E2EE worker internally, so there is no handle to
 * hook more narrowly. Non-E2EE messages are ignored cheaply by shape, and the original
 * `postMessage` always runs regardless of what this code does.
 */
function hookWorkerPostMessage(): void {
  if (typeof Worker === "undefined") return;

  const original = Worker.prototype.postMessage;
  Worker.prototype.postMessage = function patchedPostMessage(
    this: Worker,
    message: unknown,
    transferOrOptions?: unknown,
  ): void {
    try {
      interceptE2eeWorkerMessage(message);
    } catch (e) {
      console.warn("[Elementium] E2EE intercept error (non-fatal):", e);
    }
    (original as unknown as (this: Worker, m: unknown, t?: unknown) => void).call(
      this,
      message,
      transferOrOptions,
    );
  } as typeof Worker.prototype.postMessage;
}

/** Install the E2EE key bridge. Safe to call once, before Element Call initializes. */
export function setupE2eeBridge(): void {
  hookImportKey();
  hookWorkerPostMessage();
  console.log("[Elementium] E2EE key bridge installed");
}
