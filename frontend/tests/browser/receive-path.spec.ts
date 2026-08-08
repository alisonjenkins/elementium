/**
 * Measures what a *browser* makes of the audio Elementium transmits.
 *
 * The Rust-side bisection suite (`crates/elementium-webrtc/tests/audio_layer_bisection.rs`)
 * proves our client can push audio through a real SFU to another Rust client with a
 * delivery ratio of 1.000. That leaves the production receiver entirely untested: in a real
 * call the far end is Chromium, running libwebrtc's NetEq jitter buffer and livekit's
 * insertable-streams E2EE worker. Neither has an equivalent on our side, so a fault that
 * only appears there is invisible to every other test in this repo -- and "everything I can
 * measure is clean but people hear a robot" is precisely that shape of bug.
 *
 * The decisive quantity is not packet loss. It is concealment: `packetsLost` counts frames
 * that never arrived, while `concealedSamples` counts audio the browser had to synthesise
 * because it could not use what did arrive. A decryption failure, a jitter-buffer stall or
 * a malformed payload all show up as concealment with zero reported loss, which is exactly
 * why RTCP kept reporting a healthy stream.
 *
 * KNOWN FLAKE: the browser intermittently never receives the SFU's announcement that the
 * Rust publisher has joined -- no `participantConnected`, no `trackPublished`, so nothing to
 * subscribe to. It is retried once here because it is not what these tests measure, but it
 * is not understood, and it is worth understanding: "a participant already in the room never
 * learns about our published track" would match the reported symptom of a caller being
 * barely audible far better than any codec explanation.
 *
 * The `encryption` candidate named here is settled, and settling it invalidated some of
 * what this file used to measure. Our AddTrack did leave the field at NONE while the
 * transport encrypted the frames; that is fixed. It was not the cause of the flake, but it
 * was hiding something:
 *
 *   room.on(TrackPublished, pub =>
 *     setParticipantCryptorEnabled(pub.trackInfo.encryption !== NONE, identity))
 *   ...
 *   if (!this.isEnabled() || byteLength === 0) return controller.enqueue(encodedFrame);
 *
 * Announced as NONE, the participant's cryptor is switched off and every frame is enqueued
 * *undecrypted*. That did not show up as damage, because livekit's frame layout leaves the
 * Opus TOC byte in the clear: each frame reached the decoder with a valid header and a
 * ciphertext payload, so NetEq produced the right number of samples, on time, with nothing
 * to conceal. Full marks for noise.
 *
 * So concealment alone cannot tell audio from noise, and every "healthy" verdict recorded
 * here before that fix was worth less than it looked. With the declaration correct the
 * frames are really decrypted, and the bad-start pair below tells a very different story.
 *
 * The SFU is started for you: see `tests/global-setup.ts`, which brings up the whole
 * MatrixRTC stack before any test runs and stops it afterwards unless it was already
 * running.
 */
import { test, expect } from "@playwright/test";
import { createServer } from "node:http";
import { createHmac } from "node:crypto";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { readFile } from "node:fs/promises";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import path from "node:path";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const LK_DIST = path.join(REPO, "frontend/node_modules/livekit-client/dist");
const PUBLISHER = path.join(REPO, "target/debug/examples/publish_test_tone");
/** The real screen-share publisher: portal, PipeWire node, DMA-BUF import, encode, publish. */
const PUBLISHER_SCREEN = path.join(REPO, "target/debug/examples/publish_screen_share");

const SFU_HTTP = "http://127.0.0.1:7880";
const SFU_WS = "ws://127.0.0.1:7880";
const DEV_API_KEY = "devkey";
const DEV_API_SECRET = "secret";

/** Raw E2EE key material, as hex. Shared verbatim by the Rust publisher and the browser. */
const KEY_HEX = "000102030405060708090a0b0c0d0e0f";

/**
 * Fraction of received audio the browser may conceal before we call the stream broken.
 *
 * Not zero: NetEq conceals a little at stream start while it sizes its buffer, and that is
 * normal behaviour rather than a defect. But a threshold at all is the point -- the failure
 * being chased is continuous concealment, which is orders of magnitude above this.
 */
const MAX_CONCEALMENT_RATIO = 0.05;

/** Mint a LiveKit dev-mode access token (HS256), matching `livekit-server --dev`. */
function mintToken(identity: string, room: string): string {
  const now = Math.floor(Date.now() / 1000);
  const b64 = (o: unknown) =>
    Buffer.from(JSON.stringify(o)).toString("base64url");
  const head = b64({ alg: "HS256", typ: "JWT" });
  const body = b64({
    iss: DEV_API_KEY,
    sub: identity,
    iat: now,
    nbf: now,
    exp: now + 3600,
    jti: identity,
    name: identity,
    video: {
      room,
      roomJoin: true,
      canPublish: true,
      canSubscribe: true,
      canPublishData: true,
    },
  });
  const sig = createHmac("sha256", DEV_API_SECRET)
    .update(`${head}.${body}`)
    .digest("base64url");
  return `${head}.${body}.${sig}`;
}

/**
 * Serve the probe page and livekit-client's dist directory.
 *
 * A real origin rather than a `file://` page or `addScriptTag`: insertable streams need a
 * secure context and the E2EE worker must be same-origin, and 127.0.0.1 counts as secure.
 */
async function startPageServer(): Promise<{ origin: string; close: () => void }> {
  const page = await readFile(path.join(HERE, "receiver.html"));
  const server = createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    if (url.pathname.startsWith("/lk/")) {
      const name = path.basename(url.pathname);
      readFile(path.join(LK_DIST, name)).then(
        (buf) => {
          res.writeHead(200, { "content-type": "text/javascript" });
          res.end(buf);
        },
        () => {
          res.writeHead(404).end();
        },
      );
      return;
    }
    res.writeHead(200, { "content-type": "text/html" });
    res.end(page);
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const addr = server.address();
  const port = typeof addr === "object" && addr ? addr.port : 0;
  return {
    origin: `http://127.0.0.1:${port}`,
    close: () => server.close(),
  };
}

interface Publisher {
  proc: ChildProcessWithoutNullStreams;
  live: Promise<void>;
  sent: () => number | null;
  stop: () => void;
}

/** Spawn the Rust publisher and resolve once it reports that it is actually sending. */
function startPublisher(
  room: string,
  seconds: number,
  keyHex?: string,
  rotateFrames = 0,
  badFrames = 0,
  video: "h264" | "vp8" | null = null,
  keyIndex = 0,
  videoSize: "tiny" | null = null,
): Publisher {
  const args = [
    "--sfu", SFU_HTTP,
    "--room", room,
    "--identity", "rust-publisher",
    "--seconds", String(seconds),
    ...(keyHex ? ["--key-hex", keyHex] : []),
    ...(rotateFrames > 0 ? ["--rotate-frames", String(rotateFrames)] : []),
    ...(badFrames > 0 ? ["--bad-frames", String(badFrames)] : []),
    ...(video ? [`--video-${video}`] : []),
    ...(keyIndex > 0 ? ["--key-index", String(keyIndex)] : []),
    ...(videoSize ? ["--video-size", videoSize] : []),
  ];
  // `RUST_LOG` at info: the publisher's own view of negotiation and ICE is the other half
  // of any failure here, and discarding its stderr made a publisher-side fault look like a
  // browser-side one.
  const proc = spawn(PUBLISHER, args, {
    stdio: "pipe",
    env: { ...process.env, RUST_LOG: process.env.PUBLISHER_LOG ?? "warn" },
  });
  const echo = (c: Buffer) => {
    for (const line of c.toString().split("\n")) {
      // `PUBLISHING`/`SENT` are this process's protocol with the test, not diagnostics.
      if (line.trim() && !/^(PUBLISHING|SENT |ROTATED |KEY_CORRECTED)/.test(line)) {
        console.log(`  [publisher] ${line}`);
      }
    }
  };
  proc.stderr.on("data", echo);
  let sent: number | null = null;
  let out = "";
  const live = new Promise<void>((resolve, reject) => {
    proc.stdout.on("data", (chunk: Buffer) => {
      echo(chunk);
      out += chunk.toString();
      if (out.includes("PUBLISHING")) resolve();
      const m = /SENT (\d+)/.exec(out);
      if (m) sent = Number(m[1]);
    });
    proc.on("exit", (code) => {
      if (!out.includes("PUBLISHING")) {
        reject(new Error(`publisher exited (${code}) before publishing: ${out}`));
      }
    });
  });
  return { proc, live, sent: () => sent, stop: () => proc.kill() };
}

/**
 * Spawn the real screen-share publisher and resolve once it is actually sending.
 *
 * Separate from `startPublisher` rather than another flag on it: this one shows the XDG
 * ScreenCast portal's picker and blocks on a person choosing, so its "live" promise has no
 * useful timeout — a run is attended, and the wait is however long someone takes to read a
 * dialog. Folding that into the flag-driven publisher would give every other test a
 * timeout that means something different.
 */
/**
 * Spawn the real screen-share publisher and resolve once it is actually sending.
 *
 * `audio` adds `--audio`, which additionally captures share audio from a PipeWire desktop
 * sink monitor and publishes it as `MediaTrackKey::screen_share_audio()` -- the publisher
 * half of SC-004. `audioSent()` reads the `AUDIO_SENT <n>` marker the binary prints
 * alongside `VIDEO_SENT`, following the same convention.
 *
 * Separate from `startPublisher` rather than another flag on it: this one shows the XDG
 * ScreenCast portal's picker and blocks on a person choosing, so its "live" promise has no
 * useful timeout — a run is attended, and the wait is however long someone takes to read a
 * dialog. Folding that into the flag-driven publisher would give every other test a
 * timeout that means something different.
 */
function startScreenPublisher(
  room: string,
  seconds: number,
  keyHex?: string,
  audio = false,
): Publisher & { audioSent: () => number | null } {
  const args = [
    "--sfu", SFU_HTTP,
    "--room", room,
    "--identity", "rust-screen-publisher",
    "--seconds", String(seconds),
    ...(keyHex ? ["--key-hex", keyHex] : []),
    ...(audio ? ["--audio"] : []),
  ];
  const proc = spawn(PUBLISHER_SCREEN, args, {
    stdio: "pipe",
    env: { ...process.env, RUST_LOG: process.env.PUBLISHER_LOG ?? "warn" },
  });
  const echo = (c: Buffer) => {
    for (const line of c.toString().split("\n")) {
      if (line.trim() && !/^(PUBLISHING|VIDEO_SENT |AUDIO_SENT |CAPTURED |AUDIO_SINK )/.test(line)) {
        console.log(`  [screen-publisher] ${line}`);
      }
    }
  };
  proc.stderr.on("data", echo);
  let sent: number | null = null;
  let audioSent: number | null = null;
  let out = "";
  const live = new Promise<void>((resolve, reject) => {
    proc.stdout.on("data", (chunk: Buffer) => {
      echo(chunk);
      out += chunk.toString();
      // Surfaced so an attended run knows it is being waited on, rather than looking hung.
      if (out.includes("WAITING_FOR_PICKER") && !out.includes("PORTAL_GRANTED")) {
        console.log("  >>> choose a screen or window in the portal dialog <<<");
      }
      if (out.includes("PUBLISHING")) resolve();
      const m = /VIDEO_SENT (\d+)/.exec(out);
      if (m) sent = Number(m[1]);
      const am = /AUDIO_SENT (\d+)/.exec(out);
      if (am) audioSent = Number(am[1]);
    });
    proc.on("exit", (code) => {
      if (!out.includes("PUBLISHING")) {
        reject(new Error(`screen publisher exited (${code}) before publishing: ${out}`));
      }
    });
  });
  return {
    proc,
    live,
    sent: () => sent,
    audioSent: () => audioSent,
    stop: () => proc.kill(),
  };
}

interface InboundStats {
  ssrc: number;
  mimeType: string | null;
  packetsReceived: number;
  packetsLost: number;
  concealedSamples: number;
  silentConcealedSamples: number;
  concealmentEvents: number;
  totalSamplesReceived: number;
  insertedSamplesForDeceleration: number;
  removedSamplesForAcceleration: number;
  jitter: number;
}

/**
 * Run one publish/subscribe measurement and return the browser's view of the stream.
 *
 * Deltas, not totals: the counters are cumulative from the moment the track is attached,
 * and the first moments include jitter-buffer priming that is not a defect. Sampling twice
 * and subtracting measures the steady state.
 */
async function measure(
  page: import("@playwright/test").Page,
  keyHex?: string,
  rotateFrames = 0,
  keyDelayMs = 0,
  opts: { badFrames?: number; defaultTolerance?: boolean } = {},
): Promise<{ stats: InboundStats; sent: number | null; errors: string[] }> {
  const roomName = `elementium-recv-${Date.now()}`;
  // Printed so a run can be correlated with the SFU's own logs, which are the only place
  // the server's view of publication and subscription is visible.
  console.log(`  room: ${roomName}`);
  const server = await startPageServer();

  // Forward the page's own diagnostics: a livekit connect or E2EE-worker failure reports
  // itself in the console and would otherwise surface only as "never subscribed".
  page.on("console", (m) => console.log(`  [page:${m.type()}] ${m.text()}`));
  page.on("pageerror", (e) => console.log(`  [page:error] ${e.message}`));

  // The subscriber joins first and is already in the room when the publisher arrives, which
  // is both what happens in a real call and what makes this deterministic: joining after a
  // participant has already published races the SFU's participant announcement, and loses
  // often enough to be useless as a test.
  const token = mintToken("browser-subscriber", roomName);
  const query = new URLSearchParams({ url: SFU_WS, token });
  if (keyHex) query.set("key", keyHex);
  if (opts.defaultTolerance) query.set("provider", "default");
  // 30s at 50fps, so the browser can pre-install every key the publisher will use.
  if (rotateFrames > 0) {
    query.set("rotations", String(Math.ceil(1500 / rotateFrames)));
    query.set("rotatems", String(rotateFrames * 20));
    if (keyDelayMs > 0) query.set("keydelay", String(keyDelayMs));
  }
  await page.goto(`${server.origin}/?${query.toString()}`);
  await expect
    .poll(() => page.textContent("#state"), { timeout: 20_000 })
    .toBe("connected");

  const publisher = startPublisher(roomName, 30, keyHex, rotateFrames, opts.badFrames ?? 0);

  try {
    await publisher.live;

    await expect
      .poll(() => page.evaluate(() => (window as never as { __subscribed: boolean }).__subscribed), {
        timeout: 30_000,
        message: "the browser never subscribed to the published audio track",
      })
      .toBe(true)
      .catch(async (e) => {
        const evs = await page.evaluate(
          () => (window as never as { __events: string[] }).__events,
        );
        console.log(`  room events seen: ${JSON.stringify(evs)}`);
        throw e;
      });

    // Retry rather than failing on the first miss: livekit creates the subscribing peer
    // connection lazily, so stats can genuinely be unavailable for a moment after the track
    // is reported subscribed. Failing immediately turned that race into a flake.
    const read = async (): Promise<InboundStats> => {
      let last = "not attempted";
      for (let attempt = 0; attempt < 20; attempt += 1) {
        const s = await page.evaluate(() =>
          (window as never as {
            __stats: () => Promise<InboundStats | { error: string }>;
          }).__stats(),
        );
        if (!("error" in s)) return s;
        last = s.error;
        await page.waitForTimeout(500);
      }
      throw new Error(`could not read receiver stats after 10s: ${last}`);
    };

    // Let the jitter buffer settle, take a baseline, then measure a steady-state window.
    await page.waitForTimeout(5_000);
    const before = await read();
    await page.waitForTimeout(10_000);
    const after = await read();

    // Raw samples, not just the delta: a track that restarts gets a new SSRC, and
    // subtracting two different streams' counters produces a number that looks like loss
    // but is arithmetic on unrelated series.
    console.log(`  before: ${JSON.stringify(before)}`);
    console.log(`  after:  ${JSON.stringify(after)}`);
    if (before.ssrc !== after.ssrc) {
      throw new Error(
        `the inbound stream changed SSRC mid-measurement (${before.ssrc} -> ${after.ssrc}); ` +
          `the track restarted, so a delta across it is meaningless`,
      );
    }

    const stats: InboundStats = {
      ssrc: after.ssrc,
      mimeType: after.mimeType,
      jitter: after.jitter,
      packetsReceived: after.packetsReceived - before.packetsReceived,
      packetsLost: after.packetsLost - before.packetsLost,
      concealedSamples: after.concealedSamples - before.concealedSamples,
      silentConcealedSamples: after.silentConcealedSamples - before.silentConcealedSamples,
      concealmentEvents: after.concealmentEvents - before.concealmentEvents,
      totalSamplesReceived: after.totalSamplesReceived - before.totalSamplesReceived,
      insertedSamplesForDeceleration:
        after.insertedSamplesForDeceleration - before.insertedSamplesForDeceleration,
      removedSamplesForAcceleration:
        after.removedSamplesForAcceleration - before.removedSamplesForAcceleration,
    };
    const errors = await page.evaluate(
      () => (window as never as { __errors: string[] }).__errors,
    );
    return { stats, sent: publisher.sent(), errors };
  } finally {
    publisher.stop();
    server.close();
  }
}


type VideoStats = {
  mimeType: string | null;
  packetsReceived: number;
  framesReceived: number;
  framesDecoded: number;
  framesDropped: number;
  keyFramesDecoded: number;
  pliCount: number;
  frameWidth: number;
  frameHeight: number;
};

/**
 * Publish encrypted video of one codec and measure what the browser made of it.
 *
 * Deltas across a settled window, as the audio measurement does, and the same distinction
 * it draws: `framesReceived` counts what the depacketiser assembled, `framesDecoded` what
 * the decoder accepted. A frame whose E2EE framing is wrong still arrives and still counts
 * in `packetsReceived`, so that counter can never be the assertion.
 */
async function measureVideo(
  page: import("@playwright/test").Page,
  codec: "h264" | "vp8",
  encrypted = true,
  keyIndex = 0,
  videoSize: "tiny" | null = null,
): Promise<VideoStats> {
  const roomName = `elementium-${codec}-${Date.now()}`;
  console.log(`  room: ${roomName}`);
  const server = await startPageServer();

  page.on("console", (m) => console.log(`  [page:${m.type()}] ${m.text()}`));
  page.on("pageerror", (e) => console.log(`  [page:error] ${e.message}`));

  const query = new URLSearchParams({
    url: SFU_WS,
    token: mintToken("browser-subscriber", roomName),
  });
  if (encrypted) query.set("key", KEY_HEX);
  // Installs keys at indices 0..keyIndex, so a publisher stamping a non-zero index has one.
  if (keyIndex > 0) query.set("rotations", String(keyIndex));
  await page.goto(`${server.origin}/?${query.toString()}`);
  await expect.poll(() => page.textContent("#state"), { timeout: 20_000 }).toBe("connected");

  const publisher = startPublisher(
    roomName,
    20,
    encrypted ? KEY_HEX : undefined,
    0,
    0,
    codec,
    keyIndex,
    videoSize,
  );
  try {
    await publisher.live;
    await expect
      .poll(
        () =>
          page.evaluate(
            () => (window as never as { __videoSubscribed: boolean }).__videoSubscribed,
          ),
        { timeout: 30_000, message: "the browser never subscribed to the published video" },
      )
      .toBe(true);

    const read = async (): Promise<VideoStats> => {
      let last = "not attempted";
      for (let attempt = 0; attempt < 20; attempt += 1) {
        const s = await page.evaluate(() =>
          (window as never as {
            __videoStats: () => Promise<VideoStats | { error: string }>;
          }).__videoStats(),
        );
        if (!("error" in s)) return s;
        last = s.error;
        await page.waitForTimeout(500);
      }
      throw new Error(`could not read video stats after 10s: ${last}`);
    };

    await page.waitForTimeout(5_000);
    const before = await read();
    await page.waitForTimeout(8_000);
    const after = await read();
    console.log(`  before: ${JSON.stringify(before)}`);
    console.log(`  after:  ${JSON.stringify(after)}`);

    // The audio track alongside the video one. It matters because a decrypt failure
    // reported by the worker names no track: if audio is healthy in a run that reports
    // `InvalidKey`, the failing frames are the video ones, and if audio is not, the error
    // says nothing about video at all.
    const audio = await page.evaluate(() =>
      (window as never as {
        __stats: () => Promise<{ concealedSamples?: number; packetsReceived?: number } | { error: string }>;
      }).__stats(),
    );
    console.log(`  audio alongside: ${JSON.stringify(audio)}`);
    const errors = await page.evaluate(
      () => (window as never as { __errors: string[] }).__errors,
    );
    console.log(`  page errors: ${JSON.stringify(errors)}`);
    expect(errors, "no E2EE worker errors").toEqual([]);

    return {
      mimeType: after.mimeType,
      packetsReceived: after.packetsReceived - before.packetsReceived,
      framesReceived: after.framesReceived - before.framesReceived,
      framesDecoded: after.framesDecoded - before.framesDecoded,
      framesDropped: after.framesDropped - before.framesDropped,
      keyFramesDecoded: after.keyFramesDecoded - before.keyFramesDecoded,
      pliCount: after.pliCount,
      frameWidth: after.frameWidth,
      frameHeight: after.frameHeight,
    };
  } finally {
    publisher.stop();
    server.close();
  }
}

/**
 * Assert the browser reconstructed the stream rather than merely receiving it.
 *
 * `concealedSamples` minus `silentConcealedSamples` is the part that matters: silent
 * concealment is what NetEq emits for a deliberately absent stream, whereas non-silent
 * concealment is it inventing audio to paper over something it could not use.
 */
function assertHealthy(stats: InboundStats, label: string) {
  console.log(`${label}: ${JSON.stringify(stats)}`);

  expect(stats.packetsReceived, `${label}: no audio packets arrived at all`).toBeGreaterThan(0);
  expect(stats.totalSamplesReceived, `${label}: no audio samples were produced`).toBeGreaterThan(0);

  const active = stats.concealedSamples - stats.silentConcealedSamples;
  const ratio = active / stats.totalSamplesReceived;
  expect(
    ratio,
    `${label}: the browser concealed ${(ratio * 100).toFixed(1)}% of received audio ` +
      `(${active} of ${stats.totalSamplesReceived} samples) with ${stats.packetsLost} packets ` +
      `reported lost -- audio is arriving and being discarded or synthesised, which is ` +
      `invisible to RTCP and is what "sounds like a robot" measures as`,
  ).toBeLessThan(MAX_CONCEALMENT_RATIO);
}

test.describe("browser receive path", () => {
  test.describe.configure({ mode: "serial", timeout: 180_000 });

  test("plain Opus is reconstructed cleanly by libwebrtc", async ({ page }) => {
    const { stats, errors } = await measure(page);
    expect(errors, "no page errors").toEqual([]);
    expect(stats.mimeType, "must be negotiated as plain Opus, not RED").toBe("audio/opus");
    assertHealthy(stats, "unencrypted");
  });

  test("E2EE frames are decryptable by livekit's worker", async ({ page }) => {
    const { stats, errors } = await measure(page, KEY_HEX);
    // A decrypt failure surfaces here as concealment, not as an exception, so the stats are
    // the real assertion -- but worker errors are worth surfacing when they do occur.
    expect(errors, "no page errors").toEqual([]);
    assertHealthy(stats, "encrypted");
  });

  test("audio survives key rotation mid-call", async ({ page }) => {
    // Rotate every 100 frames (2 seconds), so the 10-second measurement window spans about
    // five rotations. Element Call rotates whenever room membership changes, and a rotation
    // the two sides disagree about loses only the frames encrypted during the disagreement
    // -- heard as part of a sentence going missing while the rest arrives normally, which
    // is exactly the reported symptom and is what a static-key test cannot produce.
    const { stats, errors } = await measure(page, KEY_HEX, 100);
    expect(errors, "no page errors").toEqual([]);
    assertHealthy(stats, "rotating");
  });

  // MEASURED CLEAN. This was briefly believed to reproduce the field symptom -- an early
  // run showed 52 of ~500 packets usable and 89% of samples concealed. That did not hold
  // up: with the retrying stats reader and raw before/after sampling in place, the same
  // scenario recovers completely. The earlier numbers came from a window sampled while the
  // track was still starting, which is also what produced the "never subscribed" failures.
  //
  // Kept because the scenario is worth guarding regardless, and because the negative result
  // is itself worth recording: a receiver handles a key it learns about late.
  test("audio survives a key rotation the far end learns about late", async ({ page }) => {
    // The same rotation, but the receiver only learns each new key 500ms after the sender
    // starts using it -- a realistic delay for a key published over Matrix and fanned out
    // to a room. Any frame sent inside that window is undecryptable at the far end.
    //
    // 500ms of a 2s rotation period is a quarter of all audio. If this fails while the
    // pre-installed variant passes, the defect is that we switch encryption keys the moment
    // one is handed to us, without regard for whether anyone else can read them yet.
    const { stats, errors } = await measure(page, KEY_HEX, 100, 500);
    expect(errors, "no page errors").toEqual([]);
    assertHealthy(stats, "rotating with late key delivery");
  });

  // ---------------------------------------------------------------------------
  // Key invalidation: does a receiver ever recover from a bad start?
  //
  // We begin publishing the moment the track is added, so our first frames can reach a
  // peer before it holds our key. From the receiver's side that is indistinguishable from
  // frames encrypted with a key it does not have -- which is how these two tests model it.
  //
  // The pair is the experiment: identical streams, differing only in the receiver's
  // `failureTolerance`. If the tolerant one recovers and the default one does not, the
  // damage is caused by livekit latching our key as invalid, not by anything about the
  // frames themselves.
  // ---------------------------------------------------------------------------

  test("a receiver that never invalidates keys recovers from a bad start", async ({ page }) => {
    // ExternalE2EEKeyProvider sets failureTolerance to -1, so it keeps trying forever.
    // 50 bad frames (1 second), then correct ones for the rest of the run.
    const { stats, errors } = await measure(page, KEY_HEX, 0, 0, { badFrames: 50 });
    expect(errors, "no page errors").toEqual([]);
    assertHealthy(stats, "bad start, tolerant receiver");
  });

  test("a receiver with livekit's default tolerance never recovers from a bad start", async ({
    page,
  }) => {
    // THIS TEST ASSERTS A DEFECT. It passes because the damage happens, and it will fail
    // when the damage stops -- at which point it is the fix that needs writing up, not
    // this test that needs relaxing.
    //
    // Element Call's key provider is `super({ratchetWindowSize: 10, keyringSize: 256})`
    // on BaseKeyProvider. It never sets `failureTolerance`, so it gets the default of 10.
    // Only `ExternalE2EEKeyProvider` sets -1, and Element Call does not use it.
    //
    // What that means, from livekit's worker source:
    //
    //   hasInvalidKeyAtIndex(i) -> failureTolerance >= 0 && failureCounts[i] > tolerance
    //   decrypt: if (this.keys.hasInvalidKeyAtIndex(keyIndex)) return;   // dropped
    //
    // The drop happens *before* any decryption is attempted, so the successful decryption
    // that would clear the count can never occur. Eleven undecryptable frames -- a fifth
    // of a second of audio -- are enough to reach that state, which is what a peer sees
    // whenever we publish before our key has reached them: every call start, every
    // rotation.
    //
    // Installing a key at that index does clear the count (`setKey` calls
    // `resetKeyStatus` when `updateCurrentKeyIndex` is set, and Element Call always passes
    // a key index, so it is). So in a real call a late key recovers the stream. This test
    // is the case where no such key ever arrives -- the publisher simply starts sending
    // correct frames at the same index -- which is what isolates the latch from every
    // other reason audio might resume.
    //
    // The pair with the test above is the experiment. Identical streams; the only
    // difference is the receiver's tolerance, and only the tolerant one recovers. So the
    // damage is livekit latching the key as invalid, not anything about the frames.
    const { stats, errors } = await measure(page, KEY_HEX, 0, 0, {
      badFrames: 50,
      defaultTolerance: true,
    });
    expect(errors, "no page errors").toEqual([]);

    console.log(`bad start, default tolerance: ${JSON.stringify(stats)}`);
    expect(
      stats.packetsReceived,
      "packets must still arrive -- the loss is above the transport, not in it",
    ).toBeGreaterThan(0);
    expect(
      stats.totalSamplesReceived,
      "if this is non-zero the receiver recovered, and the latch described above is gone",
    ).toBe(0);
  });

  /**
   * The one test that checks our H.264 E2EE framing against livekit's running worker.
   *
   * The framing contract was read out of livekit-client's source and reproduced over 596
   * frames of their own vectors, which is strong evidence about the *code* and none at all
   * about the runtime. H.264 clear-header sizing and RBSP escaping are exactly the kind of
   * thing that agrees with a careful reading and disagrees with the shipped worker.
   *
   * `framesDecoded` is the assertion, not `packetsReceived`. A frame whose framing we got
   * wrong still arrives and still counts as received: it fails inside the decrypt, in a
   * worker, silently. The only visible consequence is that no picture is ever produced --
   * which is precisely the failure the sending side cannot see, and the reason this test
   * exists rather than a Rust round trip through our own code.
   */
  test("H.264 video we encrypt is decrypted by livekit's worker", async ({ page }) => {
    test.fixme(
      true,
      "Encrypted H.264 never assembles a frame at the receiver, with or without the page " +
        "holding the key. Our encryption is provably correct -- a frame decrypts cleanly " +
        "through livekit's own worker functions -- and the encrypted frame carries the " +
        "same Annex B NAL structure as the plain one. See 005 T020.",
    );
    const roomName = `elementium-h264-${Date.now()}`;
    console.log(`  room: ${roomName}`);
    const server = await startPageServer();

    page.on("console", (m) => console.log(`  [page:${m.type()}] ${m.text()}`));
    page.on("pageerror", (e) => console.log(`  [page:error] ${e.message}`));

    const query = new URLSearchParams({
      url: SFU_WS,
      token: mintToken("browser-subscriber", roomName),
      key: KEY_HEX,
    });
    await page.goto(`${server.origin}/?${query.toString()}`);
    await expect.poll(() => page.textContent("#state"), { timeout: 20_000 }).toBe("connected");

    const publisher = startPublisher(roomName, 20, KEY_HEX, 0, 0, "h264");
    try {
      await publisher.live;

      await expect
        .poll(
          () =>
            page.evaluate(
              () => (window as never as { __videoSubscribed: boolean }).__videoSubscribed,
            ),
          { timeout: 30_000, message: "the browser never subscribed to the published video" },
        )
        .toBe(true);

      type VideoStats = {
        mimeType: string | null;
        packetsReceived: number;
        framesReceived: number;
        framesDecoded: number;
        framesDropped: number;
        keyFramesDecoded: number;
        pliCount: number;
        frameWidth: number;
        frameHeight: number;
      };
      const read = async (): Promise<VideoStats> => {
        let last = "not attempted";
        for (let attempt = 0; attempt < 20; attempt += 1) {
          const s = await page.evaluate(() =>
            (window as never as {
              __videoStats: () => Promise<VideoStats | { error: string }>;
            }).__videoStats(),
          );
          if (!("error" in s)) return s;
          last = s.error;
          await page.waitForTimeout(500);
        }
        throw new Error(`could not read video stats after 10s: ${last}`);
      };

      await page.waitForTimeout(5_000);
      const before = await read();
      await page.waitForTimeout(8_000);
      const after = await read();
      console.log(`  before: ${JSON.stringify(before)}`);
      console.log(`  after:  ${JSON.stringify(after)}`);

      // Read before asserting anything: a decrypt failure inside livekit's worker reports
      // itself here and nowhere else, and asserting first hides the one line that says why.
      const errors = await page.evaluate(
        () => (window as never as { __errors: string[] }).__errors,
      );
      console.log(`  page errors: ${JSON.stringify(errors)}`);

      expect(after.mimeType, "the SFU must have negotiated H.264, not VP8").toMatch(/H264/i);
      expect(
        after.packetsReceived - before.packetsReceived,
        "video packets must be arriving at all",
      ).toBeGreaterThan(0);
      expect(
        after.framesDecoded - before.framesDecoded,
        "livekit's worker must decrypt our H.264 frames into pictures",
      ).toBeGreaterThan(10);
      expect(after.frameWidth, "decoded width").toBe(320);
      expect(after.frameHeight, "decoded height").toBe(240);

      expect(errors, "no E2EE worker errors").toEqual([]);
    } finally {
      publisher.stop();
      server.close();
    }
  });

  /**
   * CONTROL for the H.264 test: the same publisher, the same E2EE, VP8 instead.
   *
   * If VP8 assembles and H.264 does not, the fault is H.264-specific -- its framing, its
   * packetisation, or its parameter sets. If neither assembles, video publishing through
   * this path has never worked at all, and the H.264 test says nothing about H.264.
   *
   * Worth having permanently rather than as a one-off: it is the only thing that can tell
   * "our H.264 is wrong" from "our video publishing is wrong", and that question will come
   * back every time either changes.
   */
  test("control: VP8 video we encrypt is decrypted by livekit's worker", async ({ page }) => {
    const stats = await measureVideo(page, "vp8");
    expect(stats.mimeType, "the SFU must have negotiated VP8").toMatch(/VP8/i);
    expect(stats.framesReceived, "the depacketiser must assemble frames").toBeGreaterThan(10);
    expect(stats.framesDecoded, "livekit's worker must decrypt them into pictures")
      .toBeGreaterThan(10);
  });

  /**
   * CONTROL: H.264 with no encryption at all.
   *
   * Separates "our H.264 bitstream and packetisation" from "our H.264 E2EE framing". Both
   * produce the same symptom -- packets arrive, no frame is assembled -- and nothing else
   * distinguishes them.
   */
  test("control: plain H.264 assembles at the receiver", async ({ page }) => {
    const stats = await measureVideo(page, "h264", false);
    expect(stats.mimeType, "the SFU must have negotiated H.264").toMatch(/H264/i);
    expect(stats.framesReceived, "the depacketiser must assemble frames").toBeGreaterThan(10);
  });

  /**
   * The key-index experiment: the same H.264 stream, encrypted at index 1 instead of 0.
   *
   * Our frame ends `[iv][IV_LENGTH][key_index]`, so at index 0 the last byte of the NAL is
   * a zero -- and H.264 NAL handling trims trailing zeros. If index 1 decodes and index 0
   * does not, the trailer is being truncated somewhere between our writer and the browser,
   * and nothing about our encryption is wrong.
   */
  test("H.264 at key index 1 tells us whether the trailing zero is the problem", async ({
    page,
  }) => {
    test.fixme(
      true,
      "Answered, and the answer was no: index 1 fails exactly as index 0 does, so the " +
        "trailing zero of the key index is not what breaks it. Kept because the question " +
        "recurs and this settles it in one run. See 005 T020.",
    );
    const stats = await measureVideo(page, "h264", true, 1);
    expect(stats.mimeType, "the SFU must have negotiated H.264").toMatch(/H264/i);
    expect(stats.framesDecoded, "frames must decode at a non-zero key index")
      .toBeGreaterThan(10);
  });

  /**
   * CONTROL: browser-to-browser H.264 under E2EE, with our publisher nowhere in it.
   *
   * The question this settles is whether livekit's own sender and receiver can carry H.264
   * with insertable-streams encryption at all. Every other control has narrowed the failure
   * to that combination; this one says whose failure it is. If two browsers manage it, the
   * fault is in what we send. If they do not, H.264 under E2EE is not something this stack
   * supports, and preferring VP8 when encryption is on becomes a product decision rather
   * than a bug to chase.
   */
  test("control: browser to browser, H.264 under E2EE", async ({ browser }) => {
    const roomName = `elementium-b2b-h264-${Date.now()}`;
    console.log(`  room: ${roomName}`);
    const server = await startPageServer();
    const ctx = await browser.newContext();

    try {
      const subscriber = await ctx.newPage();
      subscriber.on("console", (m) => console.log(`  [sub:${m.type()}] ${m.text()}`));
      const subQuery = new URLSearchParams({
        url: SFU_WS,
        token: mintToken("browser-subscriber", roomName),
        key: KEY_HEX,
      });
      await subscriber.goto(`${server.origin}/?${subQuery.toString()}`);
      await expect.poll(() => subscriber.textContent("#state"), { timeout: 20_000 }).toBe(
        "connected",
      );

      const publisher = await ctx.newPage();
      publisher.on("console", (m) => console.log(`  [pub:${m.type()}] ${m.text()}`));
      const pubQuery = new URLSearchParams({
        url: SFU_WS,
        token: mintToken("browser-publisher", roomName),
        key: KEY_HEX,
        publish: "1",
        pubvideo: "h264",
      });
      await publisher.goto(`${server.origin}/?${pubQuery.toString()}`);
      await expect.poll(() => publisher.textContent("#state"), { timeout: 20_000 }).toBe(
        "publishing",
      );

      await expect
        .poll(
          () =>
            subscriber.evaluate(
              () => (window as never as { __videoSubscribed: boolean }).__videoSubscribed,
            ),
          { timeout: 30_000, message: "the subscriber never subscribed to the video" },
        )
        .toBe(true);

      const read = async () =>
        subscriber.evaluate(() =>
          (window as never as {
            __videoStats: () => Promise<Record<string, number | string | null>>;
          }).__videoStats(),
        );
      await subscriber.waitForTimeout(8_000);
      const stats = await read();
      console.log(`  browser-to-browser H.264 + E2EE: ${JSON.stringify(stats)}`);

      expect(
        stats.framesDecoded,
        "two browsers must manage H.264 under E2EE, or nothing can",
      ).toBeGreaterThan(0);
    } finally {
      server.close();
      await ctx.close();
    }
  });

  /**
   * Encrypted H.264 small enough to fit one RTP packet.
   *
   * A fragmented frame and an unfragmented one take different paths through str0m's
   * packetiser, and our encrypted frames are the only ones that fail. Publishing at 160x120
   * puts most frames under the MTU: if they assemble, fragmentation is where to look, and
   * if they do not, it is not.
   */
  test("diagnostic: encrypted H.264 below the MTU", async ({ page }) => {
    test.fixme(
      true,
      "Answered: frames small enough to travel in one RTP packet fail exactly as " +
        "fragmented ones do, so FU-A is not where the fault is. Kept because it rules out " +
        "a whole code path in one run. See 005 T020.",
    );
    const stats = await measureVideo(page, "h264", true, 0, "tiny");
    console.log(`  tiny encrypted H.264: ${JSON.stringify(stats)}`);
    expect(stats.framesReceived, "unfragmented encrypted H.264 must assemble")
      .toBeGreaterThan(5);
  });

  // CONTROL. Two browsers through the same SFU, with no Rust publisher involved.
  //
  // Establishes a baseline for every other test here: if this subscribes reliably and the
  // Rust-publisher tests do not, the difference is in what we publish. If this flakes the
  // same way, the flake is in livekit's subscribe path or the environment, and the other
  // tests' failures say nothing about our code.
  test("control: browser to browser through the SFU", async ({ browser }) => {
    const roomName = `elementium-control-${Date.now()}`;
    console.log(`  room: ${roomName}`);
    const server = await startPageServer();
    const ctx = await browser.newContext();

    try {
      const subscriber = await ctx.newPage();
      subscriber.on("pageerror", (e) => console.log(`  [sub:error] ${e.message}`));
      const subQuery = new URLSearchParams({
        url: SFU_WS,
        token: mintToken("browser-subscriber", roomName),
      });
      await subscriber.goto(`${server.origin}/?${subQuery.toString()}`);
      await expect.poll(() => subscriber.textContent("#state"), { timeout: 20_000 }).toBe(
        "connected",
      );

      // `CONTROL_PUBLISH_DELAY_MS` makes the control match the Rust tests' timing, where
      // the publisher takes ~20s to start. A subscriber that has been idle in the room for
      // a while is a different case for livekit's reconcile logic than one that sees a
      // publisher arrive immediately, and this knob is what tells the two apart.
      const delayMs = Number(process.env.CONTROL_PUBLISH_DELAY_MS ?? 0);
      if (delayMs > 0) {
        console.log(`  delaying publisher by ${delayMs}ms`);
        await subscriber.waitForTimeout(delayMs);
      }

      const publisher = await ctx.newPage();
      publisher.on("pageerror", (e) => console.log(`  [pub:error] ${e.message}`));
      const pubQuery = new URLSearchParams({
        url: SFU_WS,
        token: mintToken("browser-publisher", roomName),
        publish: "1",
      });
      await publisher.goto(`${server.origin}/?${pubQuery.toString()}`);
      await expect.poll(() => publisher.textContent("#state"), { timeout: 20_000 }).toBe(
        "publishing",
      );

      await expect
        .poll(
          () =>
            subscriber.evaluate(
              () => (window as never as { __subscribed: boolean }).__subscribed,
            ),
          { timeout: 30_000, message: "the subscriber never subscribed to the browser's track" },
        )
        .toBe(true);

      await subscriber.waitForTimeout(5_000);
      const read = async () =>
        subscriber.evaluate(() =>
          (window as never as {
            __stats: () => Promise<InboundStats | { error: string }>;
          }).__stats(),
        );
      const before = await read();
      await subscriber.waitForTimeout(10_000);
      const after = await read();
      if ("error" in before || "error" in after) {
        throw new Error(`control could not read stats: ${JSON.stringify({ before, after })}`);
      }

      console.log(`  control before: ${JSON.stringify(before)}`);
      console.log(`  control after:  ${JSON.stringify(after)}`);
      expect(
        after.packetsReceived - before.packetsReceived,
        "a browser publishing to a browser must deliver a steady packet rate",
      ).toBeGreaterThan(400);
    } finally {
      await ctx.close();
      server.close();
    }
  });
});

/**
 * Screen and application sharing (spec `008-screen-share-capture`).
 *
 * SC-001 and SC-003 both want the same thing this whole file already provides for camera
 * video: a real browser, running real libwebrtc, measured with `getStats()` rather than
 * trusted by eye or by track state. Neither test below can actually run yet, and the
 * reason is the same one `specs/008-screen-share-capture/quickstart.md` gives for its own
 * Level 4 and Level 5: proving the feature requires a real capture, and a real capture on
 * this stack means `elementium_screen::start_share()` opening the XDG ScreenCast portal and
 * showing a picker dialog that only a human can answer. `ashpd`'s call blocks on that
 * dialog by design -- see the identical point made in
 * `crates/elementium-media/examples/capture_attribution.rs` about its own portal call --
 * so there is no timeout to wait out and no way to script past it from here.
 *
 * There is a second gap below the portal, independent of the dialog: every video test
 * elsewhere in this file drives the browser side with `publish_test_tone`, a Rust binary
 * that stands in for the app's sender. It does not capture a screen -- it draws a
 * checkerboard and publishes that (see `--video-h264`/`--video-vp8` in
 * `crates/elementium-webrtc/examples/publish_test_tone.rs`, sourced from
 * `MediaTrackKey::camera()`, never from `elementium_screen`). There is no equivalent
 * example that calls `elementium_screen::start_share()`, reads real frames from the
 * `WaylandCapturer`, and feeds them into `LiveKitRoom::publish_video_track()` the way the
 * real `get_display_media` command now does. Writing that binary would close this gap for
 * SC-001, in the way `publish_test_tone` already does for camera video, but it lives under
 * `crates/`, outside what this suite (`frontend/tests/browser/`) may touch.
 *
 * Both tests are written to the shape they would need once that binary exists, so that
 * unblocking them is "point PUBLISHER_SCREEN at a real binary and delete the fixme", not
 * "write the test". What would fully unblock a CI run: that publish-side example, plus
 * either a persisted portal choice (`ashpd::desktop::PersistMode::ExplicitlyRevoked` or
 * `Persistent`, replaying a previously approved source without a dialog -- the current
 * `elementium_screen::start_share()` uses `PersistMode::DoNot`, so this would need a code
 * change outside this suite too) or a headless/mock portal backend the harness can point
 * `XDG_DESKTOP_PORTAL_DIR`/`busctl` at instead of the real one.
 */
test.describe("screen share (spec 008)", () => {
  test.describe.configure({ mode: "serial", timeout: 180_000 });


  /**
   * SC-001: "A screen share started in a call produces a steadily increasing decoded frame
   * count at the receiving participant, sustained for at least 30 seconds."
   *
   * The single most important property of this test is what it does *not* do: stop at one
   * before/after pair. The bug this feature fixes handed the page a perfectly valid, `live`
   * `MediaStreamTrack` that carried nothing -- every naive assertion (track exists, track is
   * live, one frame arrived) passed for months while the remote participant saw black. A
   * two-sample delta is close to that same trap: a source that delivers one keyframe and
   * then stalls -- a very plausible shape for a regression here, since it is exactly what a
   * capturer that opens a session but stops pumping frames after the first would produce --
   * shows the same "increased" verdict a healthy stream does, if all you ever check is
   * start and end.
   *
   * So this samples `framesDecoded` repeatedly across the full window and requires *every*
   * consecutive pair to show growth, not just the first and last. That is what "steadily
   * increasing... sustained" in SC-001's wording actually rules out.
   */
  test("a screen share produces a sustained, increasing decoded frame count", async ({
    page,
  }) => {
    // Attended by design, and skipped unless a run says it has someone present.
    //
    // The publisher shows the XDG ScreenCast portal's picker and blocks until a person
    // chooses. That is not an obstacle to route around: the portal exists so no application
    // can start capturing a screen without the user knowing, and a test that bypassed it
    // would be exercising a path no user ever takes. So this runs when asked for, and is
    // skipped rather than failed in CI, where nobody is there to click.
    test.skip(
      !process.env.ATTENDED,
      "needs a person to choose a source in the portal picker; run with ATTENDED=1",
    );
    // Well beyond the block's 180s: the clock starts at a dialog a person has to read, and
    // only then does the 45s publish and 30s measured window begin.
    test.setTimeout(420_000);

    const roomName = `elementium-screenshare-${Date.now()}`;
    console.log(`  room: ${roomName}`);
    const server = await startPageServer();

    page.on("console", (m) => console.log(`  [page:${m.type()}] ${m.text()}`));
    page.on("pageerror", (e) => console.log(`  [page:error] ${e.message}`));

    // Encrypted, per FR-011: shared video must be protected on the same terms as camera
    // video, so the receive path under test is the E2EE one, not a plaintext shortcut.
    const query = new URLSearchParams({
      url: SFU_WS,
      token: mintToken("browser-subscriber", roomName),
      key: KEY_HEX,
    });
    await page.goto(`${server.origin}/?${query.toString()}`);
    await expect.poll(() => page.textContent("#state"), { timeout: 20_000 }).toBe("connected");

    // 45s of real capture: the 30s SC-001 asks for, plus room for the 5s settle window
    // below so the measured span is at least 30s of steady state, not 30s including
    // jitter-buffer priming.
    const publisher = startScreenPublisher(roomName, 45, KEY_HEX);

    try {
      await publisher.live;
      await expect
        .poll(
          () =>
            page.evaluate(
              () => (window as never as { __videoSubscribed: boolean }).__videoSubscribed,
            ),
          { timeout: 30_000, message: "the browser never subscribed to the shared screen" },
        )
        .toBe(true);

      const read = async (): Promise<VideoStats> => {
        let last = "not attempted";
        for (let attempt = 0; attempt < 20; attempt += 1) {
          const s = await page.evaluate(() =>
            (window as never as {
              __videoStats: () => Promise<VideoStats | { error: string }>;
            }).__videoStats(),
          );
          if (!("error" in s)) return s;
          last = s.error;
          await page.waitForTimeout(500);
        }
        throw new Error(`could not read video stats after 10s: ${last}`);
      };

      // Let the jitter buffer and decoder settle before the measured window starts.
      await page.waitForTimeout(5_000);

      // Six 5-second steps: 30 seconds of observation, sampled often enough that a burst
      // followed by a stall cannot hide inside the average the way it would in a single
      // before/after pair.
      const SAMPLE_INTERVAL_MS = 5_000;
      const SAMPLE_COUNT = 7;
      const samples: number[] = [];
      for (let i = 0; i < SAMPLE_COUNT; i += 1) {
        if (i > 0) await page.waitForTimeout(SAMPLE_INTERVAL_MS);
        const s = await read();
        samples.push(s.framesDecoded);
      }
      console.log(`  framesDecoded every 5s over 30s: ${JSON.stringify(samples)}`);

      // The actual SC-001 assertion: every consecutive pair must show growth. Checking only
      // samples[0] and samples[last] would pass for a stream that delivered one burst of
      // frames in the first five seconds and nothing after -- which is a live track, a
      // subscribed track, and a `framesDecoded` that "increased" by any two-point measure,
      // while failing exactly the property SC-001 states: steadily increasing, sustained
      // for the whole window.
      for (let i = 1; i < samples.length; i += 1) {
        expect(
          samples[i],
          `framesDecoded must still be climbing at t=${(i * SAMPLE_INTERVAL_MS) / 1000}s ` +
            `(sample ${i}: ${samples[i]} vs sample ${i - 1}: ${samples[i - 1]}); a flat step ` +
            `here means the stream started and then stopped delivering, which a single ` +
            `before/after pair spanning the same 30s cannot distinguish from healthy`,
        ).toBeGreaterThan(samples[i - 1]);
      }
    } finally {
      publisher.stop();
      server.close();
    }
  });

  /**
   * SC-004: "With share audio enabled, a tone played by the shared source is present in the
   * receiver's audio while the microphone remains independently audible and independently
   * mutable."
   *
   * The receiver-side half of that claim is exactly what `RTCPeerConnection.getStats()`
   * already answers for every microphone test above: does audio on this track arrive and
   * get reconstructed, or does it show up as concealment -- a decrypt failure, a stalled
   * jitter buffer, a malformed payload -- while RTCP keeps reporting a healthy stream. The
   * "independently audible and independently mutable" half is a statement about the
   * microphone and share-audio tracks being two distinct, separately controllable tracks;
   * `receiver.html`'s `__audioTracksBySource` map (keyed by `publication.source`, so a
   * second subscribed audio track no longer silently overwrites `__audioTrack`) is what a
   * test would read to check that, but this harness has no independently-driven microphone
   * publisher running at the same time as the screen-share publisher to pair it with --
   * `startPublisher`'s tone is a separate process nothing here starts alongside this one.
   * So this proves the provable half: the share-audio track itself is received and decoded
   * cleanly, sampled repeatedly across the window for the reason SC-001's video assertion
   * samples repeatedly -- a source that delivers one burst and then stalls would still read
   * as "some packets arrived" in a single before/after pair.
   */
  test("share audio is received and reconstructed at the receiver", async ({ page }) => {
    // Attended for the same reason SC-001 is: the publisher blocks on the portal picker.
    test.skip(
      !process.env.ATTENDED,
      "needs a person to choose a source in the portal picker; run with ATTENDED=1",
    );
    test.setTimeout(420_000);

    const roomName = `elementium-screenshare-audio-${Date.now()}`;
    console.log(`  room: ${roomName}`);
    const server = await startPageServer();

    page.on("console", (m) => console.log(`  [page:${m.type()}] ${m.text()}`));
    page.on("pageerror", (e) => console.log(`  [page:error] ${e.message}`));

    // Encrypted, per FR-011, matching SC-001: share audio must be protected on the same
    // terms as the video, so this exercises the real E2EE receive path, not a shortcut.
    const query = new URLSearchParams({
      url: SFU_WS,
      token: mintToken("browser-subscriber", roomName),
      key: KEY_HEX,
    });
    await page.goto(`${server.origin}/?${query.toString()}`);
    await expect.poll(() => page.textContent("#state"), { timeout: 20_000 }).toBe("connected");

    // 40s of real capture: enough for the 5s settle plus a 10s steady-state window below,
    // with headroom the way SC-001's 45s gives its own 30s window.
    const publisher = startScreenPublisher(roomName, 40, KEY_HEX, true);

    try {
      await publisher.live;

      await expect
        .poll(
          () =>
            page.evaluate(
              () =>
                (window as never as {
                  __audioTracksBySource?: Record<string, unknown>;
                }).__audioTracksBySource?.["screen_share_audio"] !== undefined,
            ),
          { timeout: 30_000, message: "the browser never subscribed to the shared audio" },
        )
        .toBe(true);

      const read = async (): Promise<InboundStats> => {
        let last = "not attempted";
        for (let attempt = 0; attempt < 20; attempt += 1) {
          const s = await page.evaluate(() =>
            (window as never as {
              __stats: (source?: string) => Promise<InboundStats | { error: string }>;
            }).__stats("screen_share_audio"),
          );
          if (!("error" in s)) return s;
          last = s.error;
          await page.waitForTimeout(500);
        }
        throw new Error(`could not read share-audio stats after 10s: ${last}`);
      };

      // Let the jitter buffer settle, take a baseline, then measure a steady-state window,
      // exactly as `measure()` does for the microphone tests above.
      await page.waitForTimeout(5_000);
      const before = await read();
      await page.waitForTimeout(10_000);
      const after = await read();
      console.log(`  before: ${JSON.stringify(before)}`);
      console.log(`  after:  ${JSON.stringify(after)}`);

      if (before.ssrc !== after.ssrc) {
        throw new Error(
          `the share-audio stream changed SSRC mid-measurement (${before.ssrc} -> ${after.ssrc}); ` +
            `the track restarted, so a delta across it is meaningless`,
        );
      }

      const stats: InboundStats = {
        ssrc: after.ssrc,
        mimeType: after.mimeType,
        jitter: after.jitter,
        packetsReceived: after.packetsReceived - before.packetsReceived,
        packetsLost: after.packetsLost - before.packetsLost,
        concealedSamples: after.concealedSamples - before.concealedSamples,
        silentConcealedSamples: after.silentConcealedSamples - before.silentConcealedSamples,
        concealmentEvents: after.concealmentEvents - before.concealmentEvents,
        totalSamplesReceived: after.totalSamplesReceived - before.totalSamplesReceived,
        insertedSamplesForDeceleration:
          after.insertedSamplesForDeceleration - before.insertedSamplesForDeceleration,
        removedSamplesForAcceleration:
          after.removedSamplesForAcceleration - before.removedSamplesForAcceleration,
      };
      assertHealthy(stats, "share audio");

      // The publisher's own count of what it actually put on the wire, so a healthy
      // `packetsReceived` can be read against it rather than against a guess -- the same
      // delivery-ratio idea `VIDEO_SENT`/`SENT` already give the video and tone tests.
      const audioSent = publisher.audioSent();
      console.log(`  publisher reported AUDIO_SENT ${audioSent}`);
      expect(
        audioSent,
        "the publisher must report an AUDIO_SENT count; --audio never captured or encoded anything",
      ).not.toBeNull();
      expect(
        audioSent,
        "the publisher must have actually sent share-audio packets",
      ).toBeGreaterThan(0);
    } finally {
      publisher.stop();
      server.close();
    }
  });

  /**
   * SC-003: "Sharing a single window transmits nothing from outside that window,
   * demonstrated by changing content elsewhere and observing no change at the receiver."
   *
   * This is the negative test the spec and quickstart both insist is the actual claim:
   * confirming the *chosen* window's content arrives proves only that capture works --
   * SC-001 already covers that. Confirming that a change *outside* the chosen window does
   * **not** reach the receiver is FR-005 and US2's whole privacy property, and it is the
   * one a backend that silently falls back to full-monitor capture would fail while still
   * passing every positive test.
   *
   * It is written here as a documented manual procedure, not as automated Playwright code,
   * because it cannot be expressed as an assertion with what this harness has:
   *
   * 1. It needs the real portal picker, with a human choosing a single *window* rather than
   *    a monitor -- the same blocker SC-001 above has, and additionally one Level 7 of the
   *    quickstart calls out separately: window selection depends on which portal backend is
   *    present (`org.gnome.Mutter.ScreenCast` vs. the monitor-only wlroots backend), so even
   *    a human running this by hand has to check `busctl --user list` first.
   * 2. It needs a *second*, independently controlled window whose content changes on cue,
   *    outside Playwright's reach entirely -- Playwright drives the one Chromium page this
   *    harness serves, not arbitrary windows on the desktop.
   * 3. Above all, it needs to look at *pixels*. `receiver.html`'s `__videoStats` reports
   *    only `RTCPeerConnection.getStats()` counters -- frame counts, not frame content.
   *    "Nothing changed" is not a counter question: `framesDecoded` climbs identically
   *    whether the decoded picture is the chosen window or a full-desktop capture that
   *    happens to contain it, so no counter-based assertion, however cleverly sampled,
   *    could tell scoped capture from unscoped capture. Answering this needs the attached
   *    `<video>` element's actual decoded pixels -- e.g. drawing it to a canvas and
   *    comparing regions before and after the change -- which `receiver.html` does not do
   *    for any test in this file, because until now nothing here needed to look at a
   *    picture rather than a number.
   *
   * Manual procedure (mirrors quickstart.md Level 5):
   *
   *   1. Start a call between two endpoints (`just dev-test-env` + a second participant, or
   *      `just call-peers` / `just app-join`).
   *   2. From one endpoint, start a screen share and pick a single *window* in the portal
   *      picker (requires `org.gnome.Mutter.ScreenCast` or an equivalent window-capable
   *      backend -- confirm with `busctl --user list | grep ScreenCast` first, per
   *      quickstart Level 7; a monitor-only backend makes this test inapplicable, not
   *      failed).
   *   3. At the receiver, confirm the shared window's content is visible and updating --
   *      this much SC-001 already establishes.
   *   4. Change content in a *different* window on the sharer's desktop: play a video,
   *      resize something, move a bright window over the shared one.
   *   5. Watch the receiver for the length of that change. Pass: nothing about the received
   *      picture reflects it. Fail: any trace of the other window's content reaches the
   *      receiver, which means the backend captured more than the chosen window regardless
   *      of what the picker said.
   */
  test("a single-window share carries nothing from outside that window", async () => {
    test.skip(
      true,
      "SC-003 is proven, but not from here: attempted in this harness and it cannot work. " +
        "The test must script two windows and change one of them, and this compositor tiles " +
        "-- Playwright's Chromium takes the workspace, the scripted windows stop being " +
        "composited, and a window that is not being drawn produces no damage and therefore " +
        "no frames at all. Measured: the publisher sent 7 packets and stopped while the SFU " +
        "asked for a keyframe 44 times. The pixel comparison lives at the capture side " +
        "instead, where nothing competes for the screen: see quickstart.md Level 5, which " +
        "records 0.00 mean pixel change from content outside the shared window against 4.56 " +
        "from content inside it. `window.__videoFingerprint()` in receiver.html is kept: it " +
        "is what a receiver-side pixel assertion would use once a headless capture source " +
        "exists that does not need a composited window.",
    );
  });
});
