/**
 * `RTCRtpSender.setParameters` used to resolve successfully and change nothing:
 * `getParameters` kept returning the encodings frozen at `addTransceiver` time, so every
 * bandwidth adaptation `livekit-client` believed it made -- 15 call sites in the shipped
 * bundle -- was silently discarded. Video that degraded under a bad link never recovered.
 *
 * What these pin: `setParameters` forwards the aggregate `maxBitrate` (bits per second,
 * converted to kbps) to the Rust pipeline that actually owns the encoder; `getParameters`
 * echoes back whatever was last set, so a caller reading back its own change sees it; and
 * fields this app cannot honour (`scaleResolutionDownBy`, `degradationPreference`) are
 * reported rather than dropped without a trace.
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

// The shim reaches for this constructing local descriptions elsewhere in the module; none of
// it runs in these tests, but the module is imported as a whole, so the environment has to
// hold it. Copied from negotiation-needed.test.ts, which hit the same import-time need.
vi.stubGlobal("RTCSessionDescription", class {
  type: string;
  sdp: string;
  constructor(init: { type: string; sdp?: string }) {
    this.type = init.type;
    this.sdp = init.sdp ?? "";
  }
});

const { ElementiumRTCPeerConnection } = await import("./webrtc-shim");
const { TRACK_SOURCE } = await import("./media-devices");

/** A connection that has finished creating itself, ready to add transceivers to. */
async function connection(): Promise<InstanceType<typeof ElementiumRTCPeerConnection>> {
  const pc = new ElementiumRTCPeerConnection();
  // Let `init` settle, so `pcId` exists and the connection behaves as a live one.
  await Promise.resolve();
  await Promise.resolve();
  return pc;
}

/** A fake camera track carrying the source tag `addTransceiver` reads off a real one. */
function videoTrack(source = "camera"): MediaStreamTrack {
  const track = { kind: "video", id: "track-1" } as unknown as Record<string, unknown>;
  track[TRACK_SOURCE] = source;
  return track as unknown as MediaStreamTrack;
}

describe("RTCRtpSender.setParameters / getParameters", () => {
  beforeEach(() => {
    invoked.length = 0;
  });

  it("forwards the maxBitrate of a single encoding to the backend, in kbps", async () => {
    const pc = await connection();
    const transceiver = pc.addTransceiver(videoTrack());
    invoked.length = 0; // addTransceiver itself does not call set_video_bitrate

    await transceiver.sender.setParameters({
      ...transceiver.sender.getParameters(),
      encodings: [{ maxBitrate: 1_500_000 }],
    });

    const call = invoked.find((c) => c.cmd === "set_video_bitrate");
    expect(call).toBeDefined();
    expect(call?.args).toMatchObject({
      kind: "video",
      source: "camera",
      maxBitratesBps: [1_500_000],
    });
  });

  it("forwards the maximum maxBitrate across several encodings, ignoring ones with none", async () => {
    // livekit-client's congestion control can pass more than one encoding even though this
    // app does not implement simulcast; only the aggregate cap is meaningful here.
    const pc = await connection();
    const transceiver = pc.addTransceiver(videoTrack());
    invoked.length = 0;

    await transceiver.sender.setParameters({
      ...transceiver.sender.getParameters(),
      encodings: [{ maxBitrate: 300_000 }, {}, { maxBitrate: 900_000 }],
    });

    const call = invoked.find((c) => c.cmd === "set_video_bitrate");
    expect(call?.args).toMatchObject({
      maxBitratesBps: [300_000, null, 900_000],
    });
  });

  it("does not call the backend for an audio sender", async () => {
    const pc = await connection();
    const track = { kind: "audio", id: "a1" } as unknown as Record<string, unknown>;
    track[TRACK_SOURCE] = "microphone";
    const transceiver = pc.addTransceiver(track as unknown as MediaStreamTrack);
    invoked.length = 0;

    await transceiver.sender.setParameters({
      ...transceiver.sender.getParameters(),
      encodings: [{ maxBitrate: 64_000 }],
    });

    expect(invoked.find((c) => c.cmd === "set_video_bitrate")).toBeUndefined();
  });

  it("getParameters reflects the last setParameters call", async () => {
    const pc = await connection();
    const transceiver = pc.addTransceiver(videoTrack());

    const before = transceiver.sender.getParameters();
    expect(before.encodings).not.toEqual([{ maxBitrate: 2_000_000, priority: "high" }]);

    const requested = {
      ...before,
      encodings: [{ maxBitrate: 2_000_000, priority: "high" as RTCPriorityType }],
    };
    await transceiver.sender.setParameters(requested);

    const after = transceiver.sender.getParameters();
    expect(after.encodings).toEqual(requested.encodings);
  });

  it("reports scaleResolutionDownBy rather than silently dropping it", async () => {
    const pc = await connection();
    const transceiver = pc.addTransceiver(videoTrack());
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    await transceiver.sender.setParameters({
      ...transceiver.sender.getParameters(),
      encodings: [{ scaleResolutionDownBy: 2 }],
    });

    expect(warn).toHaveBeenCalledWith(expect.stringContaining("scaleResolutionDownBy=2"));
    warn.mockRestore();
  });

  it("reports degradationPreference rather than silently dropping it", async () => {
    const pc = await connection();
    const transceiver = pc.addTransceiver(videoTrack());
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    await transceiver.sender.setParameters({
      ...transceiver.sender.getParameters(),
      degradationPreference: "maintain-resolution",
    });

    expect(warn).toHaveBeenCalledWith(expect.stringContaining("degradationPreference=maintain-resolution"));
    warn.mockRestore();
  });

  it("an encoding with no maxBitrate at all leaves the backend policy to decide, not the shim", async () => {
    // Policy 1 says "no encoding carried a maxBitrate" means "change nothing" -- but that
    // decision belongs to the pure aggregation function in Rust (unit-tested there), not to
    // the shim guessing and skipping the call. The shim's job is only to forward what it was
    // given.
    const pc = await connection();
    const transceiver = pc.addTransceiver(videoTrack());
    invoked.length = 0;

    await transceiver.sender.setParameters({
      ...transceiver.sender.getParameters(),
      encodings: [{}],
    });

    const call = invoked.find((c) => c.cmd === "set_video_bitrate");
    expect(call?.args).toMatchObject({ maxBitratesBps: [null] });
  });
});
