/**
 * Is the audio that comes out the other end the audio that went in?
 *
 *     just test-app-call-audio
 *
 * Every audio measurement this project had before this one answers a different question.
 * Packets received, samples received, Opus frames decoded, concealment ratio: all of them say
 * whether *something* arrived, none of them says whether it is what was sent. The report that
 * started this -- "the mic audio is bad" -- is invisible to all of them, and three theories
 * about it have been disproved by reading code, which is what happens when nobody is
 * comparing a known signal against what came out.
 *
 * So this transmits something recognisable and looks for it at the far end. Each participant
 * plays a ladder of six pure tones, cycling in a fixed order, and every ladder is drawn from a
 * different set of frequencies -- see `tone-ladder.ts` for why a sequence rather than one
 * tone, and why these frequencies. The assertions are about the *identity* of what arrived:
 * the right tones, in the right order, without a gap longer than a stated threshold.
 *
 * # Three participants, and where each signal is measured
 *
 * - **Elementium** plays its ladder as its microphone (`ELEMENTIUM_FAKE_MIC`, see
 *   `fake-audio.ts`), and is measured at the far end by an `AnalyserNode` in a real browser.
 * - **Two Element Web participants** each publish a different ladder through Chromium's
 *   fake capture device, and are measured inside Elementium, from the PCM its own decoder
 *   wrote to disk.
 *
 * Two others rather than one because an SFU forwarding to several subscribers is a different
 * code path from one forwarding to one, and because a fault that mixes two senders together
 * is only visible when there are two.
 *
 * # No feedback path, and nothing on this machine is reconfigured
 *
 * The machine this runs on has a microphone and speakers in the same room. Neither is
 * involved, and neither is altered: every participant's microphone is a file, so the room
 * cannot get into the measurement, and Elementium's playback is silenced by an environment
 * variable on its own process rather than by changing any device or default. The browsers are
 * headless and launched with `--mute-audio`. If any of that were untrue the numbers below
 * would be measuring the room.
 */
import { expect, test, type Browser } from "@playwright/test";
import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";
import { startElementWeb, freshSessions, type Credentials } from "./element-web";
import { joinCall, type Participant } from "./element-call";
import { num, startElementium, type ElementiumApp } from "./elementium-app";
import {
  clearDumps,
  launchTonePublisher,
  openRoom,
  readDumps,
  readProbe,
  startProbe,
  whichLadder,
} from "./audio-probe";
import { LADDERS, assess, describe as describeVerdict, ladderWav } from "./tone-ladder";
import { fakeMicEnv, silentPlaybackEnv } from "./fake-audio";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const FIXTURE = path.join(REPO, "target/test-env-fixture.json");

/** How long the application may take to be in the call and publishing. */
const APP_READY_MS = 12 * 60 * 1000;
/** How long each end is listened to. Three full cycles of a 2.4s ladder, and then some. */
const LISTEN_MS = 12_000;
/**
 * The longest silence, inside a signal that has started, that still counts as intact audio.
 *
 * 300ms is four Opus frames plus a jitter buffer's worth of slack. It is well above anything
 * a healthy stream produces -- a clean generated ladder measures under 120ms, and that is the
 * analysis window straddling a tone boundary rather than any real absence -- and well below
 * the tenths of a second that make speech unintelligible.
 */
const MAX_GAP_MS = 300;
/** How much of the listening period must carry a recognisable tone. */
const MIN_COVERAGE = 0.8;

interface Fixture {
  room_id: string;
  participants: Credentials[];
}

let app: ElementiumApp;
let peerB: Participant;
let peerC: Participant;
let browserB: Browser;
let browserC: Browser;
let server: Awaited<ReturnType<typeof startElementWeb>>;

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

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

test.describe.serial("audio arrives intact", () => {
  test.skip(
    process.env["ELEMENTIUM_APP_CALL_AUDIO"] !== "1",
    "run with `just test-app-call-audio`: this starts the application and reconfigures the " +
      "session's default audio devices",
  );

  test.beforeAll(async () => {
    test.setTimeout(APP_READY_MS + 6 * 60 * 1000);

    const env = JSON.parse(await readFile(FIXTURE, "utf8")) as Fixture;
    const roomId = env.room_id;

    // A dump from a previous run would stop this one recording anything at all: the dump
    // files are opened with `create_new`.
    const stale = await clearDumps();
    if (stale > 0) console.log(`  removed ${stale} audio dumps from a previous run`);

    const micEnv = await fakeMicEnv(LADDERS.elementium);
    const quietEnv = await silentPlaybackEnv();
    if (Object.keys(quietEnv).length === 0) {
      console.log(
        "  [audio] could not locate the system ALSA config, so Elementium's playback is not " +
          "silenced: the other participants' tones will come out of this machine's speakers. " +
          "No measurement depends on it -- every microphone here is a file.",
      );
    }

    // tester1 is Elementium's own -- `patch-element-web.sh` logs the autojoin driver in as
    // tester1 -- so the browser participants are tester2 and tester3, both created by
    // `provision.sh`.
    const peers = (await freshSessions(3)).slice(1);
    const [whoB, whoC] = peers;
    if (!whoB || !whoC) throw new Error("this needs two participants besides Elementium");

    const wavB = path.join(os.tmpdir(), "elementium-peerB-ladder.wav");
    const wavC = path.join(os.tmpdir(), "elementium-peerC-ladder.wav");
    // Chromium loops the file, so one cycle would do; several keeps the loop point rare.
    await writeFile(wavB, ladderWav(LADDERS.peerB, { cycles: 25 }));
    await writeFile(wavC, ladderWav(LADDERS.peerC, { cycles: 25 }));

    server = await startElementWeb();
    browserB = await launchTonePublisher(wavB);
    browserC = await launchTonePublisher(wavC);

    peerB = await openRoom(await browserB.newContext(), server, whoB, roomId);
    await joinCall(peerB);
    peerC = await openRoom(await browserC.newContext(), server, whoC, roomId);
    await joinCall(peerC);
    console.log(`  ${peerB.who.user_id} and ${peerC.who.user_id} are in the call`);

    // Audio only: this test is about audio, and two extra video publishers on one machine
    // buy nothing but CPU contention with the thing being measured.
    app = startElementium({
      video: false,
      env: { ELEMENTIUM_AUDIO_DUMP: "1", ...micEnv, ...quietEnv },
    });
    await app.waitFor(
      "sending encoded audio",
      (e) => e.message === "Outbound audio pipeline" && (num(e, "sent_frames") ?? 0) > 0,
      APP_READY_MS,
    );
    console.log("  [elementium] publishing audio");
  });

  test.afterAll(async () => {
    await app?.stop();
    await peerB?.context.close();
    await peerC?.context.close();
    await browserB?.close();
    await browserC?.close();
    await server?.close();
  });

  /**
   * Elementium -> everyone else, measured in the browsers that received it.
   *
   * The assertion is not "audio arrived". It is that the six tones Elementium was fed came
   * out at the far end at the right frequencies, in the order they were transmitted, with no
   * silence longer than `MAX_GAP_MS` once the signal had started. Reordering, a missing band
   * and a dropout each fail differently, and the failure prints the measurement.
   */
  test("both participants receive Elementium's tone ladder, intact and in order", async () => {
    test.setTimeout(LISTEN_MS + 5 * 60 * 1000);

    for (const p of [peerB, peerC]) {
      await until(
        `${p.who.user_id} has two inbound audio tracks to listen to`,
        async () => {
          const attached = await startProbe(p);
          return attached >= 2 ? null : `${attached} inbound audio tracks attached`;
        },
        120_000,
      );
    }
    await wait(LISTEN_MS);

    for (const p of [peerB, peerC]) {
      const tracks = await readProbe(p);
      const summaries = tracks.map((t) => {
        const best = whichLadder(t.observations);
        return { trackId: t.trackId, ladder: best?.ladder.name ?? "none", verdict: best?.verdict };
      });
      const report = summaries
        .map((s, i) => `    track ${i}: ${s.verdict ? describeVerdict(s.verdict) : "nothing"}`)
        .join("\n");
      console.log(`  ${p.who.user_id} heard ${tracks.length} inbound tracks:\n${report}`);

      // Attribution by content: the track carrying Elementium's frequencies is Elementium's,
      // whatever the signalling says. `assess` is then run against the ladder that was
      // actually sent, so a track carrying the wrong ladder fails as a missing signal.
      const ours = tracks
        .map((t) => assess(t.observations, LADDERS.elementium))
        .reduce((a, b) => (a.tonesSeen >= b.tonesSeen ? a : b));

      expect(
        ours.tonesSeen,
        `${p.who.user_id} did not receive Elementium's signal. Across ` +
          `${tracks.length} inbound tracks the best match was:\n${describeVerdict(ours)}\n` +
          `Expected all ${LADDERS.elementium.tones.length} tones ` +
          `(${LADDERS.elementium.tones.join(", ")}Hz).`,
      ).toBe(LADDERS.elementium.tones.length);
      expect(
        ours.orderErrors.length,
        `${p.who.user_id} received Elementium's tones out of order, which is reordered or ` +
          `skipped audio rather than merely damaged audio.\n${describeVerdict(ours)}`,
      ).toBe(0);
      expect(
        ours.longestGapMs,
        `${p.who.user_id} lost ${ours.longestGapMs.toFixed(0)}ms of Elementium's audio in ` +
          `one stretch.\n${describeVerdict(ours)}`,
      ).toBeLessThanOrEqual(MAX_GAP_MS);
      expect(
        ours.coverage,
        `only ${(ours.coverage * 100).toFixed(0)}% of what ${p.who.user_id} received from ` +
          `Elementium was a recognisable tone.\n${describeVerdict(ours)}`,
      ).toBeGreaterThanOrEqual(MIN_COVERAGE);
    }
  });

  /**
   * Everyone else -> Elementium, measured from the PCM Elementium's own decoder produced.
   *
   * There is no other vantage point: Playwright cannot see inside a Tauri webview with a
   * native WebRTC stack. `ELEMENTIUM_AUDIO_DUMP` writes each inbound track's decoded samples
   * to disk, and they are put through exactly the same analysis as the browser side, so the
   * two directions are one measurement taken twice rather than two different tests.
   *
   * Both ladders must be found, and each in a separate track: a fault that sums two senders
   * into one output would otherwise pass, since every tone would still be present somewhere.
   */
  test("Elementium receives both participants' ladders, intact and separately", async () => {
    test.setTimeout(LISTEN_MS + 5 * 60 * 1000);
    // The dump is capped at 30s per stream, so it must be read while there is still a
    // measurement in it -- and after enough of the call to have one.
    await wait(LISTEN_MS);

    const dumps = await readDumps();
    const inbound = dumps.filter((d) => !d.key.startsWith("capture-"));
    const report = inbound
      .map((d) => {
        const best = whichLadder(d.observations);
        return (
          `    ${d.key} (${d.seconds.toFixed(1)}s, ${d.sampleRate}Hz ${d.channels}ch): ` +
          `${best ? describeVerdict(best.verdict) : "nothing recognisable"}`
        );
      })
      .join("\n");
    console.log(`  [elementium] ${inbound.length} inbound audio dumps:\n${report}`);

    for (const ladder of [LADDERS.peerB, LADDERS.peerC]) {
      const verdicts = inbound.map((d) => ({ key: d.key, v: assess(d.observations, ladder) }));
      const best = verdicts.reduce(
        (a, b) => (a.v.tonesSeen >= b.v.tonesSeen ? a : b),
        { key: "none", v: assess([], ladder) },
      );
      expect(
        best.v.tonesSeen,
        `Elementium never received the ${ladder.name} ladder ` +
          `(${ladder.tones.join(", ")}Hz). It decoded ${inbound.length} inbound audio ` +
          `streams:\n${report}`,
      ).toBe(ladder.tones.length);
      expect(
        best.v.orderErrors.length,
        `Elementium received the ${ladder.name} ladder out of order:\n` +
          `    ${best.key}: ${describeVerdict(best.v)}`,
      ).toBe(0);
      expect(
        best.v.longestGapMs,
        `Elementium lost ${best.v.longestGapMs.toFixed(0)}ms of the ${ladder.name} ladder in ` +
          `one stretch:\n    ${best.key}: ${describeVerdict(best.v)}`,
      ).toBeLessThanOrEqual(MAX_GAP_MS);
    }

    const carriers = new Set(
      [LADDERS.peerB, LADDERS.peerC].map(
        (ladder) =>
          inbound
            .map((d) => ({ key: d.key, v: assess(d.observations, ladder) }))
            .reduce((a, b) => (a.v.tonesSeen >= b.v.tonesSeen ? a : b)).key,
      ),
    );
    expect(
      carriers.size,
      `both ladders were found in the same inbound stream, which means Elementium is mixing ` +
        `two senders into one track rather than keeping them apart:\n${report}`,
    ).toBe(2);
  });

  /**
   * Where a fault is, if there is one -- the outbound path, bisected.
   *
   * `audio_debug_dump.rs` writes the capture path at three points, and running the same
   * analysis over each localises damage rather than reporting it: `capture-raw` failing means
   * the microphone never delivered the signal, `capture-encoder-in` failing means resampling
   * or reframing damaged it, and `capture-loopback` failing -- our own Opus decoded back --
   * means the encoder did.
   *
   * Deliberately last and deliberately narrow: this asserts only that the signal reached
   * Elementium's encoder and survived it. Whether it reached anyone is the first test's
   * business, and this one cannot answer it.
   */
  test("the signal survives Elementium's own capture and encode", async () => {
    test.setTimeout(120_000);
    const dumps = await readDumps();
    for (const key of ["capture-raw", "capture-encoder-in", "capture-loopback"]) {
      const dump = dumps.find((d) => d.key === key);
      expect(dump, `Elementium wrote no ${key} dump; ELEMENTIUM_AUDIO_DUMP did not reach it`)
        .toBeDefined();
      const verdict = assess(dump!.observations, LADDERS.elementium);
      console.log(`  [elementium] ${key}: ${describeVerdict(verdict)}`);
      expect(
        verdict.tonesSeen,
        `Elementium's ${key} does not carry the ladder its microphone was playing.\n` +
          `    ${describeVerdict(verdict)}`,
      ).toBe(LADDERS.elementium.tones.length);
      expect(
        verdict.orderErrors.length,
        `Elementium's ${key} has the ladder's tones out of order.\n    ${describeVerdict(verdict)}`,
      ).toBe(0);
    }
  });

  /**
   * M5: a key distributed once at join and never again is why a far end freezes mid-call.
   *
   * A peer that never received our key cannot decrypt our frames however well our encoder is
   * running, and until `key-distribution-watch.ts` nothing in this project could tell that
   * apart from an encoder fault -- an absence is not an event, and the log recorded the join
   * and the silence after it without saying anything was wrong.
   *
   * Asserted from Elementium's own log rather than from the far end on purpose: the far end
   * already proves it *received* keys, and the fault being watched for is the case where it
   * does not, which is precisely when there is no far end to ask. This is cheap and applies to
   * whatever membership happened to occur during the call above.
   *
   * Both directions are checked. Asserting only the absence of the error would pass on a run
   * where the watch never armed at all -- an absence of failures is not evidence when nothing
   * was tested.
   */
  test("every membership change was answered with a key distribution", () => {
    const overdue = app.events.filter((e) => e.message.includes("no key was distributed"));
    const answered = app.events.filter((e) =>
      e.message.includes("a key distribution followed the membership change"),
    );
    for (const e of answered) console.log(`  ${e.message.split(". ")[0]}`);
    expect(
      overdue.map((e) => `${(e.at / 1000).toFixed(1)}s ${e.message}`),
      "a MatrixRTC membership change went unanswered by any key distribution -- see M5",
    ).toEqual([]);
    expect(
      answered.length,
      "no membership change was ever answered by a key distribution, so the assertion above " +
        "passed without testing anything. Either the call had no membership change or the " +
        "watch is not installed; check for a shim install line naming key-distribution-watch.",
    ).toBeGreaterThan(0);
  });
});
