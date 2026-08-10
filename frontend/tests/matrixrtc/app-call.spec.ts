/**
 * A real call, with Elementium in it, asserted on by nobody.
 *
 *     just test-app-call
 *
 * Every fault in this project so far has been found the same way: the maintainer joins a call
 * with real friends and describes what they see. Friends are not always available, the faults
 * are intermittent, and "frames per minute" is not a measurement. Each assertion below is one
 * such evening, turned into ten seconds of counters.
 *
 * The three pieces this needs all existed already and had never been joined up: the local
 * MatrixRTC stack (`tests/global-setup.ts`), browser participants who can be driven
 * (`peers.spec.ts`), and an Elementium that joins by itself (`just app-join`). See
 * `elementium-app.ts` for the join between the last of those and a test.
 *
 * # What is being measured, and from which side
 *
 * The browser participants are the far end -- what they decode is what a person in the call
 * would have seen. Elementium's own log is the near end. Both are read as *rates over a stated
 * window*, never as "is this counter non-zero": the fault that started this had a healthy
 * non-zero counter for everything and twenty frames a minute of picture.
 *
 * # Requirements
 *
 * A camera and a microphone. Elementium publishes real capture -- there is no fake device on
 * the native path the way Chromium has one -- so this cannot run on a machine with no webcam,
 * and the light comes on while it runs. The GUI itself is headless: `just app-join` runs it
 * under Xvfb.
 */
import { expect, test, type Browser } from "@playwright/test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startElementWeb, freshSessions, type Credentials } from "./element-web";
import {
  openRoom,
  joinCall,
  keysInstalled,
  inboundAudio,
  inboundVideo,
  usable,
  type Participant,
} from "./element-call";
import { num, startElementium, type ElementiumApp } from "./elementium-app";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const FIXTURE = path.join(REPO, "target/test-env-fixture.json");

/**
 * How long Elementium may take to be in the call and publishing.
 *
 * Generous because a cold checkout compiles the application first; `just test-app-call` builds
 * it beforehand so a warm run spends this on starting a webview, syncing a Matrix client and
 * negotiating with the SFU. Bounded because a test that hangs is worse than one that fails.
 */
const APP_READY_MS = 12 * 60 * 1000;
/** How long the far end may take to decode the first frame of our video. */
const FIRST_FRAME_MS = 120_000;
/** How long each rate is measured over. Long enough that a stall cannot hide inside it. */
const WINDOW_MS = 10_000;
/**
 * The frame rate below which a stream is not video.
 *
 * Elementium encodes at 30fps by default (`MAX_ENCODE_FPS`). Ten is a third of that -- far
 * enough below to survive a slow machine, and two orders of magnitude above the keyframe-only
 * failure this exists to catch, which delivered about 0.3.
 */
const MIN_FPS = 10;
/**
 * The most of a stream's decoded frames that may be keyframes.
 *
 * The ratio is the actual signal. A working VP8 stream is a keyframe every few seconds among
 * thirty frames a second -- a percent or two. The fault produced a ratio of one: every frame
 * that decoded was a keyframe, because the delta frames referencing them were being discarded.
 */
const MAX_KEYFRAME_RATIO = 0.2;

/**
 * How many browser participants are already publishing when Elementium joins.
 *
 * One by default, which is the smallest call that can exhibit anything. Raised to probe a
 * specific suspicion: the twenty-frames-a-minute fault has never reproduced here against a
 * single peer, and one difference between this and the calls where it does is the number of
 * people -- the point at which an SFU stops relaying and starts making forwarding decisions
 * about who gets what. `just test-app-call-crowd` sets it to three.
 *
 * Every assertion below applies unchanged at any size; only the population differs, which is
 * the point. If the fault appears at three and not at one, it is reproducible on demand and
 * there is nothing left to guess about.
 */
const EARLY_PEERS = Math.max(1, Number(process.env["ELEMENTIUM_APP_CALL_PEERS"] ?? "1"));

interface Fixture {
  room_id: string;
  participants: Credentials[];
}

/** The call, held open across the tests below; each one measures a different thing about it. */
let app: ElementiumApp;
let early: Participant;
let late: Participant | undefined;
let server: Awaited<ReturnType<typeof startElementWeb>>;
let browserRef: Browser;
let roomId: string;
let peers: Credentials[];
/** The participants publishing alongside `early`, held only so they can be closed. */
let crowd: Participant[] = [];
/** Index in `peers` of the first session not already in the call. */
let lateIndex = 1;

/**
 * How many frames of remote video Elementium has decoded, from whichever path it used.
 *
 * There are two, and which one is live depends on the runtime: the webview decodes the
 * encoded frames itself where WebCodecs exists (20-60kB a frame instead of 3.7MB of RGBA),
 * and Rust decodes them where it does not. Both now report a running count -- the page's
 * through the console bridge, Rust's as a `tracing` event -- and either one answers the
 * question, so this takes whichever is higher rather than assuming a path.
 */
const inboundVideoFrames = (a: ElementiumApp): number => {
  const native =
    num(
      a.latest((e) => e.message === "Inbound video frame decoded"),
      "frames_decoded",
    ) ?? 0;
  const inPage = a.latest((e) => e.message.includes("WebCodecs render progress"));
  const parsed = /decoded=(\d+)/.exec(inPage?.message ?? "");
  return Math.max(native, parsed ? Number(parsed[1]) : 0);
};

/** Which log line says Elementium decoded inbound audio, and how many frames so far. */
const inboundAudioFrames = (a: ElementiumApp): number =>
  num(
    a.latest((e) => e.message === "Decoded inbound Opus audio frame"),
    "count",
  ) ?? 0;

/** Sleep, only ever for a stated interval. */
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Both samples of one inbound video stream, and the rate between them. */
interface VideoRate {
  ssrc: number;
  frames: number;
  keyFrames: number;
  fps: number;
  keyFrameRatio: number;
  bytes: number;
}

/**
 * Measure every inbound video stream over `windowMs`, as rates rather than totals.
 *
 * Deltas, not the counters themselves: a stream that decoded two hundred frames and then
 * stopped an hour ago reports two hundred either way, and the difference between that and live
 * video is the entire question.
 */
async function videoRates(p: Participant, windowMs: number): Promise<VideoRate[]> {
  const before = await inboundVideo(p);
  await wait(windowMs);
  const after = await inboundVideo(p);
  const seconds = windowMs / 1000;
  return after.map((a) => {
    const b = before.find((x) => x.ssrc === a.ssrc);
    const frames = a.framesDecoded - (b?.framesDecoded ?? 0);
    const keyFrames = a.keyFramesDecoded - (b?.keyFramesDecoded ?? 0);
    return {
      ssrc: a.ssrc,
      frames,
      keyFrames,
      fps: frames / seconds,
      keyFrameRatio: frames > 0 ? keyFrames / frames : 1,
      bytes: a.bytesReceived - (b?.bytesReceived ?? 0),
    };
  });
}

/** One line per stream, for a failure message that says what actually happened. */
const describeRates = (rates: VideoRate[], windowMs: number): string =>
  rates.length === 0
    ? "no inbound video streams at all"
    : rates
        .map(
          (r) =>
            `  ssrc ${r.ssrc}: framesDecoded +${r.frames} in ${windowMs / 1000}s ` +
            `(${r.fps.toFixed(1)}/s), keyFramesDecoded +${r.keyFrames} ` +
            `(${(r.keyFrameRatio * 100).toFixed(0)}% of them), bytes +${r.bytes}`,
        )
        .join("\n");

/** Poll `check` until it holds, or give up saying what the last reading was. */
async function until(
  what: string,
  check: () => Promise<string | null>,
  timeoutMs: number,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last = "nothing measured yet";
  while (Date.now() < deadline) {
    const failure = await check();
    if (failure === null) return;
    last = failure;
    await wait(2000);
  }
  throw new Error(`${what} did not happen within ${timeoutMs}ms. Last reading: ${last}`);
}

test.describe.serial("Elementium in a real call", () => {
  test.skip(
    process.env["ELEMENTIUM_APP_CALL"] !== "1",
    "run with `just test-app-call`: this starts the application and uses the camera",
  );

  test.beforeAll(async ({ browser }) => {
    test.setTimeout(APP_READY_MS + 5 * 60 * 1000);
    browserRef = browser;
    const env = JSON.parse(await readFile(FIXTURE, "utf8")) as Fixture;
    roomId = env.room_id;

    // tester1 is Elementium's: `scripts/patch-element-web.sh` logs in as tester1 when it
    // injects the autojoin driver. Taking it here too would put two devices of one user in
    // the call, which is a different situation from two people.
    // One session for Elementium's own tester1 (dropped below), `EARLY_PEERS` to publish
    // before it joins, and one spare for the participant who arrives afterwards.
    // `freshSessions` registers anyone `provision.sh` did not, so the count is free.
    // Only sessions `provision.sh` actually created. Logging in as a tester it never made
    // succeeds at the API and then stops in Element Web at a device-verification dialog,
    // which surfaces two minutes later as a missing message composer. `test-app-call-crowd`
    // provisions enough; asking for more than exist is clamped here rather than left to
    // fail in a way that names the wrong thing.
    const available = env.participants.length - 1;
    if (EARLY_PEERS > available) {
      console.log(
        `  asked for ${EARLY_PEERS} early participants but only ${available} are ` +
          `provisioned; using ${available}. Re-run with ELEMENTIUM_TEST_PARTICIPANTS set.`,
      );
    }
    const earlyCount = Math.min(EARLY_PEERS, available);
    peers = (await freshSessions(env.participants.length)).slice(1);
    const [first] = peers;
    if (!first) throw new Error("provision.sh produced nobody to be the other side");

    server = await startElementWeb();
    early = await openRoom(await browser.newContext(), server, first, roomId);
    await joinCall(early);

    // The rest of the crowd, joined before Elementium for the same reason `early` is: the
    // fault is reported by people who were already in the call when it arrived.
    crowd = [];
    for (const who of peers.slice(1, earlyCount)) {
      // Sequential rather than concurrent: each join is a Matrix sync and an SFU
      // negotiation, and starting several at once makes a failure hard to attribute.
      // eslint-disable-next-line no-await-in-loop
      const peer = await openRoom(await browser.newContext(), server, who, roomId);
      // eslint-disable-next-line no-await-in-loop
      await joinCall(peer);
      crowd.push(peer);
    }
    if (earlyCount > 1) {
      console.log(`  ${earlyCount} participants publishing before Elementium joins`);
    }
    lateIndex = earlyCount;

    // Elementium joins *after* the first participant, because that is the order a person
    // joins a call in and the order every reported fault was seen in.
    app = startElementium({ video: true });
    // "outbound video" is the encoder's own throttled report and carries `sent`, so this
    // waits for frames actually handed to the transport rather than for a track being
    // announced. Announced and sending are different states, and the gap between them is
    // where a call that looks joined carries nothing.
    await app.waitFor(
      "sending encoded video",
      (e) => e.message === "outbound video" && (num(e, "sent") ?? 0) > 0,
      APP_READY_MS,
    );
    console.log("  [elementium] publishing");

    // Media, not membership. Element Call shows a tile for anyone who is *in* the call, and
    // the whole subject here is the difference between that and a picture.
    await until(
      "the far end decoded a first frame of Elementium's video",
      async () => {
        const streams = await inboundVideo(early);
        const best = Math.max(0, ...streams.map((s) => s.framesDecoded));
        return best > 0 ? null : `${streams.length} inbound video streams, best framesDecoded=0`;
      },
      FIRST_FRAME_MS,
    );
  });

  test.afterAll(async () => {
    await app?.stop();
    for (const peer of crowd) {
      // eslint-disable-next-line no-await-in-loop
      await peer.context.close();
    }
    await early?.context.close();
    await late?.context.close();
    await server?.close();
  });

  /**
   * The fault: the far end saw about twenty frames a minute, because it could decode our
   * keyframes and nothing else. Every counter we had -- packets sent, bytes, frames encoded --
   * looked like a working call, and so did `packetsReceived` at the far end.
   */
  test("the far end decodes our delta frames, not only our keyframes", async () => {
    test.setTimeout(WINDOW_MS + 120_000);
    const rates = await videoRates(early, WINDOW_MS);
    console.log(`  inbound video at ${early.who.user_id}:\n${describeRates(rates, WINDOW_MS)}`);

    const best = rates.reduce<VideoRate | undefined>(
      (a, b) => (a && a.frames >= b.frames ? a : b),
      undefined,
    );
    const diagnosis = describeRates(rates, WINDOW_MS);
    expect(
      best?.fps ?? 0,
      `Elementium's video is not being decoded at a video frame rate.\n${diagnosis}\n` +
        `expected at least ${MIN_FPS}/s`,
    ).toBeGreaterThanOrEqual(MIN_FPS);
    expect(
      best?.keyFrameRatio ?? 1,
      `almost every frame the far end decoded was a keyframe, which is what a stream whose ` +
        `delta frames are being discarded looks like.\n${diagnosis}\n` +
        `expected at most ${MAX_KEYFRAME_RATIO * 100}% keyframes`,
    ).toBeLessThanOrEqual(MAX_KEYFRAME_RATIO);
  });

  /**
   * The fault: two independent bugs in one evening meant remote participants were never
   * visible in Elementium at all. Asserted from Elementium's own log, because there is no way
   * to read a Tauri webview's decoder from outside it.
   */
  test("Elementium decodes the video the far end is sending", async () => {
    test.setTimeout(WINDOW_MS + 180_000);
    // The far end has to be publishing before this means anything, and Element Call can take
    // a moment to get around to it after joining.
    await until(
      "Elementium decoded a first inbound video frame",
      async () => {
        const n = inboundVideoFrames(app);
        return n > 0 ? null : `frames_decoded=${n} across ${app.events.length} log events`;
      },
      120_000,
    );

    const before = inboundVideoFrames(app);
    await wait(WINDOW_MS);
    const after = inboundVideoFrames(app);
    const failures =
      num(
        app.latest((e) => e.message === "Inbound video frame decoded"),
        "decode_failures",
      ) ?? 0;
    const fps = (after - before) / (WINDOW_MS / 1000);
    console.log(
      `  [elementium] inbound video frames_decoded ${before} -> ${after} ` +
        `(${fps.toFixed(1)}/s), decode_failures ${failures}`,
    );

    // Six a second, not thirty: this counter is reported every thirtieth frame, so the
    // measurable rate is quantised in steps of three per second over this window. Six is
    // comfortably above the quantisation and still an order of magnitude above a stream that
    // decodes a keyframe now and then.
    expect(
      fps,
      `Elementium is not decoding inbound video at a video frame rate: ` +
        `frames_decoded went ${before} -> ${after} in ${WINDOW_MS / 1000}s ` +
        `(${fps.toFixed(1)}/s), with ${failures} decode failures. Expected at least 6/s.`,
    ).toBeGreaterThanOrEqual(6);
  });

  /** Audio in both directions, measured at each end rather than assumed from the other. */
  test("audio flows both ways", async () => {
    test.setTimeout(WINDOW_MS + 180_000);

    // Elementium -> the far end. `usable` rather than `packetsReceived`: a stream whose key
    // never arrived still produces packets, and one whose key is wrong still produces samples.
    await until(
      `${early.who.user_id} can use Elementium's audio`,
      async () => {
        const streams = await inboundAudio(early);
        const ok = usable(streams);
        return ok.length > 0
          ? null
          : `${streams.length} inbound audio streams, ${ok.length} usable ` +
              `(samples ${streams.map((s) => s.totalSamplesReceived).join(",") || "none"})`;
      },
      120_000,
    );
    console.log(`  ${early.who.user_id} hears Elementium`);

    // The far end -> Elementium.
    const before = inboundAudioFrames(app);
    await wait(WINDOW_MS);
    const after = inboundAudioFrames(app);
    console.log(
      `  [elementium] inbound Opus frames ${before} -> ${after} in ${WINDOW_MS / 1000}s`,
    );
    // Opus frames are 20ms, so a live stream is fifty a second and ten seconds is five
    // hundred. A hundred is a fifth of that: unambiguously flowing, without failing a run on a
    // machine that lost a moment to a decode stall.
    expect(
      after - before,
      `Elementium is not decoding the far end's audio: count went ${before} -> ${after} ` +
        `in ${WINDOW_MS / 1000}s, expected at least 100 (a live stream is ~500).`,
    ).toBeGreaterThanOrEqual(100);
  });

  /**
   * The fault this was written to catch, which it does not reproduce -- and that is a result.
   *
   * The expectation going in was a failure: Elementium distributes its E2EE key once, at join,
   * so a participant arriving later should be permanently unable to decrypt it. Measured, the
   * late joiner decoded both remote streams at full rate and its key log named Elementium
   * among the senders. This was written as `test.fail()` first and Playwright reported it as
   * passing unexpectedly, which is how the assumption was checked rather than restated.
   *
   * The likely reason: in this configuration Elementium is a *host for Element Call*, and it
   * is Element Call -- inside the widget -- that generates, distributes and rotates the key
   * over Matrix to-device messages, including to a joiner. Elementium's part is to adopt each
   * key it is handed, and it does: sixteen `E2EE key received` events in a single call here.
   * So the "sends its key once" fault, if it is real, belongs to the path where Elementium
   * owns key distribution itself -- the native LiveKit client in `livekit/room.rs` -- which
   * this test does not exercise and cannot speak for.
   *
   * Left as an ordinary assertion, not `test.fail()`: it holds today, and the day it stops is
   * the day a late joiner has gone deaf and blind to us again.
   */
  test("a participant who joins after Elementium can decrypt its media", async () => {
    test.setTimeout(6 * 60 * 1000);

    // The first session not already in the call: `peers[0]` is `early` and the next
    // `EARLY_PEERS - 1` are the crowd.
    const third = peers[lateIndex];
    if (!third) throw new Error("no spare session for the participant who joins late");
    late = await openRoom(await browserRef.newContext(), server, third, roomId);
    await joinCall(late);
    console.log(`  ${late.who.user_id} joined after Elementium`);

    // Two remote publishers now: the participant who was already here, and Elementium. Both
    // must decode. One of the two decoding is exactly the fault -- and is why this is asserted
    // per stream rather than as "some video arrived".
    await until(
      `${late.who.user_id} decodes video from both other participants`,
      async () => {
        const rates = await videoRates(late!, 4000);
        const good = rates.filter((r) => r.fps >= MIN_FPS);
        return good.length >= 2 ? null : `${good.length}/2 streams above ${MIN_FPS}/s`;
      },
      90_000,
    );

    const rates = await videoRates(late, WINDOW_MS);
    const keys = await keysInstalled(late);
    const senders = [...new Set(keys.map((k) => k.participantIdentity))].sort();
    // Identity, never key material -- `observeKeys` records only who and which index.
    const fromElementium = senders.filter((s) => s.includes("tester1"));
    console.log(
      `  late joiner ${late.who.user_id}:\n${describeRates(rates, WINDOW_MS)}\n` +
        `  keys installed from [${senders.join(", ")}]`,
    );

    const decoding = rates.filter((r) => r.fps >= MIN_FPS);
    expect(
      decoding.length,
      `a participant who joined after Elementium can decode ${decoding.length} of the 2 ` +
        `streams in the call.\n${describeRates(rates, WINDOW_MS)}\n` +
        `keys installed from [${senders.join(", ")}] -- Elementium's own key ` +
        `${fromElementium.length > 0 ? "did arrive" : "never arrived"}.`,
    ).toBeGreaterThanOrEqual(2);
    expect(
      fromElementium.length,
      `the late joiner never installed a key from Elementium (tester1). Keys came from ` +
        `[${senders.join(", ")}]. Elementium sends its key once, at join, so a participant ` +
        `who was not there to receive it never gets one.`,
    ).toBeGreaterThan(0);
  });
});
