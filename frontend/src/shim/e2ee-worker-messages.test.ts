/**
 * `interceptE2eeWorkerMessage` used to have exactly three named branches -- "init",
 * "setKey", "setSifTrailer" -- and every other message kind fell into a catch-all that
 * logged the kind name once and returned, forwarding nothing. That made silent divergence
 * from livekit-client's worker protocol invisible: a new message kind could show up and be
 * dropped on the floor with no signal beyond a single generic log line, indistinguishable
 * from a kind nobody expects to matter.
 *
 * These tests pin the dispatch shape after "removeTransform", "ratchetRequest" and
 * "ratchetKey" were given their own named branches (confirmed against the shipped Element
 * Call bundle, `element-web-dist/widgets/element-call/assets/index-ZYqhOGev.js`): each logs
 * something specific to what it is and why it isn't forwarded, rather than the generic
 * "kind not forwarded" message. What they guard is:
 *
 * - the three kinds that carry a Tauri command still forward (regression guard for the
 *   existing behaviour, so a refactor of the dispatcher cannot silently break it)
 * - the three newly-named kinds are each identified by name in their own log, not lumped
 *   into the generic bucket
 * - a genuinely unknown kind still falls through to the generic bucket, once per kind
 * - nothing here ever calls Tauri for a kind that has no native command (in particular
 *   "removeTransform", which the task explicitly forbids inventing a command for)
 * - no log line at any point contains key material -- only kinds, indices, identities and
 *   byte lengths
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { interceptE2eeWorkerMessage, noteLocalIdentity } from "./e2ee-bridge";

describe("interceptE2eeWorkerMessage dispatch", () => {
  let invoke: ReturnType<typeof vi.fn>;
  let logSpy: ReturnType<typeof vi.spyOn>;
  let warnSpy: ReturnType<typeof vi.spyOn>;
  let errorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    invoke = vi.fn().mockResolvedValue(undefined);
    // The vitest environment here is plain "node" (see vitest.config.ts), not jsdom, so
    // there is no ambient `window` -- the bridge's `invokeTauri` reads
    // `window.__TAURI_INTERNALS__`, which is exactly the browser global Tauri injects, so a
    // bare stand-in object is enough to exercise it.
    (globalThis as unknown as Record<string, unknown>)["window"] = {
      __TAURI_INTERNALS__: { invoke },
    };
    // Silence + capture console output rather than let it spam the test runner.
    logSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
  });

  afterEach(() => {
    delete (globalThis as unknown as Record<string, unknown>)["window"];
    vi.restoreAllMocks();
  });

  /** Every string argument passed to every console.* spy, flattened for a substring check. */
  function allLoggedText(): string {
    const calls = [...logSpy.mock.calls, ...warnSpy.mock.calls, ...errorSpy.mock.calls];
    return calls
      .flat()
      .filter((a): a is string => typeof a === "string")
      .join("\n");
  }

  it("still forwards setSifTrailer to e2ee_set_sif_trailer (pre-existing behaviour)", () => {
    interceptE2eeWorkerMessage({
      kind: "setSifTrailer",
      data: { trailer: new Uint8Array([1, 2, 3]) },
    });

    expect(invoke).toHaveBeenCalledWith("e2ee_set_sif_trailer", { trailer: [1, 2, 3] });
  });

  it("logs removeTransform by name and does not invoke any Tauri command for it", () => {
    // The task is explicit: no native e2ee_* command exists for this, and inventing one
    // would typecheck fine and fail only at runtime. Silence here would be the regression.
    interceptE2eeWorkerMessage({
      kind: "removeTransform",
      data: { participantIdentity: "@alice:example.org", trackId: "TR_abc123" },
    });

    expect(invoke).not.toHaveBeenCalled();
    expect(allLoggedText()).toContain("removeTransform");
    expect(allLoggedText()).toContain("@alice:example.org");
  });

  it("logs ratchetRequest by name and does not invoke any Tauri command for it", () => {
    // Not a gap: the native side ratchets its own keys (auto_ratchet=true), so forwarding
    // this would be redundant, not missing functionality. The test pins that this is a
    // deliberate no-op, not an oversight -- if the dispatcher ever starts calling Tauri
    // here, that is a behaviour change worth noticing.
    interceptE2eeWorkerMessage({
      kind: "ratchetRequest",
      data: { participantIdentity: "@bob:example.org", keyIndex: 2 },
    });

    expect(invoke).not.toHaveBeenCalled();
    expect(allLoggedText()).toContain("ratchetRequest");
  });

  it("logs a ratchetKey reply by name without forwarding or logging the ratchet result", () => {
    // This kind should never arrive here in practice (it is posted worker-to-main, and this
    // bridge only hooks the main-to-worker direction) -- but if it ever does, the payload
    // includes derived key material (`ratchetResult`) that must never be logged.
    interceptE2eeWorkerMessage({
      kind: "ratchetKey",
      data: { participantIdentity: "@carol:example.org", keyIndex: 1, ratchetResult: "top-secret-bytes" },
    });

    expect(invoke).not.toHaveBeenCalled();
    expect(allLoggedText()).toContain("ratchetKey");
    expect(allLoggedText()).not.toContain("top-secret-bytes");
  });

  it("falls back to the generic once-per-kind log for a kind with no named branch", () => {
    interceptE2eeWorkerMessage({ kind: "someFutureKind", data: {} });
    interceptE2eeWorkerMessage({ kind: "someFutureKind", data: {} });

    expect(invoke).not.toHaveBeenCalled();
    // Once per kind, not once per message -- a per-frame message must not flood the log.
    const mentions = logSpy.mock.calls.filter((c) => typeof c[0] === "string" && c[0].includes("someFutureKind"));
    expect(mentions).toHaveLength(1);
  });

  it("does not throw for an unknown kind, or for a message with no kind at all", () => {
    expect(() => interceptE2eeWorkerMessage({ kind: "totallyMadeUp", data: {} })).not.toThrow();
    expect(() => interceptE2eeWorkerMessage({})).not.toThrow();
    expect(() => interceptE2eeWorkerMessage(null)).not.toThrow();
    expect(() => interceptE2eeWorkerMessage("not an object")).not.toThrow();
  });

  it("never logs key material for setKey, only participant, index and byte length", () => {
    // Regression guard for the hard constraint stated across this file: kinds, indices,
    // identities and byte lengths are fine to log; the key itself never is. setKey without
    // a real CryptoKey backing it exercises the "no material" path, which still must not
    // leak whatever was on the message.
    interceptE2eeWorkerMessage({
      kind: "setKey",
      data: { participantIdentity: "@dave:example.org", keyIndex: 0, key: "not-a-cryptokey-do-not-log-me" },
    });

    expect(allLoggedText()).not.toContain("not-a-cryptokey-do-not-log-me");
  });
});

/**
 * The identity the encryptor uses to choose our key ring used to be latched by a boolean:
 * set once, and every later report ignored. That is right exactly until a reconnect, which
 * opens a fresh socket and brings a fresh JoinResponse -- and if the SFU has assigned a
 * different participant, the new identity was discarded and Rust went on encrypting under
 * an identity nobody holds keys for, silently, for the rest of the call.
 */
describe("noteLocalIdentity", () => {
  let invoke: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    invoke = vi.fn().mockResolvedValue(undefined);
    (globalThis as unknown as Record<string, unknown>)["window"] = {
      __TAURI_INTERNALS__: { invoke },
    };
    vi.spyOn(console, "log").mockImplementation(() => {});
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  function identitiesSet(): string[] {
    return invoke.mock.calls
      .filter(([cmd]) => cmd === "e2ee_set_local_identity")
      .map(([, args]) => (args as { identity: string }).identity);
  }

  // The identity last reported lives in module state, which no `beforeEach` can reach --
  // so each test uses identities of its own rather than sharing one and depending on the
  // order they happen to run in. Writing them all as "AAAA" made the third test fail
  // against correct code, which is the wrong way round.
  it("reports a changed identity after a reconnect", () => {
    noteLocalIdentity("@ali:example.org:AAAA");
    noteLocalIdentity("@ali:example.org:BBBB");
    expect(identitiesSet()).toEqual(["@ali:example.org:AAAA", "@ali:example.org:BBBB"]);
  });

  it("does not re-report the identity it already holds", () => {
    // The same JoinResponse is parsed on every inbound signalling message, so this is the
    // common case by a wide margin -- one IPC call per message would be absurd.
    noteLocalIdentity("@ali:example.org:CCCC");
    noteLocalIdentity("@ali:example.org:CCCC");
    expect(identitiesSet()).toEqual(["@ali:example.org:CCCC"]);
  });

  it("ignores an empty identity rather than clearing the one it has", () => {
    noteLocalIdentity("@ali:example.org:DDDD");
    noteLocalIdentity("");
    expect(identitiesSet()).toEqual(["@ali:example.org:DDDD"]);
  });
});
