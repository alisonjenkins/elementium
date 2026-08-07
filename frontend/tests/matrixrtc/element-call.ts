/**
 * Drive Element Call itself, rather than livekit-client directly.
 *
 * The faults these tests exist for live in Element Call's key handling: it rotates on every
 * leaver, delays adopting its own new key, and distributes over Matrix to-device messages.
 * A test built on livekit-client would supply its own keys on its own schedule and could not
 * reproduce any of that -- it would be testing livekit, which is not where the fault is.
 */
import { expect, type BrowserContext, type Frame, type Page } from "@playwright/test";
import { useSession, type Credentials, type ElementWebServer } from "./element-web";

const HOMESERVER = "http://localhost:8008";

/** Inbound audio as the receiving browser reports it. */
export interface InboundAudio {
  ssrc: number;
  packetsReceived: number;
  totalSamplesReceived: number;
  concealedSamples: number;
  silentConcealedSamples: number;
}

/**
 * Record every `RTCPeerConnection` the page creates, before Element Call can make one.
 *
 * Stats are read from the connections rather than through Element Call's own UI because a
 * participant tile appears as soon as someone is *in* the call, whether or not their audio
 * is arriving -- and the difference between those two is the entire subject here.
 */
async function recordPeerConnections(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const store = window as unknown as { __pcs?: RTCPeerConnection[] };
    store.__pcs = [];
    const Native = window.RTCPeerConnection;
    const Patched = function (this: unknown, ...args: unknown[]) {
      const pc = new (Native as unknown as new (...a: unknown[]) => RTCPeerConnection)(...args);
      store.__pcs!.push(pc);
      return pc;
    } as unknown as typeof RTCPeerConnection;
    Patched.prototype = Native.prototype;
    window.RTCPeerConnection = Patched;
  });
}

export interface Participant {
  who: Credentials;
  page: Page;
  context: BrowserContext;
  /** The Element Call widget's frame, once it exists. */
  widget: () => Frame;
}

/** The Element Call widget frame, which is where every call control lives. */
function widgetFrame(page: Page): Frame {
  const frame = page.frames().find((f) => f.url().includes("element-call"));
  if (!frame) throw new Error("the Element Call widget is not loaded");
  return frame;
}

/**
 * Open the room as `who`, in a context of their own.
 *
 * One browser context each: Element Call keys E2EE material per participant and stores
 * sessions per origin, so sharing a context would let participants see each other's
 * internals rather than only what reaches them through the SFU.
 */
export async function openRoom(
  context: BrowserContext,
  server: ElementWebServer,
  who: Credentials,
  roomId: string,
): Promise<Participant> {
  const page = await context.newPage();
  await recordPeerConnections(page);
  await useSession(page, who, HOMESERVER);
  console.log(`  [${who.user_id}] loading the room`);
  await page.goto(`${server.origin}/#/room/${roomId}`);
  await page
    .getByRole("textbox", { name: /message|send a message/i })
    .first()
    .waitFor({ timeout: 90_000 });
  return { who, page, context, widget: () => widgetFrame(page) };
}

/**
 * Enter the call and wait until this participant is actually in it.
 *
 * The lobby's "Join call" is not the same event as being in the call: between them the
 * client asks the homeserver for an OpenID token, exchanges it for an SFU token, publishes,
 * and posts its membership. Returning at the click would make every later assertion a race.
 */
export async function joinCall(p: Participant, timeout = 90_000): Promise<void> {
  // Progress is reported because this is slow and multi-step: when it times out, "which of
  // the four waits" is the whole question, and without this the report says only that five
  // minutes passed.
  const step = (s: string) => console.log(`  [${p.who.user_id}] ${s}`);
  step("opening the call");
  await p.page.getByRole("button", { name: "Video call" }).first().click();

  // The widget is created on click, so the frame does not exist until after it.
  await expect
    .poll(() => p.page.frames().some((f) => f.url().includes("element-call")), { timeout })
    .toBe(true);

  step("widget loaded; waiting for the lobby");
  const widget = p.widget();
  await widget.getByRole("button", { name: "Join call" }).click({ timeout });
  step("join clicked; waiting to be in the call");
  // "End call" only exists once joined; the lobby has no such control.
  await widget.getByRole("button", { name: /end call|leave/i }).waitFor({ timeout });
  step("in the call");
}

/** Leave the call, as a participant clicking the button would. */
export async function leaveCall(p: Participant): Promise<void> {
  await p.widget().getByRole("button", { name: /end call|leave/i }).click();
}

/**
 * Inbound audio streams this participant is receiving, one per remote publisher.
 *
 * Read across every peer connection the page made, because Element Call uses a separate
 * connection for publishing and subscribing and only one of them carries inbound media.
 */
export async function inboundAudio(p: Participant): Promise<InboundAudio[]> {
  return p.widget().evaluate(async () => {
    const store = window as unknown as { __pcs?: RTCPeerConnection[] };
    const out: InboundAudio[] = [];
    // Recorded so an empty result can be told apart from a lost hook: no connections at all
    // means this frame is not the one Element Call is using, which is a fault in the harness
    // rather than in the call.
    (window as unknown as { __pcCount?: number }).__pcCount = (store.__pcs ?? []).length;
    for (const pc of store.__pcs ?? []) {
      const report = await pc.getStats();
      report.forEach((r: Record<string, unknown>) => {
        if (r["type"] !== "inbound-rtp" || r["kind"] !== "audio") return;
        out.push({
          ssrc: Number(r["ssrc"] ?? 0),
          packetsReceived: Number(r["packetsReceived"] ?? 0),
          totalSamplesReceived: Number(r["totalSamplesReceived"] ?? 0),
          concealedSamples: Number(r["concealedSamples"] ?? 0),
          silentConcealedSamples: Number(r["silentConcealedSamples"] ?? 0),
        });
      });
    }
    return out;
  }) as Promise<InboundAudio[]>;
}

/**
 * Audio this participant is receiving *and using*, per remote publisher.
 *
 * Packets arriving is not the same as audio being heard. A stream whose key never arrived
 * still produces packets, and one whose key is wrong produces samples -- of noise. Requiring
 * samples that are not concealed is the closest a receiver can get to "this was audible".
 */
export function usable(streams: InboundAudio[]): InboundAudio[] {
  return streams.filter(
    (s) => s.totalSamplesReceived > 0 && s.concealedSamples < s.totalSamplesReceived / 2,
  );
}
