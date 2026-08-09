/**
 * Remote tracks the SFU pushes at us used to have no transceiver and no receiver: `getReceivers()`
 * was hardcoded to `[]`, nothing was added to the transceiver list when a remote track arrived,
 * and the `RTCTrackEvent` carried `receiver`/`transceiver` as bare `{}` objects disconnected
 * from anything. livekit-client looks tracks up by track id and by mid through exactly these
 * (`getTransceiverByTrackId`, `getRemoteTrackIdByMid`, `receiver.track`, `receiver.getStats()`),
 * so those lookups silently found nothing and incoming media was never wired up.
 *
 * Also covers `currentDirection`: permanently `null` before, which meant livekit-client's
 * `getLocalTracks()` (which filters on `currentDirection` being `sendonly`/`sendrecv`) never
 * saw a published track no matter what actually got negotiated.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invoked: { cmd: string; args: unknown }[] = [];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => {
    invoked.push({ cmd, args });
    if (cmd === "create_peer_connection") return Promise.resolve({ id: "pc-test" });
    if (cmd === "create_offer") return Promise.resolve({ sdpType: "offer", sdp: "v=0\r\n" });
    if (cmd === "create_answer") return Promise.resolve({ sdpType: "answer", sdp: "v=0\r\n" });
    return Promise.resolve(null);
  },
}));

// The shim reaches for these when a track arrives or a description is applied; none of it
// runs in the browser during these tests, so the node environment has to hold them itself.
vi.stubGlobal("RTCSessionDescription", class {
  type: string;
  sdp: string;
  constructor(init: { type: string; sdp?: string }) {
    this.type = init.type;
    this.sdp = init.sdp ?? "";
  }
});

class FakeMediaStreamTrack {
  kind: string;
  id: string;
  constructor(kind = "audio", id = `track-${Math.random().toString(36).slice(2)}`) {
    this.kind = kind;
    this.id = id;
  }
}
// Constructing `MediaStreamTrack` throws in every browser -- it has no constructor. The
// stub has to throw too, or a test cannot catch code that calls `new MediaStreamTrack()`,
// which is exactly what shipped and took down a live call.
vi.stubGlobal(
  "MediaStreamTrack",
  class {
    constructor() {
      throw new TypeError("Illegal constructor");
    }
  },
);

class FakeMediaStream {
  private tracks: FakeMediaStreamTrack[] = [];
  addTrack(t: FakeMediaStreamTrack) {
    this.tracks.push(t);
  }
  getTracks() {
    return this.tracks;
  }
}
vi.stubGlobal("MediaStream", FakeMediaStream);

// RTCTrackEvent isn't available in the node test environment; the shim's real construction
// path is exercised because the constructor is invoked, but a plain Event carrying the same
// fields is enough to assert on for these tests.
vi.stubGlobal("RTCTrackEvent", class extends Event {
  track: unknown;
  streams: unknown;
  receiver: unknown;
  transceiver: unknown;
  constructor(type: string, init: { track: unknown; streams: unknown; receiver: unknown; transceiver: unknown }) {
    super(type);
    this.track = init.track;
    this.streams = init.streams;
    this.receiver = init.receiver;
    this.transceiver = init.transceiver;
  }
});

const { ElementiumRTCPeerConnection } = await import("./webrtc-shim");

/** A connection that has finished creating itself. */
async function connection(): Promise<InstanceType<typeof ElementiumRTCPeerConnection>> {
  const pc = new ElementiumRTCPeerConnection();
  // Let `init` settle, so `pcId` exists and the connection behaves as a live one.
  await Promise.resolve();
  await Promise.resolve();
  return pc;
}

/**
 * Deliver a `remoteTrackAdded` backend event the way Rust does, via the module-level
 * registry `handleBackendEvent` is reached through -- there's no public method for this, so
 * the test goes through the same private path the real event dispatch uses.
 */
function deliverRemoteTrack(pc: InstanceType<typeof ElementiumRTCPeerConnection>, mid: string, kind: string) {
  (pc as unknown as { handleBackendEvent: (e: unknown) => void }).handleBackendEvent({
    type: "remoteTrackAdded",
    pcId: "pc-test",
    mid,
    kind,
  });
}

describe("remote track receivers and transceivers", () => {
  beforeEach(() => {
    invoked.length = 0;
  });

  it("produces a findable receiver with the real mid and track", async () => {
    const pc = await connection();
    deliverRemoteTrack(pc, "1", "audio");

    const receivers = pc.getReceivers();
    expect(receivers).toHaveLength(1);
    expect((receivers[0] as unknown as { track: { kind: string } }).track.kind).toBe("audio");

    const transceivers = pc.getTransceivers();
    expect(transceivers).toHaveLength(1);
    expect(transceivers[0].mid).toBe("1");
    // The receiver on the transceiver is the very same object `getReceivers()` returned --
    // not a second, disconnected one.
    expect(transceivers[0].receiver).toBe(receivers[0]);
  });

  it("dispatches a track event carrying the same receiver and transceiver objects", async () => {
    const pc = await connection();
    let seen: { receiver: unknown; transceiver: unknown; track: unknown } | null = null;
    pc.ontrack = (ev) => {
      seen = ev as unknown as { receiver: unknown; transceiver: unknown; track: unknown };
    };

    // "audio", not "video": the video path reaches for `document.createElement`, which this
    // node-environment test (deliberately, like the rest of this shim's tests) has no DOM
    // for -- see the module doc comment on why `sdpTracingEnabled()` reaches for
    // `globalThis` for the same reason.
    deliverRemoteTrack(pc, "2", "audio");

    expect(seen).not.toBeNull();
    const receivers = pc.getReceivers();
    const transceivers = pc.getTransceivers();
    expect(seen!.receiver).toBe(receivers[0]);
    expect(seen!.transceiver).toBe(transceivers[0]);
    expect(seen!.track).toBe((receivers[0] as unknown as { track: unknown }).track);
  });

  it("getTransceivers includes both local and remote transceivers", async () => {
    const pc = await connection();
    const local = pc.addTransceiver("audio", { direction: "sendonly" });
    deliverRemoteTrack(pc, "5", "audio");

    const transceivers = pc.getTransceivers();
    expect(transceivers).toHaveLength(2);
    expect(transceivers).toContain(local);
    expect(transceivers.find((t) => t.mid === "5")).toBeDefined();
  });

  it("currentDirection is null before negotiation and set after", async () => {
    const pc = await connection();
    const transceiver = pc.addTransceiver("audio", { direction: "sendrecv" });
    expect(transceiver.currentDirection).toBeNull();

    await pc.setLocalDescription({ type: "offer", sdp: "v=0\r\n" });
    expect(transceiver.currentDirection).toBe("sendrecv");

    await pc.setRemoteDescription({ type: "answer", sdp: "v=0\r\n" });
    expect(transceiver.currentDirection).toBe("sendrecv");
  });

  it("currentDirection goes back to null on stop()", async () => {
    const pc = await connection();
    const transceiver = pc.addTransceiver("audio", { direction: "sendonly" });
    await pc.setLocalDescription({ type: "offer", sdp: "v=0\r\n" });
    expect(transceiver.currentDirection).toBe("sendonly");

    transceiver.stop();
    expect(transceiver.currentDirection).toBeNull();
  });

  it("does not throw when a remote audio track arrives", async () => {
    // `new MediaStreamTrack()` is an illegal constructor -- it throws in every browser --
    // and it was the fallback for a stream with no track in it. Only the video path adds
    // one, so every remote *audio* track hit it and threw, out of the try/catch below it,
    // aborting the handler part-way through applying an answer. The connection was left in
    // have-local-offer, the page reported "unable to set answer", and the published
    // microphone was held waiting for a stable state that never came back.
    const pc = await connection();
    expect(() => deliverRemoteTrack(pc, "7", "audio")).not.toThrow();
    // And it still registers, rather than being skipped to avoid the throw.
    expect(pc.getReceivers()).toHaveLength(1);
  });
});
