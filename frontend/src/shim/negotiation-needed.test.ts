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

/**
 * Set to hold `create_offer` open, so a test can act in the window between the backend
 * being asked for an SDP and answering. That window is where the fault lived: a track
 * published inside it is in no offer, and whether its request survives depends on when the
 * shim decides what its offer covers.
 */
let holdCreateOffer: { promise: Promise<void>; release: () => void } | null = null;
let holdSetLocal: { promise: Promise<void>; release: () => void } | null = null;

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => {
    invoked.push({ cmd, args });
    if (cmd === "create_peer_connection") return Promise.resolve({ id: "pc-test" });
    if (cmd === "create_offer") {
      const offer = { sdpType: "offer", sdp: "v=0\r\n" };
      const held = holdCreateOffer;
      if (held) return held.promise.then(() => offer);
      return Promise.resolve(offer);
    }
    if (cmd === "create_answer") return Promise.resolve({ sdpType: "answer", sdp: "v=0\r\n" });
    if (cmd === "set_local_description" && holdSetLocal) {
      return holdSetLocal.promise.then(() => null);
    }
    return Promise.resolve(null);
  },
}));

/** Make a command wait until the returned function is called. */
function hold(which: "offer" | "setLocal"): () => void {
  let release: () => void = () => {};
  const promise = new Promise<void>((resolve) => {
    release = resolve;
  });
  const gate = { promise, release };
  if (which === "offer") holdCreateOffer = gate;
  else holdSetLocal = gate;
  return () => {
    if (which === "offer") holdCreateOffer = null;
    else holdSetLocal = null;
    release();
  };
}

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

  it("gets a published track into an offer even when it lands mid-negotiation", async () => {
    // The sequence a real call produced, three times, and the reason a microphone stayed
    // unpublished while every other part of this machinery worked: a description is being
    // applied, the backend is building an offer, and the track is published into that
    // window -- belonging to neither.
    //
    // What matters is not which mechanism rescues it. Either the offer being built ends up
    // carrying it, or a fresh negotiation is asked for afterwards. What must never happen,
    // and did, is that the request is quietly cleared by an offer that does not contain it,
    // leaving the track waiting for an offer already sent without it.
    const { pc, events } = await connection();
    const firstOffer = await pc.createOffer();

    const releaseLocal = hold("setLocal");
    const releaseOffer = hold("offer");
    const applying = pc.setLocalDescription(firstOffer);
    const building = pc.createOffer();
    await settle();

    pc.addTransceiver("audio", { direction: "sendonly" });
    await settle();

    releaseLocal();
    await applying;
    releaseOffer();
    const secondOffer = await building;
    await pc.setLocalDescription(secondOffer);
    await pc.setRemoteDescription({ type: "answer", sdp: "v=0\r\n" });
    await settle();

    const offered = invoked
      .filter((c) => c.cmd === "create_offer")
      .flatMap((c) => ((c.args as { transceivers?: unknown[] }).transceivers ?? []));
    const audioOffered = offered.some(
      (t) => (t as { kind?: string; direction?: string }).kind === "audio"
        && (t as { direction?: string }).direction === "sendonly",
    );
    // Either it made it into an offer, or one is still being asked for. Never neither.
    expect(audioOffered || events() > 0).toBe(true);
  });

  it("does not lose a transceiver added while the backend is building an offer", async () => {
    // The root of three failed builds. `createOffer` handed the backend the pending
    // transceivers and then cleared the list *after* the await -- deleting anything added
    // while the backend was working. livekit-client publishes exactly there: on a real call
    // the backend took 1.1 seconds to answer and the microphone was added 20ms in. It went
    // into no offer, and the record of it was gone, so no later offer could carry it
    // either. The participant showed as muted for the rest of the call.
    const { pc } = await connection();
    const release = hold("offer");

    const building = pc.createOffer();
    await settle();
    // Published while the backend builds.
    pc.addTransceiver("audio", { direction: "sendonly" });
    release();
    await building;

    // The next offer must still know about it.
    await pc.createOffer();
    const offeredAudio = invoked
      .filter((c) => c.cmd === "create_offer")
      .flatMap((c) => ((c.args as { transceivers?: unknown[] }).transceivers ?? []))
      .some((t) => (t as { kind?: string }).kind === "audio");
    expect(offeredAudio).toBe(true);
  });

  it("does not ask for an offer the caller is already building", async () => {
    // The opening of every call, in the order it actually happens: livekit-client adds its
    // receive transceivers *before the connection finishes being created* -- so the request
    // is held -- and then immediately builds an offer describing exactly those. Releasing
    // the held request afterwards made it build a second, empty offer; str0m had nothing
    // new to say so the same SDP came back byte-for-byte, the SFU answered both, and
    // livekit could not match the extra answer. `No pending offer to match answer`,
    // negotiation timeout, connection closed -- every fifteen seconds, all call.
    const pc = new ElementiumRTCPeerConnection();
    let count = 0;
    pc.addEventListener("negotiationneeded", () => {
      count += 1;
    });

    // Before init resolves: held, because there is no connection to negotiate on yet.
    pc.addTransceiver("audio", { direction: "recvonly" });
    pc.addTransceiver("video", { direction: "recvonly" });

    // The caller's own offer, covering exactly those.
    const offer = await pc.createOffer();
    await settle();
    expect(count).toBe(0);

    await pc.setLocalDescription(offer);
    await pc.setRemoteDescription({ type: "answer", sdp: "v=0\r\n" });
    await settle();
    expect(count).toBe(0);

    // A track published afterwards is in no offer, and must still be asked for.
    pc.addTransceiver("audio", { direction: "sendonly" });
    await settle();
    expect(count).toBe(1);
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
