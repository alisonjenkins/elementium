/**
 * Reconcile the frames Elementium encoded against the frames the far end decoded.
 *
 * The existing call test asserts the far end's decode *rate* is above a floor. That is
 * necessary and weak: a stream losing a fifth of its frames passes it. The live fault it was
 * written for -- twenty frames a minute at the far end while every near-end counter read
 * healthy -- is a shortfall, and a shortfall is only visible when the two sides are counted
 * over the *same interval* and subtracted.
 *
 * # Which near-end number is the number of frames
 *
 * Not `sent`. `sent` in the `outbound video` report is incremented once per *packet* handed
 * to the transport (see `encode_and_send` in `src-tauri/src/commands/media_devices.rs`), and
 * one frame is several packets, so comparing it to `framesDecoded` compares apples to a
 * multiple of pears. The frame count is:
 *
 *     captured - paced_out - undecodable - encode_errors
 *
 * `captured` is every frame that survived the mute check; `paced_out` is what the encode
 * pacer deliberately dropped to hold the frame-rate cap (expected to be large when the camera
 * runs faster than the cap -- that is the limiter working, not loss); `undecodable` and
 * `encode_errors` are frames that never became a bitstream. What is left is what the far end
 * could possibly decode.
 *
 * # Aligning the two windows
 *
 * `outbound video` is emitted every 300 captured frames -- ten seconds at 30fps -- so the
 * near-end counter is a staircase, not a continuous signal. Sampling the far end at an
 * arbitrary instant and the near end "whenever it last spoke" mis-aligns the windows by up to
 * ten seconds, which at 30fps is three hundred frames of imaginary loss.
 *
 * So the window boundaries are *defined by the near end*: wait for a fresh `outbound video`
 * report (polled at 100ms), and read the far end's stats immediately afterwards. The residual
 * skew is the poll interval plus one `getStats` round trip at each boundary, and the transport
 * latency -- constant in steady state -- cancels between the boundaries. See
 * `BOUNDARY_FRAMES` for what that is worth in frames.
 */
import type { Participant } from "./element-call";
import { num, type AppEvent, type ElementiumApp } from "./elementium-app";

/** Everything the far end will say about one inbound video stream. */
export interface InboundVideoDetail {
  ssrc: number;
  /** The `MediaStreamTrack.id` of the receiving track, for attributing a stream to a tile. */
  trackIdentifier: string;
  framesDecoded: number;
  keyFramesDecoded: number;
  /** Frames the depacketiser assembled -- everything that survived the network. */
  framesReceived: number;
  /** Frames that arrived and were then thrown away rather than shown. */
  framesDropped: number;
  packetsReceived: number;
  packetsLost: number;
  bytesReceived: number;
  frameWidth: number;
  frameHeight: number;
}

/**
 * Read every inbound video stream the far end has, with the fields the reconciliation needs.
 *
 * A local copy rather than `element-call.ts`'s `inboundVideo`: this needs `packetsLost`,
 * `framesReceived`, `frameWidth`/`frameHeight` and `trackIdentifier`, and that file is being
 * edited concurrently.
 */
export async function inboundVideoDetail(p: Participant): Promise<InboundVideoDetail[]> {
  return p.widget().evaluate(async () => {
    const store = window as unknown as { __pcs?: RTCPeerConnection[] };
    const out: InboundVideoDetail[] = [];
    for (const pc of store.__pcs ?? []) {
      const report = await pc.getStats();
      report.forEach((r: Record<string, unknown>) => {
        if (r["type"] !== "inbound-rtp" || r["kind"] !== "video") return;
        out.push({
          ssrc: Number(r["ssrc"] ?? 0),
          trackIdentifier: String(r["trackIdentifier"] ?? ""),
          framesDecoded: Number(r["framesDecoded"] ?? 0),
          keyFramesDecoded: Number(r["keyFramesDecoded"] ?? 0),
          framesReceived: Number(r["framesReceived"] ?? 0),
          framesDropped: Number(r["framesDropped"] ?? 0),
          packetsReceived: Number(r["packetsReceived"] ?? 0),
          packetsLost: Number(r["packetsLost"] ?? 0),
          bytesReceived: Number(r["bytesReceived"] ?? 0),
          frameWidth: Number(r["frameWidth"] ?? 0),
          frameHeight: Number(r["frameHeight"] ?? 0),
        });
      });
    }
    return out;
  }) as Promise<InboundVideoDetail[]>;
}

/** A `<video>` on the far end's screen, and whose tile it sits in. */
export interface RenderedTrack {
  /** `MediaStreamTrack.id`, which joins this to `trackIdentifier` in `getStats`. */
  trackId: string;
  /** The tile's visible text -- a display name, and "screen share" wording when it is one. */
  label: string;
  videoWidth: number;
  videoHeight: number;
}

/**
 * Which participant each decoded stream belongs to, read from the rendered tiles.
 *
 * Attribution by resolution is a guess: two people can send the same size. The receiving
 * `MediaStreamTrack.id` is not a guess -- it appears in `getStats` as `trackIdentifier` and on
 * the `<video>` the tile is showing -- and the tile carries the sender's display name. So
 * "this SSRC is Elementium's camera" becomes something read off the far end's own screen
 * rather than inferred.
 */
export async function renderedTracks(p: Participant): Promise<RenderedTrack[]> {
  return p.widget().evaluate(() => {
    const out: RenderedTrack[] = [];
    for (const video of Array.from(document.querySelectorAll("video"))) {
      const source = video.srcObject;
      if (!(source instanceof MediaStream)) continue;
      // The tile, not the <video>: the name is a sibling. Four levels is enough for every
      // Element Call layout seen so far, and the label is only used to *name* a stream --
      // a miss degrades to "unattributed", never to a wrong attribution.
      let node: HTMLElement | null = video;
      let label = "";
      for (let i = 0; i < 4 && node; i++) {
        const text = (node.innerText ?? "").replace(/\s+/g, " ").trim();
        if (text.length > label.length) label = text;
        node = node.parentElement;
      }
      for (const track of source.getVideoTracks()) {
        out.push({
          trackId: track.id,
          label: label.slice(0, 80),
          videoWidth: video.videoWidth,
          videoHeight: video.videoHeight,
        });
      }
    }
    return out;
  }) as Promise<RenderedTrack[]>;
}

/** One `outbound video` report from Elementium, in frames rather than packets. */
export interface OutboundSample {
  /** Milliseconds since the application started, from the log reader. */
  at: number;
  trackId: string;
  captured: number;
  pacedOut: number;
  undecodable: number;
  encodeErrors: number;
  /** Packets, not frames -- kept only to be reported, never compared to a frame count. */
  packetsSent: number;
  skippedNotConnected: number;
  droppedChannelFull: number;
  droppedChannelClosed: number;
  kbytes: number;
}

/** Turn one `outbound video` log event into a sample, or `undefined` if it is not one. */
export function outboundSample(e: AppEvent | undefined): OutboundSample | undefined {
  if (!e || e.message !== "outbound video") return undefined;
  const trackId = e.fields["track_id"];
  if (typeof trackId !== "string") return undefined;
  return {
    at: e.at,
    trackId,
    captured: num(e, "captured") ?? 0,
    pacedOut: num(e, "paced_out") ?? 0,
    undecodable: num(e, "undecodable") ?? 0,
    encodeErrors: num(e, "encode_errors") ?? 0,
    packetsSent: num(e, "sent") ?? 0,
    skippedNotConnected: num(e, "skipped_not_connected") ?? 0,
    droppedChannelFull: num(e, "dropped_channel_full") ?? 0,
    droppedChannelClosed: num(e, "dropped_channel_closed") ?? 0,
    kbytes: num(e, "kbytes") ?? 0,
  };
}

/** The most recent `outbound video` report for one track, if there is one yet. */
export const latestOutbound = (a: ElementiumApp, trackId: string): OutboundSample | undefined =>
  outboundSample(
    a.latest((e) => e.message === "outbound video" && e.fields["track_id"] === trackId),
  );

/**
 * Every video pipeline Elementium has started, newest first, as `(trackId, source, size)`.
 *
 * `video pipeline started` is emitted once per pipeline with the capture size and which kind
 * of source it is -- `camera`, `x11` or `screencast`. That is how the camera track and the
 * screen-share track are told apart at the near end, and where the resolution the far end is
 * checked against comes from: what the capturer actually opened, not what a test assumed.
 */
export interface Pipeline {
  trackId: string;
  source: string;
  width: number;
  height: number;
  at: number;
}

export function pipelines(a: ElementiumApp): Pipeline[] {
  const out: Pipeline[] = [];
  for (const e of a.events) {
    if (e.message !== "video pipeline started") continue;
    const trackId = e.fields["track_id"];
    const source = e.fields["source"];
    if (typeof trackId !== "string" || typeof source !== "string") continue;
    out.push({
      trackId,
      source,
      width: num(e, "width") ?? 0,
      height: num(e, "height") ?? 0,
      at: e.at,
    });
  }
  return out.reverse();
}

/**
 * Wait for the *next* `outbound video` report for `trackId`, then return it.
 *
 * Polled at 100ms rather than the log reader's usual 500ms because this instant is a window
 * boundary: every millisecond between the report being written and the far end's stats being
 * read is skew the reconciliation has to tolerate. See `BOUNDARY_FRAMES`.
 */
export async function nextOutboundReport(
  a: ElementiumApp,
  trackId: string,
  timeoutMs: number,
): Promise<OutboundSample> {
  const start = latestOutbound(a, trackId);
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const now = latestOutbound(a, trackId);
    if (now && (!start || now.captured > start.captured)) return now;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(
    `Elementium published no new "outbound video" report within ${timeoutMs}ms ` +
      `(last captured=${start?.captured ?? "none"}). The report is emitted every 300 captured ` +
      `frames, so this means capture itself has stopped.`,
  );
}

/** Frames that became a bitstream and were handed to the transport. See the header. */
export const framesEncoded = (s: OutboundSample): number =>
  s.captured - s.pacedOut - s.undecodable - s.encodeErrors;

/**
 * The skew, in frames, that the window alignment can be wrong by at *each* boundary.
 *
 * A boundary costs one 100ms log poll plus one `getStats` round trip (~50ms observed). At the
 * 30fps encode cap that is about five frames; ten is that doubled, for a loaded machine. It is
 * a fixed allowance rather than a percentage because it does not grow with the window -- which
 * is the whole reason the window is long.
 */
export const BOUNDARY_FRAMES = 10;

/**
 * The fraction of frames that may go missing before it stops being weather and starts being
 * the bug.
 *
 * Two per cent. On this loopback stack the honest expectation is zero: nothing is lossy
 * between Elementium and the SFU and the SFU and a browser on the same machine, so any
 * sustained shortfall is a fault in the pipeline rather than the network. Two per cent leaves
 * room for a frame in flight at a boundary and for a scheduler hiccup on a busy CI machine,
 * and is still an order of magnitude tighter than "a fifth of frames vanished" -- the case the
 * rate-floor check waves through. Combined with `BOUNDARY_FRAMES`, a 30-second window at 30fps
 * (~900 frames) tolerates about 28 missing frames.
 */
export const LOSS_TOLERANCE = 0.02;

/** How the encoded and decoded counts compared, and where the difference went. */
export interface Reconciliation {
  /** Milliseconds between the two near-end reports that bound the window. */
  windowMs: number;
  encoded: number;
  decoded: number;
  received: number;
  dropped: number;
  lost: number;
  packets: number;
  keyFrames: number;
  /** `encoded - decoded`; negative would mean the far end decoded more than we made. */
  shortfall: number;
  /** What a shortfall this size is allowed to be, from `LOSS_TOLERANCE` and boundaries. */
  allowed: number;
  /** Frames that never arrived: encoded but not assembled at the far end. */
  missingInTransit: number;
  /** Frames that arrived and were not turned into a picture. */
  missingInDecoder: number;
}

export function reconcile(
  before: OutboundSample,
  after: OutboundSample,
  farBefore: InboundVideoDetail,
  farAfter: InboundVideoDetail,
): Reconciliation {
  const encoded = framesEncoded(after) - framesEncoded(before);
  const decoded = farAfter.framesDecoded - farBefore.framesDecoded;
  const received = farAfter.framesReceived - farBefore.framesReceived;
  return {
    windowMs: after.at - before.at,
    encoded,
    decoded,
    received,
    dropped: farAfter.framesDropped - farBefore.framesDropped,
    lost: farAfter.packetsLost - farBefore.packetsLost,
    packets: farAfter.packetsReceived - farBefore.packetsReceived,
    keyFrames: farAfter.keyFramesDecoded - farBefore.keyFramesDecoded,
    shortfall: encoded - decoded,
    allowed: Math.round(encoded * LOSS_TOLERANCE) + 2 * BOUNDARY_FRAMES,
    missingInTransit: encoded - received,
    missingInDecoder: received - decoded,
  };
}

/**
 * The reconciliation as a sentence, and -- when it does not add up -- which fault it is.
 *
 * "The network lost it" and "it arrived and was not decoded" are different faults with
 * different owners, and the two counters that separate them are already in the report. Saying
 * which one it is here is the difference between a test that reports a number and a test that
 * reports a diagnosis.
 */
export function describeReconciliation(who: string, r: Reconciliation): string {
  const seconds = r.windowMs / 1000;
  const lines = [
    `  ${who}: over ${seconds.toFixed(1)}s -- ` +
      `encoded ${r.encoded}, decoded ${r.decoded}, received ${r.received}, ` +
      `dropped ${r.dropped}, packetsLost ${r.lost} of ${r.packets + r.lost}, ` +
      `keyframes ${r.keyFrames}`,
    `    shortfall ${r.shortfall} frames (${
      r.encoded > 0 ? ((100 * r.shortfall) / r.encoded).toFixed(1) : "n/a"
    }%), allowed ${r.allowed}`,
  ];
  if (r.shortfall > r.allowed) {
    lines.push(
      r.missingInTransit > r.missingInDecoder
        ? `    diagnosis: ${r.missingInTransit} frames were encoded and never assembled at the ` +
          `far end (${r.lost} packets lost) -- they did not survive the path.`
        : `    diagnosis: ${r.missingInDecoder} frames arrived and were not decoded ` +
          `(${r.dropped} counted as dropped) -- they got there and the receiver did not ` +
          `turn them into a picture.`,
    );
  }
  return lines.join("\n");
}

/** How long a stream may go without producing a decoded frame before it counts as a stall. */
export const MAX_STALL_MS = 600;

/** A continuity trace: how long each dry spell between decoded frames was. */
export interface Continuity {
  samples: number;
  /** The longest interval, in ms, across which `framesDecoded` did not move. */
  longestGapMs: number;
  /** How many separate intervals exceeded `MAX_STALL_MS`. */
  stalls: number;
  frames: number;
  elapsedMs: number;
}

/**
 * Watch one stream decode, and record the longest interval in which nothing decoded.
 *
 * A rate check cannot see a stall: three hundred frames delivered in two bursts and three
 * hundred delivered evenly are the same number and only one of them is watchable. This polls
 * far more often than the stall threshold, and measures against the wall clock rather than
 * against the requested interval, so a slow `getStats` inflates nothing.
 */
export async function continuity(
  p: Participant,
  ssrc: number,
  windowMs: number,
  intervalMs = 150,
): Promise<Continuity> {
  const started = Date.now();
  const first = (await inboundVideoDetail(p)).find((s) => s.ssrc === ssrc);
  let lastFrames = first?.framesDecoded ?? 0;
  const startFrames = lastFrames;
  let lastMovedAt = started;
  let longestGapMs = 0;
  let stalls = 0;
  let samples = 0;
  let inStall = false;

  while (Date.now() - started < windowMs) {
    await new Promise((r) => setTimeout(r, intervalMs));
    const now = Date.now();
    const s = (await inboundVideoDetail(p)).find((x) => x.ssrc === ssrc);
    samples++;
    const frames = s?.framesDecoded ?? lastFrames;
    if (frames > lastFrames) {
      longestGapMs = Math.max(longestGapMs, now - lastMovedAt);
      lastFrames = frames;
      lastMovedAt = now;
      inStall = false;
    } else if (now - lastMovedAt > MAX_STALL_MS && !inStall) {
      stalls++;
      inStall = true;
    }
  }
  const end = Date.now();
  longestGapMs = Math.max(longestGapMs, end - lastMovedAt);
  return {
    samples,
    longestGapMs,
    stalls,
    frames: lastFrames - startFrames,
    elapsedMs: end - started,
  };
}
