/**
 * Listen for a known signal at each end of a call.
 *
 * Two observers, one arithmetic (`tone-ladder.ts`):
 *
 * - **In the browser.** An `AnalyserNode` on each inbound `MediaStreamTrack`, sampled every
 *   40ms. This is the far end: what it reports is what a person in the call would have heard,
 *   after decryption, decoding and the jitter buffer.
 * - **In Elementium.** `ELEMENTIUM_AUDIO_DUMP` writes the decoded PCM of every inbound track
 *   to `/tmp` as raw f32; those files are read here and put through the same measurement. The
 *   near end cannot be observed any other way -- it is a Tauri webview with a native WebRTC
 *   stack, and Playwright cannot reach inside it.
 *
 * Nothing here logs a raw line from anywhere. The dump filenames carry a peer-connection id
 * and a media id and nothing else; the browser probe returns frequencies and levels.
 */
import { chromium, type Browser, type BrowserContext, type Page } from "@playwright/test";
import { readFile, readdir, unlink } from "node:fs/promises";
import path from "node:path";
import { readdirSync } from "node:fs";
import { useSession, type Credentials, type ElementWebServer } from "./element-web";
import { observeKeys, type Participant } from "./element-call";
import { ALL_TONES, LADDERS, identify, observePcm, type Observation } from "./tone-ladder";

/** How often each inbound track is sampled in the browser. Matches `observePcm`'s window. */
const PROBE_INTERVAL_MS = 40;

/**
 * Launch a browser whose microphone is a tone ladder rather than Chromium's own fake device.
 *
 * `--use-file-for-fake-audio-capture` is what makes the reverse direction measurable at all:
 * without it the "microphone" is Chromium's pulsed beep, which is silence most of the time,
 * so a dropout and correct behaviour look identical. It is already used suite-wide in
 * `playwright.config.ts` with a single 440Hz tone; a *per-participant* file means a browser
 * per participant, because the flag is a browser-level command line argument and not
 * something a context can override.
 *
 * The rest of the arguments are the ones `playwright.config.ts` sets, repeated because
 * launching a browser directly does not inherit them. `--enable-blink-features=
 * RTCInsertableStreams` is the one that is not optional: livekit's E2EE worker is built on
 * insertable streams, and without it this participant fails for a reason unrelated to audio.
 */
export async function launchTonePublisher(wavPath: string): Promise<Browser> {
  return chromium.launch({
    executablePath: chromiumPath(),
    args: [
      "--no-sandbox",
      "--use-fake-ui-for-media-stream",
      "--use-fake-device-for-media-stream",
      `--use-file-for-fake-audio-capture=${wavPath}`,
      "--enable-blink-features=RTCInsertableStreams",
      // Headless Chromium has no audio output device, so nothing this participant receives
      // can reach the room's speakers and be picked up by anything. Stated because the
      // absence of a feedback path is a precondition of every number below.
      "--mute-audio",
    ],
  });
}

/** The same Chromium `playwright.config.ts` uses, found the same way. */
function chromiumPath(): string | undefined {
  const explicit = process.env["ELEMENTIUM_CHROMIUM"];
  if (explicit) return explicit;
  const root = process.env["PLAYWRIGHT_BROWSERS_PATH"];
  if (!root) return undefined;
  const dir = readdirSync(root)
    .filter((d) => d.startsWith("chromium-"))
    .sort()
    .pop();
  return dir ? `${root}/${dir}/chrome-linux64/chrome` : undefined;
}

/**
 * Record every `RTCPeerConnection` the page makes, and open the room as `who`.
 *
 * A local copy of what `element-call.ts` does, deliberately, for two reasons. It is being
 * edited concurrently for an unrelated fix, and this version has to dismiss one more thing:
 * a device-verification dialog that sits over the room and stops the composer ever becoming
 * visible, which presents as "the room never loaded" after two minutes. The dialog appears
 * for sessions whose account has cross-signing set up -- most often a tester the shared
 * `provision.sh` did not create.
 *
 * Dismissing rather than satisfying it leaves this participant an *unverified* device. That
 * is worth stating precisely, because these tests do touch encryption: Element Call's media
 * keys travel as Matrix to-device messages, and an unverified device still receives them --
 * unverified is not the same as blocked, and only a room with "never send to unverified
 * devices" set would change that. Nothing here asserts anything about *who* a key came from
 * or whether it should have; it asserts that audio arrived intact, which requires the key to
 * have worked. If it did not, the assertion fails rather than quietly passing.
 */
export async function openRoom(
  context: BrowserContext,
  server: ElementWebServer,
  who: Credentials,
  roomId: string,
): Promise<Participant> {
  const page = await context.newPage();
  await recordPeerConnections(page);
  await observeKeys(page);
  await useSession(page, who, "http://localhost:8008");
  await page.goto(`${server.origin}/#/room/${roomId}`);
  await dismissInterruptions(page, 120_000);
  await page
    .getByRole("textbox", { name: /message|send a message/i })
    .first()
    .waitFor({ timeout: 30_000 });
  return {
    who,
    page,
    context,
    widget: () => {
      const frame = page.frames().find((f) => f.url().includes("element-call"));
      if (!frame) throw new Error("the Element Call widget is not loaded");
      return frame;
    },
  };
}

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

/**
 * Click past anything standing between a fresh session and the room.
 *
 * Raced against the composer appearing rather than waited on in sequence: on most runs none
 * of these appear, and a fixed wait for each would cost half a minute per participant.
 */
async function dismissInterruptions(page: Page, timeoutMs: number): Promise<void> {
  const composer = page.getByRole("textbox", { name: /message|send a message/i }).first();
  const deadline = Date.now() + timeoutMs;
  const buttons = [
    /^(later|skip|dismiss|continue|got it)$/i,
    // The device-verification dialog's escape hatches, which are worded differently in
    // different Element Web versions.
    /skip verification|verify later|i'll verify later|not now|use another device/i,
  ];
  while (Date.now() < deadline) {
    if (await composer.isVisible().catch(() => false)) return;
    for (const name of buttons) {
      const button = page.getByRole("button", { name }).first();
      if (await button.isVisible().catch(() => false)) {
        await button.click().catch(() => undefined);
      }
    }
    await page.waitForTimeout(500);
  }
}

/** One inbound track, as the far end heard it. */
export interface TrackObservations {
  trackId: string;
  observations: Observation[];
}

/**
 * Attach an analyser to every inbound audio track this participant currently has.
 *
 * Called after both other publishers are known to be in the call: an analyser cannot be
 * attached to a track that does not exist yet, and the count returned is how the caller
 * checks that assumption instead of assuming it.
 *
 * The muted `<audio>` element is not decoration. Chromium will not pull data through a
 * `MediaStreamAudioSourceNode` built on a remote track unless that track is also attached to
 * a media element somewhere; without it the analyser reports a flat -Infinity forever, which
 * looks exactly like a call with no audio in it.
 */
export async function startProbe(p: Participant): Promise<number> {
  return p.widget().evaluate(
    async (intervalMs: number) => {
      const w = window as unknown as {
        __pcs?: RTCPeerConnection[];
        __toneProbe?: { probes: { id: string; samples: unknown[] }[] };
        __toneProbeTimer?: number;
      };
      if (w.__toneProbe) return w.__toneProbe.probes.length;

      const ctx = new AudioContext({ sampleRate: 48000 });
      await ctx.resume();
      const started = performance.now();
      const probes: {
        id: string;
        analyser: AnalyserNode;
        buffer: Float32Array;
        element: HTMLAudioElement;
        samples: { atMs: number; hz: number; db: number; floorDb: number }[];
      }[] = [];

      for (const pc of w.__pcs ?? []) {
        for (const receiver of pc.getReceivers()) {
          const track = receiver.track;
          if (!track || track.kind !== "audio") continue;
          if (probes.some((x) => x.id === track.id)) continue;
          const stream = new MediaStream([track]);
          const element = new Audio();
          element.srcObject = stream;
          element.muted = true;
          void element.play().catch(() => undefined);
          const analyser = ctx.createAnalyser();
          analyser.fftSize = 4096;
          analyser.smoothingTimeConstant = 0;
          ctx.createMediaStreamSource(stream).connect(analyser);
          probes.push({
            id: track.id,
            analyser,
            buffer: new Float32Array(analyser.frequencyBinCount),
            element,
            samples: [],
          });
        }
      }

      const binHz = ctx.sampleRate / 4096;
      w.__toneProbeTimer = window.setInterval(() => {
        const atMs = performance.now() - started;
        for (const probe of probes) {
          probe.analyser.getFloatFrequencyData(probe.buffer);
          let best = 0;
          let sum = 0;
          for (let i = 1; i < probe.buffer.length; i++) {
            const v = probe.buffer[i] ?? -Infinity;
            if (v > (probe.buffer[best] ?? -Infinity)) best = i;
            sum += Number.isFinite(v) ? v : -160;
          }
          const peak = probe.buffer[best] ?? -160;
          probe.samples.push({
            atMs,
            hz: best * binHz,
            db: Number.isFinite(peak) ? peak : -160,
            floorDb: sum / Math.max(1, probe.buffer.length - 1),
          });
        }
      }, intervalMs);

      w.__toneProbe = { probes: probes as unknown as { id: string; samples: unknown[] }[] };
      return probes.length;
    },
    PROBE_INTERVAL_MS,
  ) as Promise<number>;
}

/** Everything the probe has collected so far, per inbound track. */
export async function readProbe(p: Participant): Promise<TrackObservations[]> {
  return p.widget().evaluate(() => {
    const w = window as unknown as {
      __toneProbe?: { probes: { id: string; samples: Observation[] }[] };
    };
    return (w.__toneProbe?.probes ?? []).map((probe) => ({
      trackId: probe.id,
      observations: probe.samples,
    }));
  }) as Promise<TrackObservations[]>;
}

/**
 * The mean level of the whole spectrum is not a floor when a strong tone is in it, but the
 * error is in the safe direction: it *raises* the apparent floor, so a peak has to stand out
 * further to count. The browser side therefore under-reports tones rather than inventing
 * them.
 */

/**
 * Where `audio_debug_dump.rs` writes, and the shape of the names it writes.
 *
 * `/tmp` literally, not `os.tmpdir()`: the Rust side has `/tmp` hard-coded, and this suite
 * runs inside a nix shell, which sets `TMPDIR` to a private directory. Reading the dumps from
 * `os.tmpdir()` found nothing at all and reported it as "Elementium decoded no inbound audio"
 * -- an absent measurement dressed up as a result.
 */
const DUMP_DIR = "/tmp";
const DUMP_NAME = /^elementium_audio_dump_(.+)_(\d+)hz_(\d+)ch\.f32le$/;

/**
 * Remove dumps from earlier runs.
 *
 * Not tidiness: `audio_debug_dump.rs` opens dump files with `create_new`, so a leftover file
 * makes the run it was supposed to record produce nothing at all, and the failure is an
 * absent measurement rather than an error.
 */
export async function clearDumps(): Promise<number> {
  const names = await readdir(DUMP_DIR);
  const ours = names.filter((n) => DUMP_NAME.test(n));
  for (const n of ours) await unlink(path.join(DUMP_DIR, n)).catch(() => undefined);
  return ours.length;
}

/** One of Elementium's dumps, measured. */
export interface DumpMeasurement {
  /** `capture-raw`, `capture-encoder-in`, `capture-loopback`, or `{pc_id}-{mid}`. */
  key: string;
  sampleRate: number;
  channels: number;
  seconds: number;
  observations: Observation[];
}

/** Every dump Elementium has written, put through the same measurement as the far end. */
export async function readDumps(): Promise<DumpMeasurement[]> {
  const out: DumpMeasurement[] = [];
  for (const name of await readdir(DUMP_DIR)) {
    const match = DUMP_NAME.exec(name);
    if (!match) continue;
    const [, key, rate, channels] = match;
    const sampleRate = Number(rate);
    const ch = Number(channels);
    const raw = await readFile(path.join(DUMP_DIR, name));
    // Interleaved f32; folded to mono because the ladder is mono and a channel that was
    // dropped somewhere would show as half the amplitude, not as a missing tone.
    const frames = Math.floor(raw.byteLength / 4 / Math.max(1, ch));
    const mono = new Float32Array(frames);
    const view = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
    for (let i = 0; i < frames; i++) {
      let sum = 0;
      for (let c = 0; c < ch; c++) sum += view.getFloat32((i * ch + c) * 4, true);
      mono[i] = sum / Math.max(1, ch);
    }
    out.push({
      key: key ?? "?",
      sampleRate,
      channels: ch,
      seconds: frames / Math.max(1, sampleRate),
      observations: observePcm(mono, sampleRate, ALL_TONES),
    });
  }
  return out;
}

/** Which ladder a dump or a track carries, judged only by what is in it. */
export const whichLadder = (observations: readonly Observation[]) =>
  identify(observations, Object.values(LADDERS));
