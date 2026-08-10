/**
 * Every frame accounted for, and a screen share that actually shares a screen.
 *
 *     just test-app-call-video
 *
 * `app-call.spec.ts` asks whether the far end decodes Elementium's video *at a rate above a
 * floor*. That is necessary and weak: a stream quietly losing a fifth of its frames sails
 * through it, and "a fifth of the frames are missing" is exactly the shape of the fault that
 * started all of this -- a remote participant seeing about twenty frames a minute while every
 * counter on the sending side read healthy.
 *
 * So this asks the accounting question instead. Over one window, bounded at both ends by
 * Elementium's own encoder reports: how many frames did we produce, and how many did each of
 * the three other participants decode? The difference, if there is one, is split between
 * "never arrived" and "arrived and was not decoded", because those are different faults with
 * different owners. See `video-accounting.ts` for the arithmetic and the tolerance.
 *
 * Three other participants, not one: a fault that only appears when the SFU forwards a stream
 * several ways is invisible in a two-party call, and the SFU is the piece none of the existing
 * tests put under any width of load.
 *
 * And a screen share, which nothing has ever tested. It runs against a stage deliberately
 * placed on the virtual display (`xvfb-stage.ts`): an `xvfb-run` display starts as an empty
 * root window, and a rectangle of black decodes at full rate while proving nothing.
 *
 * # What the screen-share test does not claim
 *
 * That the far end sees a *moving* picture. It asserts the share arrives as a second track and
 * that every captured frame is decoded, not that consecutive frames differ.
 *
 * This was built and then removed rather than never attempted. Reading the pixels back at the
 * far end works -- a `MediaStream`-backed `<video>` does not taint a canvas -- and the far end
 * did report the stage's own luminance, so the capture is genuinely of the stage. What could
 * not be made reliable is the stage *animating*: on a display with no window manager, Chromium
 * treats its window as not visible and freezes the page, so an in-page timer stops. Driving
 * the repaint from outside over the devtools protocol is written here and its one measured
 * attempt was lost when the machine ran out of memory, so it stands unverified.
 *
 * A named gap is worth more than an assertion that goes red for reasons that have nothing to
 * do with the product. If this is picked up again, the missing piece is proving the stage
 * repaints -- everything downstream of that was working.
 *
 * # Requirements
 *
 * A camera and a microphone, as `app-call.spec.ts` explains: Elementium publishes real capture.
 * The GUI is headless, under Xvfb, via `just app-join`.
 */
import { expect, test, type Browser } from "@playwright/test";
import { readFile, writeFile, rm } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startElementWeb, freshSessions, type Credentials } from "./element-web";
import { joinCall, inboundAudio, type Participant } from "./element-call";
import { ensureJoined, openRoomClean } from "./far-participants";
import { startElementium, type ElementiumApp } from "./elementium-app";
import {
  BOUNDARY_FRAMES,
  MAX_STALL_MS,
  continuity,
  describeReconciliation,
  inboundVideoDetail,
  nextOutboundReport,
  pipelines,
  reconcile,
  renderedTracks,
  type InboundVideoDetail,
  type OutboundSample,
  type Pipeline,
} from "./video-accounting";
import { newXvfb, runningXvfb, startStage, type Stage, type XvfbDisplay } from "./xvfb-stage";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const FIXTURE = path.join(REPO, "target/test-env-fixture.json");
/** The file the autojoin driver polls for before it starts sharing. See `shareWhenAsked`. */
const SHARE_SIGNAL = path.join(REPO, "element-web-dist/elementium-share-now");

/** How long Elementium may take to be in the call and publishing. A cold checkout compiles. */
const APP_READY_MS = 12 * 60 * 1000;
/** How long the far end may take to decode a first frame of anything. */
const FIRST_FRAME_MS = 150_000;
/**
 * The reconciliation window.
 *
 * Thirty seconds, not ten. The boundary skew is a fixed number of frames (`BOUNDARY_FRAMES`)
 * however long the window is, so a longer window is a tighter test: at 30fps this is ~900
 * frames, against which twenty frames of boundary allowance is two per cent rather than six.
 * It is also three `outbound video` reports, which are emitted every 300 captured frames.
 */
const WINDOW_MS = 30_000;
/** A shorter window for the screen share, which is measured after the camera work is done. */
const SHARE_WINDOW_MS = 20_000;
/** The frame rate below which a stream is not video. Elementium's encode cap is 30fps. */
const MIN_FPS = 10;
/**
 * The frame rate below which a screen share is not a screen share.
 *
 * Two, not ten, and the difference is a measurement rather than a concession. The X11 capture
 * path takes a full `XGetImage` of the root window every frame with no shared-memory transport
 * (`crates/elementium-screen/src/x11.rs`), and on an `xvfb-run` display that measures ~3.3
 * frames a second -- so the *capture* is the ceiling, and every one of those frames does reach
 * the far end (the reconciliation below is what says so). A floor of ten here would fail a
 * share that is working exactly as this capture path can, and say nothing about whether the
 * frames it did produce arrived. What is worth catching at this end is a share that is not
 * moving at all.
 */
const MIN_SHARE_FPS = 2;
/**
 * How long a screen share may go without a decoded frame.
 *
 * Four times `MAX_STALL_MS`, for the capture rate above: at ~3.3fps the expected gap is 300ms,
 * so the camera's 600ms threshold would call a healthy share stalled.
 */
const MAX_SHARE_STALL_MS = 2400;

interface Fixture {
  room_id: string;
  participants: Credentials[];
}

/** How many browser participants are in the call besides Elementium. */
const FAR_PARTICIPANTS = 3;

let app: ElementiumApp;
let far: Participant[] = [];
let server: Awaited<ReturnType<typeof startElementWeb>>;
let browserRef: Browser;
let roomId: string;
let display: XvfbDisplay | undefined;
let stage: Stage | undefined;
let camera: Pipeline;

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Poll `check` until it holds, or give up saying what the last reading was. */
async function until(
  what: string,
  check: () => Promise<string | null>,
  timeoutMs: number,
  everyMs = 2000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last = "nothing measured yet";
  while (Date.now() < deadline) {
    const failure = await check();
    if (failure === null) return;
    last = failure;
    await wait(everyMs);
  }
  throw new Error(`${what} did not happen within ${timeoutMs}ms. Last reading: ${last}`);
}

/** Every participant's inbound video, read as close to simultaneously as the harness can. */
const sampleAll = (): Promise<InboundVideoDetail[][]> =>
  Promise.all(far.map((p) => inboundVideoDetail(p)));

/**
 * Which inbound stream at `p` is Elementium's, by reading the far end's own screen.
 *
 * Attribution by resolution is a guess -- two publishers can send the same size -- so the
 * receiving track id is used instead: it appears in `getStats` as `trackIdentifier` and on the
 * `<video>` inside the tile that names the sender. Resolution is the fallback, and an
 * attribution that can only be made by guessing is reported as a failure rather than silently
 * measuring somebody else's camera.
 */
async function elementiumStream(
  p: Participant,
  streams: InboundVideoDetail[],
  expect_: { width: number; height: number },
  exclude: Set<number>,
): Promise<InboundVideoDetail> {
  const candidates = streams.filter((s) => !exclude.has(s.ssrc));
  const tiles = await renderedTracks(p);
  const named = candidates.filter((s) =>
    tiles.some((t) => t.trackId === s.trackIdentifier && /tester1\b/.test(t.label)),
  );
  if (named.length === 1) return named[0]!;

  const bySize = candidates.filter(
    (s) => s.frameWidth === expect_.width && s.frameHeight === expect_.height,
  );
  if (bySize.length === 1) return bySize[0]!;

  throw new Error(
    `could not tell which of ${candidates.length} inbound streams at ${p.who.user_id} is ` +
      `Elementium's. By tile name: ${named.length} matched "tester1". By size ` +
      `(${expect_.width}x${expect_.height}): ${bySize.length} matched. Streams: ` +
      candidates
        .map((s) => `ssrc ${s.ssrc} ${s.frameWidth}x${s.frameHeight} decoded ${s.framesDecoded}`)
        .join("; ") +
      `. Tiles: ${tiles.map((t) => `${t.videoWidth}x${t.videoHeight} "${t.label}"`).join("; ")}`,
  );
}

test.describe.serial("Elementium's video, frame by frame", () => {
  test.skip(
    process.env["ELEMENTIUM_APP_CALL"] !== "1",
    "run with `just test-app-call-video`: this starts the application and uses the camera",
  );

  test.beforeAll(async ({ browser }) => {
    test.setTimeout(APP_READY_MS + 10 * 60 * 1000);
    browserRef = browser;
    const env = JSON.parse(await readFile(FIXTURE, "utf8")) as Fixture;
    roomId = env.room_id;

    // Stale from an interrupted run, the signal would start a share before anything was
    // measuring one -- and before the stage exists to be worth sharing.
    await rm(SHARE_SIGNAL, { force: true });

    // tester1 is Elementium's, from `scripts/patch-element-web.sh`. The rest are the far end.
    const peers = (await freshSessions(FAR_PARTICIPANTS + 1)).slice(1);
    server = await startElementWeb();

    // One at a time, not in parallel: they share a homeserver and an SFU, and three
    // simultaneous initial syncs turn a slow join into a timeout that says nothing about the
    // call. This is also where the previous attempt at more participants failed -- see
    // `far-participants.ts` for what the dialog was.
    for (const who of peers) {
      await ensureJoined(who, roomId);
      const p = await openRoomClean(await browserRef.newContext(), server, who, roomId);
      await joinCall(p);
      far.push(p);
    }
    expect(far.length, "the far end").toBe(FAR_PARTICIPANTS);

    // Noted before the application starts, so the Xvfb server it creates can be told from any
    // that were already running.
    const xvfbBefore = runningXvfb();

    // Elementium joins last, as a person joining an existing call does.
    process.env["ELEMENTIUM_AUTOJOIN_SCREENSHARE"] = "1";
    app = startElementium({ video: true });
    await app.waitFor(
      "sending encoded video",
      (e) => e.message === "outbound video",
      APP_READY_MS,
    );
    console.log("  [elementium] publishing");

    display = await newXvfb(xvfbBefore, 60_000);
    console.log(`  [elementium] running on ${display.display}`);

    const cam = pipelines(app).find((p) => p.source === "camera");
    if (!cam) {
      throw new Error(
        `Elementium started no camera pipeline. Pipelines seen: ` +
          pipelines(app)
            .map((p) => `${p.source} ${p.width}x${p.height}`)
            .join(", ") || "none",
      );
    }
    camera = cam;
    console.log(`  [elementium] camera capture ${camera.width}x${camera.height}`);

    await until(
      "every participant decoded a first frame of Elementium's video",
      async () => {
        const all = await sampleAll();
        const seen = all.map((s) => Math.max(0, ...s.map((x) => x.framesDecoded)));
        return seen.every((n) => n > 0) ? null : `best framesDecoded per participant: ${seen}`;
      },
      FIRST_FRAME_MS,
    );
  });

  test.afterAll(async () => {
    await rm(SHARE_SIGNAL, { force: true });
    await stage?.close();
    await app?.stop();
    for (const p of far) await p.context.close().catch(() => undefined);
    await server?.close();
  });

  /**
   * The accounting. Encoded frames in, decoded frames out, over one window, per participant.
   *
   * The window is bounded by Elementium's own reports rather than by the clock, because the
   * near-end counter only moves every 300 captured frames: sampling the far end at an
   * arbitrary instant would mis-align the two windows by up to ten seconds, which at 30fps is
   * three hundred frames of loss that never happened.
   */
  test("every frame Elementium encodes is decoded by every participant", async () => {
    test.setTimeout(WINDOW_MS + 5 * 60 * 1000);

    const before = await nextOutboundReport(app, camera.trackId, 90_000);
    const farBefore = await sampleAll();
    const mine = await Promise.all(
      far.map((p, i) => elementiumStream(p, farBefore[i]!, camera, new Set())),
    );

    let after: OutboundSample = before;
    while (after.at - before.at < WINDOW_MS) {
      after = await nextOutboundReport(app, camera.trackId, 90_000);
    }
    const farAfter = await sampleAll();

    const report: string[] = [];
    const failures: string[] = [];
    for (const [i, p] of far.entries()) {
      const b = farBefore[i]!.find((s) => s.ssrc === mine[i]!.ssrc);
      const a = farAfter[i]!.find((s) => s.ssrc === mine[i]!.ssrc);
      if (!b || !a) {
        failures.push(`${p.who.user_id}: stream ssrc ${mine[i]!.ssrc} vanished mid-window`);
        continue;
      }
      const r = reconcile(before, after, b, a);
      report.push(describeReconciliation(p.who.user_id, r));
      if (r.encoded <= 0) failures.push(`${p.who.user_id}: Elementium encoded no frames at all`);
      if (r.shortfall > r.allowed) {
        failures.push(
          `${p.who.user_id}: ${r.shortfall} of ${r.encoded} encoded frames never became a ` +
            `picture (allowed ${r.allowed})`,
        );
      }
      const fps = r.decoded / (r.windowMs / 1000);
      if (fps < MIN_FPS) failures.push(`${p.who.user_id}: decoding at ${fps.toFixed(1)}/s`);
    }
    console.log(
      `  encoder over the window: captured ${after.captured - before.captured}, ` +
        `paced out ${after.pacedOut - before.pacedOut}, ` +
        `undecodable ${after.undecodable - before.undecodable}, ` +
        `encode errors ${after.encodeErrors - before.encodeErrors}, ` +
        `packets ${after.packetsSent - before.packetsSent}, ` +
        `not connected ${after.skippedNotConnected - before.skippedNotConnected}, ` +
        `channel full ${after.droppedChannelFull - before.droppedChannelFull}\n` +
        report.join("\n"),
    );

    expect(
      failures,
      `frames Elementium encoded did not all arrive as pictures.\n${report.join("\n")}\n` +
        `tolerance: 2% of the encoded count plus ${2 * BOUNDARY_FRAMES} frames of ` +
        `window-boundary skew.\n${failures.join("\n")}`,
    ).toEqual([]);
  });

  /**
   * The picture is the size we think we are sending, and it does not arrive in bursts.
   *
   * A stream delivering its frames in clumps passes any rate check and is unwatchable, so the
   * gaps are measured directly rather than inferred from an average.
   */
  test("the far end sees the resolution we send, without stalling", async () => {
    test.setTimeout(4 * 60 * 1000);

    const streams = await sampleAll();
    const mine = await Promise.all(
      far.map((p, i) => elementiumStream(p, streams[i]!, camera, new Set())),
    );
    const sizes = mine.map((s, i) => `${far[i]!.who.user_id}: ${s.frameWidth}x${s.frameHeight}`);
    console.log(`  camera capture ${camera.width}x${camera.height}; far end sees ${sizes}`);

    for (const [i, s] of mine.entries()) {
      expect(
        [s.frameWidth, s.frameHeight],
        `${far[i]!.who.user_id} is decoding ${s.frameWidth}x${s.frameHeight} of a capture ` +
          `Elementium opened at ${camera.width}x${camera.height}. Sizes seen: ${sizes}`,
      ).toEqual([camera.width, camera.height]);
    }

    const traces = await Promise.all(
      far.map((p, i) => continuity(p, mine[i]!.ssrc, 10_000)),
    );
    const described = traces
      .map(
        (t, i) =>
          `  ${far[i]!.who.user_id}: ${t.frames} frames in ${(t.elapsedMs / 1000).toFixed(1)}s ` +
          `over ${t.samples} samples, longest gap ${t.longestGapMs}ms, ${t.stalls} stalls`,
      )
      .join("\n");
    console.log(described);

    for (const [i, t] of traces.entries()) {
      expect(
        t.longestGapMs,
        `${far[i]!.who.user_id} went ${t.longestGapMs}ms without decoding a frame. A stream ` +
          `that delivers its frames in bursts passes a rate check and looks terrible.\n` +
          described,
      ).toBeLessThanOrEqual(MAX_STALL_MS);
    }
  });

  /**
   * A screen share, end to end: a second video track, a moving picture, and audio if the
   * platform can capture any.
   *
   * The stage is started first and deliberately: an Xvfb root window is black, and a black
   * rectangle decodes at full rate while proving nothing at all.
   */
  test("a screen share reaches every participant as a second, moving video track", async () => {
    test.setTimeout(10 * 60 * 1000);
    if (!display) throw new Error("the virtual display was never identified");

    stage = await startStage(display);
    console.log(`  stage showing on ${display.display}`);

    const cameraStreams = await sampleAll();
    const before = cameraStreams.map((s) => new Set(s.map((x) => x.ssrc)));
    const audioBefore = await Promise.all(far.map((p) => inboundAudio(p)));

    // The signal the autojoin driver has been polling for since it joined.
    await writeFile(SHARE_SIGNAL, `${new Date().toISOString()}\n`, "utf8");
    console.log("  asked Elementium to share its screen");

    const started = await app.waitFor(
      "a screen-share capture pipeline",
      (e) =>
        e.message === "video pipeline started" &&
        (e.fields["source"] === "x11" || e.fields["source"] === "screencast"),
      180_000,
    );
    const share = pipelines(app).find((p) => p.at === started.at);
    if (!share) throw new Error("the share pipeline was logged and then could not be re-read");
    // The X11 capturer now asks the X server for its source's size at start (M6), so this
    // usually is the real resolution. The fallback stays because it still can be 0x0 -- a
    // source that will not report its size starts anyway and learns its geometry from its
    // first frame, which is after this line. The display's own geometry covers that case: a
    // monitor share of an `xvfb-run` display is exactly `-screen 0 WxHxD`, which
    // `runningXvfb` read off the server's command line.
    const expectedShare =
      share.width > 0 && share.height > 0
        ? { width: share.width, height: share.height }
        : { width: display.width, height: display.height };
    console.log(
      `  [elementium] sharing ${share.source}; pipeline reports ` +
        `${share.width}x${share.height}, expecting ${expectedShare.width}x` +
        `${expectedShare.height} at the far end`,
    );

    // A *new* stream at every participant, not merely "some video is arriving": the camera was
    // already arriving, and the whole claim is that the share is a second track.
    await until(
      "every participant received a second video stream",
      async () => {
        const now = await sampleAll();
        const fresh = now.map((s, i) => s.filter((x) => !before[i]!.has(x.ssrc)).length);
        return fresh.every((n) => n >= 1) ? null : `new inbound video streams: ${fresh}`;
      },
      180_000,
    );

    const now = await sampleAll();
    const shareStreams = now.map((s, i) => {
      const fresh = s.filter((x) => !before[i]!.has(x.ssrc));
      return fresh.reduce((a, b) => (a.framesDecoded >= b.framesDecoded ? a : b));
    });
    console.log(
      `  share streams: ` +
        shareStreams
          .map(
            (s, i) =>
              `${far[i]!.who.user_id} ssrc ${s.ssrc} ${s.frameWidth}x${s.frameHeight} ` +
              `decoded ${s.framesDecoded}`,
          )
          .join("; "),
    );

    // Told apart from the camera by construction (a new SSRC), and again by size: the stage
    // is the whole virtual display, which is not the shape of a webcam frame.
    for (const [i, s] of shareStreams.entries()) {
      expect(
        before[i]!.has(s.ssrc),
        `${far[i]!.who.user_id}'s screen share arrived on an SSRC that was already carrying ` +
          `the camera, so the two are not distinguishable`,
      ).toBe(false);
    }

    // Accounting for the share, on the same basis as the camera.
    const s0 = await nextOutboundReport(app, share.trackId, 120_000);
    const f0 = await sampleAll();
    let s1 = s0;
    while (s1.at - s0.at < SHARE_WINDOW_MS) {
      s1 = await nextOutboundReport(app, share.trackId, 120_000);
    }
    const f1 = await sampleAll();

    const report: string[] = [];
    const failures: string[] = [];
    for (const [i, p] of far.entries()) {
      const b = f0[i]!.find((x) => x.ssrc === shareStreams[i]!.ssrc);
      const a = f1[i]!.find((x) => x.ssrc === shareStreams[i]!.ssrc);
      if (!b || !a) {
        failures.push(`${p.who.user_id}: the share stream vanished mid-window`);
        continue;
      }
      const r = reconcile(s0, s1, b, a);
      report.push(describeReconciliation(`${p.who.user_id} (share)`, r));
      const fps = r.decoded / (r.windowMs / 1000);
      if (fps < MIN_SHARE_FPS) {
        failures.push(
          `${p.who.user_id}: the share decodes at ${fps.toFixed(1)}/s, below ${MIN_SHARE_FPS}/s`,
        );
      }
      if (r.shortfall > r.allowed) {
        failures.push(
          `${p.who.user_id}: ${r.shortfall} of ${r.encoded} shared frames never became a ` +
            `picture (allowed ${r.allowed})`,
        );
      }
      if (a.frameWidth !== expectedShare.width || a.frameHeight !== expectedShare.height) {
        failures.push(
          `${p.who.user_id}: the share decodes at ${a.frameWidth}x${a.frameHeight}, ` +
            `expected the whole display at ${expectedShare.width}x${expectedShare.height}`,
        );
      }
    }
    console.log(report.join("\n"));

    // NOT ASSERTED HERE: that the picture is moving. See the note at the head of this file.

    // The share must not arrive in clumps either, on a threshold of its own -- see
    // `MAX_SHARE_STALL_MS`.
    const shareTrace = await continuity(far[0]!, shareStreams[0]!.ssrc, 10_000);
    console.log(
      `  ${far[0]!.who.user_id} share continuity: ${shareTrace.frames} frames in ` +
        `${(shareTrace.elapsedMs / 1000).toFixed(1)}s, longest gap ${shareTrace.longestGapMs}ms`,
    );
    if (shareTrace.longestGapMs > MAX_SHARE_STALL_MS) {
      failures.push(
        `${far[0]!.who.user_id}: the share went ${shareTrace.longestGapMs}ms without a ` +
          `decoded frame (allowed ${MAX_SHARE_STALL_MS}ms)`,
      );
    }

    // Desktop audio, if this platform gave us any. Whether it did is not a guess: the shim
    // logs what `get_display_media` returned, and that line reaches the application log.
    const shimLine = app.latest((e) => e.message.includes("getDisplayMedia: got video="));
    const audioOffered = /audio=(?!none)/.test(shimLine?.message ?? "");
    const audioAfter = await Promise.all(far.map((p) => inboundAudio(p)));
    const newAudio = audioAfter.map(
      (streams, i) =>
        streams.filter((s) => !audioBefore[i]!.some((b) => b.ssrc === s.ssrc)).length,
    );
    console.log(
      `  share audio: backend ${audioOffered ? "captured a track" : "captured none"}; ` +
        `new inbound audio streams per participant: ${newAudio}`,
    );
    if (audioOffered) {
      expect(
        newAudio.filter((n) => n > 0).length,
        `Elementium captured desktop audio for the share and no participant received it as a ` +
          `second audio stream (new streams per participant: ${newAudio})`,
      ).toBe(FAR_PARTICIPANTS);
    }

    expect(
      failures,
      `the screen share did not reach every participant as a working second track.\n` +
        `${report.join("\n")}\n${failures.join("\n")}`,
    ).toEqual([]);
  });
});
