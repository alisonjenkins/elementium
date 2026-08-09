import { afterEach, describe, expect, it } from "vitest";

import { buildTrackEvent, type TrackEventParts } from "./webrtc-shim";

/**
 * The bug these pin cost every remote participant, in every call, for the life of the
 * feature: `new RTCTrackEvent(...)` throws in this webview -- the constructor belongs to the
 * WebRTC API the shim replaces, so it is not exposed -- and the `catch` around it only wrote
 * a log line. `dispatchEvent` and `ontrack` were never reached, so livekit was never told a
 * remote track existed and nothing rendered, while Rust decoded the video perfectly and
 * served it at 25fps to a canvas nobody had been told to display.
 *
 * The log line read "Track event dispatch: video mid=Ccu", which is why several passes over
 * these logs read it as a success.
 *
 * So the environment without a working `RTCTrackEvent` is not an edge case to tolerate; it
 * is the only environment this has ever run in, and it is the first test here.
 */
describe("buildTrackEvent", () => {
  const original = (globalThis as Record<string, unknown>)["RTCTrackEvent"];

  afterEach(() => {
    if (original === undefined) {
      delete (globalThis as Record<string, unknown>)["RTCTrackEvent"];
    } else {
      (globalThis as Record<string, unknown>)["RTCTrackEvent"] = original;
    }
  });

  /** Stand-ins: nothing here inspects these beyond identity. */
  function parts(): TrackEventParts {
    return {
      track: { id: "the-track" } as unknown as MediaStreamTrack,
      streams: [{ id: "the-stream" } as unknown as MediaStream],
      receiver: { id: "the-receiver" } as unknown as RTCRtpReceiver,
      transceiver: { mid: "Ccu" } as unknown as RTCRtpTransceiver,
    };
  }

  function expectCarriesParts(event: Event, p: TrackEventParts): void {
    const got = event as unknown as TrackEventParts;
    expect(event.type).toBe("track");
    expect(got.track).toBe(p.track);
    expect(got.streams).toBe(p.streams);
    expect(got.receiver).toBe(p.receiver);
    expect(got.transceiver).toBe(p.transceiver);
  }

  it("still produces a usable event when RTCTrackEvent does not exist", () => {
    delete (globalThis as Record<string, unknown>)["RTCTrackEvent"];
    const p = parts();
    expectCarriesParts(buildTrackEvent(p), p);
  });

  it("still produces a usable event when RTCTrackEvent exists but throws", () => {
    (globalThis as Record<string, unknown>)["RTCTrackEvent"] = function () {
      throw new TypeError("Illegal constructor");
    };
    const p = parts();
    expectCarriesParts(buildTrackEvent(p), p);
  });

  it("uses the real constructor where there is one", () => {
    class FakeTrackEvent extends Event {
      constructor(
        type: string,
        readonly init: TrackEventParts,
      ) {
        super(type);
      }
    }
    (globalThis as Record<string, unknown>)["RTCTrackEvent"] = FakeTrackEvent;
    const p = parts();
    const event = buildTrackEvent(p);
    expect(event).toBeInstanceOf(FakeTrackEvent);
    expect((event as FakeTrackEvent).init).toBe(p);
  });

  it("never throws, whatever RTCTrackEvent turns out to be", () => {
    for (const value of [null, 42, "nonsense", {}, []]) {
      (globalThis as Record<string, unknown>)["RTCTrackEvent"] = value;
      expect(() => buildTrackEvent(parts()), String(value)).not.toThrow();
    }
  });

  /**
   * The event has to survive a real `dispatchEvent`, which is where a plain object would
   * fail -- `EventTarget` rejects anything that is not an `Event`.
   */
  it("is dispatchable on an EventTarget and reaches a listener intact", () => {
    delete (globalThis as Record<string, unknown>)["RTCTrackEvent"];
    const p = parts();
    const target = new EventTarget();
    let seen: Event | null = null;
    target.addEventListener("track", (e) => {
      seen = e;
    });

    target.dispatchEvent(buildTrackEvent(p));

    expect(seen).not.toBeNull();
    expectCarriesParts(seen as unknown as Event, p);
  });
});
