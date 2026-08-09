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

/**
 * Let the negotiation-needed check run.
 *
 * Both a microtask drain and a task turn: the initial check is queued as a microtask, and
 * the re-check that runs when a description finishes applying is queued as a task, which is
 * what the DOM does too. A microtask-only drain silently missed the second kind.
 */
async function settle(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
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

  it("does not ask for a second offer while one is still being applied", async () => {
    // `setLocalDescription` awaits the IPC before it updates the signalling state, so for
    // the width of that await the state still reads "stable" while an offer is in fact
    // being applied. A check landing in that window fired, livekit-client made a second
    // offer over the first, and the SFU answered with `NegotiationError: No pending offer
    // to match answer` -- a call that connected to nothing.
    const { pc, events } = await connection();
    const applying = pc.setLocalDescription({ type: "offer", sdp: "v=0\r\n" });
    // Mid-flight: exactly where the real addTransceiver landed.
    pc.addTransceiver("audio", { direction: "sendonly" });
    await settle();
    expect(events()).toBe(0);

    await applying;
    await settle();
    // Still held: an offer of ours is outstanding until its answer arrives.
    expect(events()).toBe(0);

    await pc.setRemoteDescription({ type: "answer", sdp: "v=0\r\n" });
    await settle();
    expect(events()).toBe(1);
  });

  it("holds a request made before the connection exists, then serves it", async () => {
    // livekit-client adds its receive transceivers immediately after constructing the peer
    // connection, and attaches `onnegotiationneeded` afterwards. Firing in that window
    // reached nobody -- and because firing clears the flag, the request was consumed rather
    // than served. A real call published nothing for this reason.
    const pc = new ElementiumRTCPeerConnection();
    pc.addTransceiver("audio", { direction: "sendonly" });
    let count = 0;
    // Attached after construction, exactly as livekit-client does it.
    pc.addEventListener("negotiationneeded", () => {
      count += 1;
    });
    await settle();
    await settle();
    await settle();
    expect(count).toBe(1);
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
