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

/**
 * Keys this bridge tried to hand to Rust and could not.
 *
 * Counted rather than only logged, because of what a failure means: the native side
 * encrypts with a key the far end does not have, and every participant hears noise. That
 * has happened before in this project, and the symptom — "everyone sounds like a modem" —
 * looks nothing like an IPC error, so a `console.warn` in a webview nobody is watching is
 * indistinguishable from silence.
 */
let keyIpcFailures = 0;

/** Tauri command invoker, tolerant of the command being unavailable. */
function invokeTauri(cmd: string, args: Record<string, unknown>): void {
  const failed = (why: string, e: unknown) => {
    keyIpcFailures += 1;
    // At error, and stated in terms of the consequence rather than the mechanism. Whoever
    // reads this is looking at a call that sounds broken, not at an IPC log.
    console.error(
      `[Elementium] E2EE IPC ${cmd} ${why} (failure #${keyIpcFailures}): the native backend ` +
        `does not have this key, so what it sends cannot be decrypted by anyone in the call.`,
      e,
    );
  };
  try {
    const internals = (window as unknown as Record<string, unknown>)["__TAURI_INTERNALS__"] as
      | { invoke?: (cmd: string, args: unknown) => Promise<unknown> }
      | undefined;
    internals?.invoke?.(cmd, args)?.catch((e: unknown) => failed("was rejected", e));
  } catch (e) {
    failed("was unavailable", e);
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
        const call = isCallKeyImport(algorithm) ? noteKeyImported() : null;
        result
          .then((key) => {
            rawKeyMaterial.set(key, copy);
            if (call !== null) importSerials.set(key, call);
          })
          .catch(() => {
            /* import failed; nothing to record */
            if (call !== null) resolveImport(call);
          });
      }
    }
    return result;
  };

  (subtle as unknown as Record<string, unknown>)["importKey"] = patched;
}

/**
 * Whether an `importKey` call is Element Call deriving a frame-encryption key.
 *
 * Element Call imports its call keys as HKDF material
 * (`importKey("raw", key, "HKDF", false, ["deriveBits", "deriveKey"])`). Element Web imports
 * plenty of other raw keys for Matrix's own crypto, and counting those would make the
 * arithmetic below meaningless.
 */
function isCallKeyImport(algorithm: unknown): boolean {
  if (algorithm === "HKDF") return true;
  return (
    typeof algorithm === "object" &&
    algorithm !== null &&
    (algorithm as { name?: unknown }).name === "HKDF"
  );
}

/**
 * Call keys derived but not yet seen leaving for the worker, by serial.
 *
 * The question this answers was raised by a real call and never settled: inbound frames
 * carried key indices 3 and 6 while every key this bridge saw was at index 0, 1 or 4, and no
 * key ever failed to be recovered. Either livekit never told the worker about those keys, or
 * it did so by a route this ignores -- and the two have different owners.
 *
 * Counting derivations against forwards separates them. A key that Element Call derives and
 * never sends is upstream of us; a key that is sent and not forwarded is ours.
 *
 * Holds no `CryptoKey` and no material -- only a serial and a timestamp -- so nothing here
 * keeps key material alive or puts it anywhere it could be logged.
 */
const pendingImports = new Map<number, { at: number }>();
/** Serial for each imported call key, so `setKey` can resolve the import it came from. */
const importSerials = new WeakMap<CryptoKey, number>();
let importSerial = 0;
let keysImported = 0;

/**
 * How long a derived key may go unforwarded before it is reported.
 *
 * Element Call delays *using* a new key by `useKeyDelay` (5s by default) after distributing
 * it, so a key sits between derivation and `setKey` for several seconds as a matter of
 * course. This has to be comfortably longer than that or every rotation reports itself.
 */
const UNFORWARDED_KEY_MS = 15_000;

function noteKeyImported(): number {
  importSerial += 1;
  keysImported += 1;
  const serial = importSerial;
  pendingImports.set(serial, { at: Date.now() });
  setTimeout(() => {
    const entry = pendingImports.get(serial);
    if (!entry) return;
    pendingImports.delete(serial);
    console.warn(
      `[Elementium] E2EE call key #${serial} was derived ${Date.now() - entry.at}ms ago and ` +
        `never reached the worker, so the native backend does not have it. ` +
        `derived=${keysImported} forwarded=${keysForwarded} ` +
        `still_waiting=${pendingImports.size} ipc_failures=${keyIpcFailures}. ` +
        `A non-zero ipc_failures means the bridge tried and the IPC refused, which is ours; ` +
        `zero means livekit was never asked to install it, which is upstream of the bridge.`,
    );
  }, UNFORWARDED_KEY_MS);
  return serial;
}

/** Mark a derived key as accounted for, so its deadline passes without a report. */
function resolveImport(serial: number): void {
  pendingImports.delete(serial);
}

interface SetKeyMessage {
  participantIdentity?: unknown;
  isPublisher?: unknown;
  key?: unknown;
  keyIndex?: unknown;
}

/**
 * Tell the native encryptor which participant we are, once.
 *
 * Without it the encryptor cannot choose our key and refuses every outbound frame: a real
 * call discarded 44,743 consecutive audio frames this way, with capture, fold, gain and
 * encoding all working and the outbound stats counting them as sent, because the drop
 * happens further down at the peer-connection boundary.
 *
 * Exported because the reliable source is the SFU's join response, which arrives on the
 * signalling socket rather than through any key message.
 */
export function noteLocalIdentity(identity: string): void {
  if (localIdentitySent || !identity) return;
  localIdentitySent = true;
  // The native context has to exist before it can be told anything.
  if (!initSent) handleInit({});
  console.log(`[Elementium] E2EE local identity set from the SFU: ${identity}`);
  invokeTauri("e2ee_set_local_identity", { identity });
}

/** Whether `e2ee_init` has been sent; the worker is initialized once per room. */
let initSent = false;
/** Whether the local participant identity has been reported to Rust. */
let localIdentitySent = false;

/** How many `setKey` messages this bridge has forwarded, for the watchdog below. */
let keysForwarded = 0;

/**
 * Every key index forwarded so far.
 *
 * Reported alongside each key because the interesting comparison is against the index the
 * *sender* stamps on its frames, which Rust logs when a frame cannot be decrypted. A key
 * set is only complete relative to what is arriving, and neither side can tell alone.
 */
const keyIndexesSeen = new Set<number>();

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
    console.warn(
      `[Elementium] E2EE setKey for "${participant}" index ${keyIndex} carried no CryptoKey; ` +
        `not forwarded, so the native backend has no key at that index`,
    );
    return;
  }

  const serial = importSerials.get(key);
  if (serial !== undefined) resolveImport(serial);

  const material = rawKeyMaterial.get(key);
  if (!material) {
    // The key was imported before this bridge was installed, or by a path that did not
    // go through `importKey("raw", ...)`. Loud, because the consequence is silent:
    // Rust keeps no key for this participant and drops all their media.
    console.error(
      `[Elementium] E2EE key for "${participant}" (index ${keyIndex}) has no recorded raw ` +
        `material — native decryption for this participant will fail. ` +
        `derived=${keysImported} forwarded=${keysForwarded}. This one is ours: the key ` +
        `reached the worker but not through an importKey("raw", ...) this bridge front-ran`,
    );
    return;
  }

  // livekit only sends `init` once per manager; if a key arrives first, make sure the
  // native context exists before the key lands on it.
  if (!initSent) handleInit({});

  // Kept as a fallback, and no longer the only route: `isPublisher` is not set on the
  // messages this bridge actually sees, so on its own it never fired. The SFU's join
  // response is what normally supplies this now -- see `noteLocalIdentity`.
  if (data.isPublisher === true && participant) {
    noteLocalIdentity(participant);
  }

  keysForwarded += 1;
  keyIndexesSeen.add(keyIndex);
  console.log(
    `[Elementium] E2EE key forwarded to native backend: participant="${participant}" ` +
      `index=${keyIndex} len=${material.length} ` +
      `indexes_seen=[${[...keyIndexesSeen].sort((a, b) => a - b).join(",")}] ` +
      // derived > forwarded means Element Call made a key the worker was never told about,
      // which is the difference between a key we lost and one we were never given.
      `derived=${keysImported} forwarded=${keysForwarded}`,
  );
  invokeTauri("e2ee_set_key", {
    participant,
    keyIndex,
    keyMaterial: Array.from(material),
  });
}

/**
 * Message kinds seen but not acted on, reported once each.
 *
 * Only `init` and `setKey` are handled. Whether that is sufficient is not something this
 * file can assert on its own -- and a real call showed why it matters: inbound frames
 * carried key indices 3 and 6 while every key this bridge ever saw was at index 0, 1 or 4,
 * with no key ever failing to be recovered. Either livekit never told the worker about
 * those keys, or it told it by a route this ignores.
 *
 * Kinds only. Never payloads: a `setKey` payload holds key material, and one careless log
 * line puts a call's encryption key on disk.
 */
const unhandledKinds = new Set<string>();

/**
 * Forward the SFU's "server injected frame" marker.
 *
 * An SFU inserts its own frames into a stream -- silence or a black picture while a
 * publisher is muted. It holds no key, so those cannot be encrypted; they arrive in the
 * clear with this trailer appended to identify them, and livekit's own cryptor passes them
 * through untouched rather than trying to decrypt them.
 *
 * Without this the native side feeds every one to AES-GCM, where it fails to authenticate
 * and is dropped -- indistinguishable in a log from a missing key, and arriving exactly
 * when someone mutes.
 */
function handleSifTrailer(data: Record<string, unknown>): void {
  const trailer = toBytes(data["trailer"] ?? data["sifTrailer"]);
  if (!trailer) {
    console.warn("[Elementium] E2EE setSifTrailer without recognisable bytes; ignoring");
    return;
  }
  console.log(`[Elementium] E2EE server-injected-frame trailer forwarded (${trailer.length} bytes)`);
  invokeTauri("e2ee_set_sif_trailer", { trailer: Array.from(trailer) });
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
  } else if (kind === "setSifTrailer") {
    handleSifTrailer(data);
  } else if (typeof kind === "string" && !unhandledKinds.has(kind)) {
    // Once per kind, so a per-frame message cannot flood the log.
    unhandledKinds.add(kind);
    console.log(
      `[Elementium] E2EE worker message kind "${kind}" is not forwarded to the native ` +
        `backend. Harmless unless it carries key material.`,
    );
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
