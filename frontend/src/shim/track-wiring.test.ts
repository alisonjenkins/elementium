/**
 * Three regressions in the per-track wiring `getUserMedia`/`getDisplayMedia` install on the
 * synthetic (canvas/AudioContext-backed) tracks this shim hands to matrix-js-sdk and
 * livekit-client.
 *
 * FIX 1 -- `.clone()` (matrix-js-sdk's `CallFeed.clone()` calls it, and ships in the bundle)
 * returned a bare copy with none of the wiring: no native id, no backend-reaching `stop()`,
 * no backend-reaching mute. Worse, if `clone()` were wired naively, stopping either handle
 * would tear down the one shared native pipeline out from under the other. What is pinned
 * here is reference counting by native track id: the pipeline is only released when the
 * last live handle stops, and a clone really is a second live handle (it can still mute the
 * backend after the original stops).
 *
 * FIX 2 -- `getSettings().deviceId` reported the synthetic canvas/AudioContext id, not the
 * camera or microphone Rust actually opened. livekit-client checks
 * `track.getSettings().deviceId === <requested id>` to confirm a device switch worked; the
 * synthetic id made that check read false even on a successful switch. Pinned here: the
 * overridden `getSettings()` reports the native id while leaving every other field alone,
 * and an unknown native id is not papered over with an invented one.
 *
 * FIX 3 -- `applyConstraints()` resolved successfully while changing nothing about real
 * capture (no Tauri command exists to reconfigure a running pipeline -- confirmed by
 * `grep -n "tauri::command" src-tauri/src/commands/media_devices.rs`), and
 * `getCapabilities()` reported the canvas/AudioContext stand-in's ranges, not the camera's.
 * Pinned here: both still resolve/return (rejecting would break best-effort callers), but
 * both log a clear warning naming what was requested and why it cannot reach native capture.
 *
 * The vitest environment here is "node", not "jsdom" -- there is no ambient `window`,
 * `document`, or `MediaStreamTrack` -- so this file builds minimal stand-ins, the same way
 * mute-wiring.test.ts does, and reimplements the production helpers' shape with the Tauri
 * `invoke` call injected so it can be observed without a live backend.
 */
import { describe, expect, it, vi } from "vitest";

type Invoke = (cmd: string, args?: Record<string, unknown>) => void;

/** Minimal stand-in for a `MediaStreamTrack`, with `enabled` on a shared prototype like the
 * real one (see mute-wiring.test.ts for why: `enabled` must be found via the prototype
 * chain, not as an own property). */
function makeFakeTrackProto(): object {
  const proto = {};
  Object.defineProperty(proto, "enabled", {
    configurable: true,
    get(this: { __enabled: boolean }) {
      return this.__enabled;
    },
    set(this: { __enabled: boolean }, v: boolean) {
      this.__enabled = v;
    },
  });
  return proto;
}

interface FakeTrack {
  __enabled: boolean;
  enabled: boolean;
  stop: () => void;
  clone: () => FakeTrack;
  getSettings: () => Record<string, unknown>;
  applyConstraints: (c?: Record<string, unknown>) => Promise<void>;
  getCapabilities: () => Record<string, unknown>;
  stopped: boolean;
}

function makeFakeTrack(nativeSettings: Record<string, unknown> = {}): FakeTrack {
  const proto = makeFakeTrackProto();
  const track = Object.create(proto) as FakeTrack;
  track.__enabled = true;
  track.stopped = false;
  track.stop = () => {
    track.stopped = true;
  };
  track.clone = () => makeFakeTrack(nativeSettings);
  track.getSettings = () => ({ ...nativeSettings });
  track.applyConstraints = async () => {};
  track.getCapabilities = () => ({ width: { max: 640 } });
  return track;
}

// --- Production helpers, reimplemented with `invoke` injected. Same shape as
// wireStopToBackend / wireCloneToBackend / wireNativeDeviceId / wireConstraintDishonesty /
// wireNativeTrack in media-devices.ts. ---

const nativeTrackRefCounts = new Map<string, number>();

function wireStopToBackend(
  track: FakeTrack,
  nativeTrackId: string,
  invoke: Invoke,
  onReleased: () => void,
): void {
  nativeTrackRefCounts.set(nativeTrackId, (nativeTrackRefCounts.get(nativeTrackId) ?? 0) + 1);
  const originalStop = track.stop.bind(track);
  track.stop = () => {
    originalStop();
    const remaining = (nativeTrackRefCounts.get(nativeTrackId) ?? 1) - 1;
    if (remaining > 0) {
      nativeTrackRefCounts.set(nativeTrackId, remaining);
      return;
    }
    nativeTrackRefCounts.delete(nativeTrackId);
    onReleased();
    invoke("stop_track", { trackId: nativeTrackId });
  };
}

function wireCloneToBackend(track: FakeTrack, nativeTrackId: string, invoke: Invoke, onReleased: () => void): void {
  const originalClone = track.clone.bind(track);
  track.clone = (): FakeTrack => {
    const cloned = originalClone();
    wireNativeTrack(cloned, nativeTrackId, invoke, onReleased);
    return cloned;
  };
}

function wireNativeDeviceId(track: FakeTrack, deviceId: string | undefined): void {
  if (!deviceId) return;
  const originalGetSettings = track.getSettings.bind(track);
  track.getSettings = () => ({ ...originalGetSettings(), deviceId });
}

function wireConstraintDishonesty(track: FakeTrack, kind: "audio" | "video", warn: (msg: string) => void): void {
  const originalApply = track.applyConstraints.bind(track);
  track.applyConstraints = async (constraints?: Record<string, unknown>): Promise<void> => {
    const keys = constraints ? Object.keys(constraints) : [];
    warn(`applyConstraints(${kind}) cannot reach native capture: [${keys.join(", ")}]`);
    await originalApply(constraints).catch(() => {});
  };

  const originalGetCapabilities = track.getCapabilities.bind(track);
  track.getCapabilities = () => {
    warn(`getCapabilities(${kind}) reports the local stand-in, not the real device`);
    return originalGetCapabilities();
  };
}

function wireNativeTrack(
  track: FakeTrack,
  nativeTrackId: string,
  invoke: Invoke,
  onReleased: () => void,
  opts: { deviceId?: string; kind?: "audio" | "video"; warn?: (msg: string) => void } = {},
): void {
  wireStopToBackend(track, nativeTrackId, invoke, onReleased);
  wireCloneToBackend(track, nativeTrackId, invoke, onReleased);
  wireNativeDeviceId(track, opts.deviceId);
  wireConstraintDishonesty(track, opts.kind ?? "video", opts.warn ?? (() => {}));
}

describe("FIX 1: clone() carries native wiring, refcounted so one stop can't kill the other", () => {
  it("stopping the original while a clone is still live does not release the native pipeline", () => {
    nativeTrackRefCounts.clear();
    const invoke = vi.fn();
    const onReleased = vi.fn();
    const original = makeFakeTrack();
    wireNativeTrack(original, "video-abc", invoke, onReleased);

    const clone = original.clone();
    original.stop();

    // The local object stopped...
    expect(original.stopped).toBe(true);
    // ...but the shared native pipeline must not have been torn down: `clone` is still a
    // live handle on it. A naive clone() would let this invoke fire early and blind the
    // other handle's caller.
    expect(invoke).not.toHaveBeenCalled();
    expect(onReleased).not.toHaveBeenCalled();
    void clone;
  });

  it("stopping the last live handle does release the pipeline exactly once", () => {
    nativeTrackRefCounts.clear();
    const invoke = vi.fn();
    const onReleased = vi.fn();
    const original = makeFakeTrack();
    wireNativeTrack(original, "video-abc", invoke, onReleased);
    const clone = original.clone();

    original.stop();
    clone.stop();

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("stop_track", { trackId: "video-abc" });
    expect(onReleased).toHaveBeenCalledTimes(1);
  });

  it("a clone of a clone still shares the same native id and refcount", () => {
    nativeTrackRefCounts.clear();
    const invoke = vi.fn();
    const onReleased = vi.fn();
    const original = makeFakeTrack();
    wireNativeTrack(original, "video-xyz", invoke, onReleased);
    const clone1 = original.clone();
    const clone2 = clone1.clone();

    original.stop();
    clone1.stop();
    // Two of three handles stopped; the pipeline must still be alive.
    expect(invoke).not.toHaveBeenCalled();

    clone2.stop();
    expect(invoke).toHaveBeenCalledTimes(1);
  });
});

describe("FIX 2: getSettings() reports the native device id, not the synthetic one", () => {
  it("overrides deviceId with the id Rust actually opened, keeping other fields", () => {
    const track = makeFakeTrack({ deviceId: "canvas-synthetic-id", frameRate: 30 });
    wireNativeDeviceId(track, "native-cam-42");

    const settings = track.getSettings();

    // This is the exact check livekit-client makes after a device switch; if this reads
    // the synthetic id, a real switch looks like a failed one.
    expect(settings.deviceId).toBe("native-cam-42");
    expect(settings.frameRate).toBe(30);
  });

  it("does not invent a device id when none was resolved", () => {
    const track = makeFakeTrack({ deviceId: "canvas-synthetic-id" });
    wireNativeDeviceId(track, undefined);

    // No requested/resolved deviceId is known (e.g. default device) -- leave the
    // underlying report alone rather than fabricate a native-looking id.
    expect(track.getSettings().deviceId).toBe("canvas-synthetic-id");
  });
});

describe("FIX 3: applyConstraints/getCapabilities make the gap to native capture visible", () => {
  it("still resolves applyConstraints, but warns naming the requested keys", async () => {
    const track = makeFakeTrack();
    const warn = vi.fn();
    wireConstraintDishonesty(track, "video", warn);

    // Must not reject: callers (livekit-client included) treat this as best-effort and do
    // not expect a throw.
    await expect(track.applyConstraints({ frameRate: 60, deviceId: "x" })).resolves.toBeUndefined();
    expect(warn).toHaveBeenCalledWith(
      expect.stringContaining("applyConstraints(video) cannot reach native capture: [frameRate, deviceId]"),
    );
  });

  it("getCapabilities still returns a value, but warns that it is the stand-in's, not the camera's", () => {
    const track = makeFakeTrack();
    const warn = vi.fn();
    wireConstraintDishonesty(track, "audio", warn);

    const caps = track.getCapabilities();

    expect(caps).toEqual({ width: { max: 640 } });
    expect(warn).toHaveBeenCalledWith(expect.stringContaining("getCapabilities(audio) reports the local stand-in"));
  });
});
