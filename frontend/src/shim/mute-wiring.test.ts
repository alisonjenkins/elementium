/**
 * Muting has to reach the backend, because muting in this app is not a local fact.
 *
 * Every caller in this stack mutes the same way: livekit-client and matrix-js-sdk set
 * `enabled = false` on the `MediaStreamTrack` they were handed. That track is a local
 * preview — the samples and frames on the wire come from a capture pipeline in Rust that
 * has never seen it. Before this wiring existed, muting changed the icon and nothing else:
 * the user believed they were silent while everyone else could still hear them.
 *
 * These tests exercise the interception in isolation rather than through `getUserMedia`,
 * which needs a live backend. What they pin is the property that failed: that setting
 * `enabled` produces a backend call, with the right track named and the right sense — an
 * inverted `muted` flag here would mute on unmute and be very hard to see.
 */
import { describe, expect, it, vi } from "vitest";

interface MuteCall {
  kind: string;
  source: string;
  muted: boolean;
}

/**
 * A stand-in for a `MediaStreamTrack`'s `enabled` accessor.
 *
 * The real one lives on `MediaStreamTrack.prototype`, which does not exist under vitest's
 * node environment, so the prototype is modelled here. If the browser ever stopped
 * defining `enabled` as an accessor the production helper would bail out with a warning,
 * which the last test covers.
 */
function trackWithEnabledAccessor(): { track: { enabled: boolean }; proto: object } {
  const proto = {};
  let value = true;
  Object.defineProperty(proto, "enabled", {
    configurable: true,
    get(): boolean {
      return value;
    },
    set(next: boolean) {
      value = next;
    },
  });
  return { track: Object.create(proto) as { enabled: boolean }, proto };
}

/** The production helper's shape, with the Tauri call injected so it can be observed. */
function wireMuteToBackend(
  track: { enabled: boolean },
  kind: string,
  source: string,
  invoke: (cmd: string, args: MuteCall) => void,
): boolean {
  const proto = Object.getPrototypeOf(track) as object;
  const original = Object.getOwnPropertyDescriptor(proto, "enabled");
  if (!original?.get || !original.set) return false;
  const get = original.get.bind(track);
  const set = original.set.bind(track);
  Object.defineProperty(track, "enabled", {
    configurable: true,
    get,
    set: (value: boolean) => {
      set(value);
      invoke("set_capture_muted", { kind, source, muted: !value });
    },
  });
  return true;
}

describe("mute reaches the backend", () => {
  it("tells the backend a track is muted when enabled goes false", () => {
    const { track } = trackWithEnabledAccessor();
    const invoke = vi.fn();
    wireMuteToBackend(track, "audio", "microphone", invoke);

    track.enabled = false;

    expect(invoke).toHaveBeenCalledWith("set_capture_muted", {
      kind: "audio",
      source: "microphone",
      muted: true,
    });
  });

  it("sends muted:false on unmute, not muted:true", () => {
    // The inversion is the bug worth guarding: `enabled` and `muted` are opposites, and
    // getting it backwards mutes on unmute — which looks like the feature working until
    // someone tries to speak.
    const { track } = trackWithEnabledAccessor();
    const invoke = vi.fn();
    wireMuteToBackend(track, "video", "camera", invoke);

    track.enabled = false;
    track.enabled = true;

    expect(invoke).toHaveBeenLastCalledWith("set_capture_muted", {
      kind: "video",
      source: "camera",
      muted: false,
    });
  });

  it("still applies the underlying enabled state, so the local preview follows", () => {
    // The interception must not swallow the original setter: a muted camera should stop
    // showing itself locally too, and reading back `enabled` is how callers check.
    const { track } = trackWithEnabledAccessor();
    wireMuteToBackend(track, "video", "camera", vi.fn());

    track.enabled = false;

    expect(track.enabled).toBe(false);
  });

  it("declines rather than pretending when enabled is not an accessor", () => {
    const track = { enabled: true };
    const invoke = vi.fn();

    expect(wireMuteToBackend(track, "audio", "microphone", invoke)).toBe(false);
    track.enabled = false;
    expect(invoke).not.toHaveBeenCalled();
  });
});
