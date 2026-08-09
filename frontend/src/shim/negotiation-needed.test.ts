/**
 * Publishing a track has to ask for an offer, because nothing else will.
 *
 * livekit-client publishes by adding a sendonly transceiver and then waiting to be asked
 * for an offer — `pc.onnegotiationneeded` is the whole of its publisher trigger. This shim
 * recorded the transceiver for the next offer and fired nothing, so an offer never came:
 * the publisher timed out after fifteen seconds, logged `negotiation disconnected`, and
 * rebuilt the room. A call did that eight times in ninety seconds with the microphone never
 * published once, and the participant tile showed a muted icon that was telling the truth.
 *
 * What these pin is the event, its coalescing, and the state rule that decides when it may
 * be delivered — an offer requested while another is outstanding is a glare, and the one
 * that matters is the one that arrives after the exchange completes.
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

// The shim reaches for these when a track arrives; none of it runs in these tests, but the
// module is imported as a whole, so the environment has to hold them.
vi.stubGlobal("RTCSessionDescription", class {
  type: string;
  sdp: string;
  constructor(init: { type: string; sdp?: string }) {
    this.type = init.type;
    this.sdp = init.sdp ?? "";
  }
});

const { ElementiumRTCPeerConnection } = await import("./webrtc-shim");

/** A connection that has finished creating itself, with its negotiation events counted. */
async function connection(): Promise<{
  pc: InstanceType<typeof ElementiumRTCPeerConnection>;
  events: () => number;
}> {
  const pc = new ElementiumRTCPeerConnection();
  let count = 0;
  pc.addEventListener("negotiationneeded", () => {
    count += 1;
  });
  // Let `init` settle, so `pcId` exists and the connection behaves as a live one.
  await Promise.resolve();
  await Promise.resolve();
  return { pc, events: () => count };
}

/** Run queued microtasks, which is where the spec puts the negotiation-needed check. */
async function settle(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

describe("negotiationneeded", () => {
  beforeEach(() => {
    invoked.length = 0;
  });

  it("fires when a sendonly transceiver is added, which is how a track is published", async () => {
    const { pc, events } = await connection();
    pc.addTransceiver("audio", { direction: "sendonly" });
    await settle();
    expect(events()).toBe(1);
  });

  it("fires once for a burst, not once per addition", async () => {
    // The camera and the microphone are published together. Two offers for one publish is a
    // renegotiation the far end has to process for nothing.
    const { pc, events } = await connection();
    pc.addTransceiver("audio", { direction: "sendonly" });
    pc.addTransceiver("video", { direction: "sendonly" });
    await settle();
    expect(events()).toBe(1);
  });

  it("waits for stable when an offer is already outstanding", async () => {
    const { pc, events } = await connection();
    await pc.setLocalDescription({ type: "offer", sdp: "v=0\r\n" });
    pc.addTransceiver("audio", { direction: "sendonly" });
    await settle();
    // have-local-offer: asking for another offer now is a glare.
    expect(events()).toBe(0);

    await pc.setRemoteDescription({ type: "answer", sdp: "v=0\r\n" });
    await settle();
    // The exchange completed, so the track added mid-negotiation gets its offer.
    expect(events()).toBe(1);
  });

  it("does not re-ask after an offer that already describes the change", async () => {
    const { pc, events } = await connection();
    pc.addTransceiver("audio", { direction: "sendonly" });
    await settle();
    expect(events()).toBe(1);

    await pc.setLocalDescription({ type: "offer", sdp: "v=0\r\n" });
    await pc.setRemoteDescription({ type: "answer", sdp: "v=0\r\n" });
    await settle();
    expect(events()).toBe(1);
  });

  it("fires for addTrack, which publishes just as much as a transceiver does", async () => {
    const { pc, events } = await connection();
    pc.addTrack({ kind: "audio", id: "a1" } as MediaStreamTrack);
    await settle();
    expect(events()).toBe(1);
  });

  it("does not fire for a removeTrack that removed nothing", async () => {
    // livekit-client calls removeTrack for senders it has already dropped.
    const { pc, events } = await connection();
    pc.removeTrack({} as RTCRtpSender);
    await settle();
    expect(events()).toBe(0);
  });
});
