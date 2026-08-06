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
 * barely audible far better than any codec explanation. Our AddTrack also never populates
 * the `encryption` field, which is one candidate.
 *
 * Requires a local SFU:
 *
 *   docker run -d --name elementium-test-livekit --network host \
 *       livekit/livekit-server --dev --bind 0.0.0.0
 */
import { test, expect } from "@playwright/test";
import { createServer } from "node:http";
import { createHmac } from "node:crypto";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const LK_DIST = path.join(REPO, "frontend/node_modules/livekit-client/dist");
const PUBLISHER = path.join(REPO, "target/debug/examples/publish_test_tone");

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
function startPublisher(room: string, seconds: number, keyHex?: string): Publisher {
  const args = [
    "--sfu", SFU_HTTP,
    "--room", room,
    "--identity", "rust-publisher",
    "--seconds", String(seconds),
    ...(keyHex ? ["--key-hex", keyHex] : []),
  ];
  const proc = spawn(PUBLISHER, args, { stdio: "pipe" });
  let sent: number | null = null;
  let out = "";
  const live = new Promise<void>((resolve, reject) => {
    proc.stdout.on("data", (chunk: Buffer) => {
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

interface InboundStats {
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
): Promise<{ stats: InboundStats; sent: number | null; errors: string[] }> {
  const roomName = `elementium-recv-${Date.now()}`;
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
  await page.goto(`${server.origin}/?${query.toString()}`);
  await expect
    .poll(() => page.textContent("#state"), { timeout: 20_000 })
    .toBe("connected");

  const publisher = startPublisher(roomName, 30, keyHex);

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

    const read = async (): Promise<InboundStats> => {
      const s = await page.evaluate(() =>
        (window as never as {
          __stats: () => Promise<InboundStats | { error: string }>;
        }).__stats(),
      );
      if ("error" in s) throw new Error(`could not read receiver stats: ${s.error}`);
      return s;
    };

    // Let the jitter buffer settle, take a baseline, then measure a steady-state window.
    await page.waitForTimeout(5_000);
    const before = await read();
    await page.waitForTimeout(10_000);
    const after = await read();

    const stats: InboundStats = {
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
});
