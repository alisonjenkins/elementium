/**
 * Bring up the local MatrixRTC stack before any test runs.
 *
 * Tests used to require a hand-started SFU, documented in a comment at the top of
 * the spec: `docker run -d --name elementium-test-livekit ...`. That is a step
 * everyone forgets once, and the failure it produces -- a websocket that never
 * connects -- looks nothing like "you forgot to start the SFU".
 *
 * So the stack is a precondition the suite establishes for itself: an SFU, a
 * homeserver, and the service that lets a Matrix identity authenticate to the SFU.
 * See `test-env/README.md` for why a homeserver is needed at all.
 *
 * # Leaving a running stack alone
 *
 * If the stack is already up when this runs, it is used as-is and *not* torn down
 * afterwards. Bringing up synapse takes a few seconds and its database persists, so
 * a developer iterating on one test keeps a warm environment instead of paying for
 * it on every run -- and, more importantly, a test run cannot destroy an
 * environment someone was using for something else.
 */
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { writeFile, mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const run = promisify(execFile);

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const REPO = path.resolve(HERE, "../..");
export const TEST_ENV = path.join(REPO, "test-env");
/** Where `provision.sh`'s output is left for the tests to read. */
export const FIXTURE = path.join(REPO, "target/test-env-fixture.json");
/** Written when this run started the stack, so teardown knows it may stop it. */
export const OWNED_MARKER = path.join(REPO, "target/test-env-owned");

const SYNAPSE = "http://localhost:8008";
const LK_JWT = "http://localhost:8090";
const SFU = "http://127.0.0.1:7880";

/** Whether every service answers. */
async function stackIsUp(): Promise<boolean> {
  const ok = async (url: string) => {
    try {
      const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
      return res.status < 500;
    } catch {
      return false;
    }
  };
  return (
    (await ok(`${SYNAPSE}/_matrix/client/versions`)) &&
    (await ok(SFU)) &&
    // No health endpoint; a 404 still proves it is listening and routing.
    (await ok(`${LK_JWT}/sfu/get`))
  );
}

/** Poll until the stack answers, or give up with something diagnosable. */
async function waitForStack(timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await stackIsUp()) return;
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(
    `the MatrixRTC stack did not come up within ${timeoutMs}ms. ` +
      `Check \`docker compose logs\` in ${TEST_ENV}.`,
  );
}

export default async function globalSetup(): Promise<void> {
  await mkdir(path.dirname(FIXTURE), { recursive: true });

  // Build the publishers these tests drive.
  //
  // They are the half of every measurement that is *ours*, and nothing rebuilt them: a
  // run picked up whatever binary happened to be in `target/debug/examples`, which could
  // be hours older than the code under test. That does not fail loudly, it just answers
  // the wrong question -- an encoder fix was read as "did not work" for most of a day
  // because the browser was being fed a publisher built before it.
  //
  // Debug rather than release because that is the path the tests use, and cargo is a
  // no-op when nothing changed.
  if (process.env["ELEMENTIUM_SKIP_PUBLISHER_BUILD"] !== "1") {
    console.log("[test-env] building the publishers the tests drive...");
    await run(
      "cargo",
      [
        "build",
        "-p",
        "elementium-webrtc",
        "--example",
        "publish_test_tone",
        "--example",
        "publish_screen_share",
      ],
      { cwd: REPO },
    );
  }

  const alreadyUp = await stackIsUp();
  if (alreadyUp) {
    console.log("[test-env] stack already running; leaving it alone afterwards");
  } else {
    // Before `up`, not after: synapse reads its config at startup, and the settings this
    // applies are the difference between a homeserver the stack can use and a default one
    // that looks healthy while no call can connect. Idempotent, so this costs a second on
    // a warm environment.
    await run("./configure-synapse.sh", [], { cwd: TEST_ENV });
    console.log("[test-env] starting synapse, livekit and lk-jwt-service...");
    await run("docker", ["compose", "up", "-d"], { cwd: TEST_ENV });
    await waitForStack(90_000);
    await writeFile(OWNED_MARKER, "1");
    console.log("[test-env] stack up");
  }

  // Rooms accumulate otherwise: every provision creates one and every call test creates
  // another, so a few runs leave a client showing dozens of identically named rooms. That
  // costs nothing on the server and makes anything observed by looking at the client much
  // harder to read.
  if (process.env["ELEMENTIUM_SKIP_ROOM_CLEANUP"] !== "1") {
    await run("./cleanup-rooms.sh", [], { cwd: TEST_ENV });
  }

  // Participants and a room, regenerated every run. Cheap, and it keeps one test's
  // membership changes from leaking into the next -- which matters here more than
  // usual, since the faults being chased are *about* membership changes.
  const { stdout } = await run("./provision.sh", [], { cwd: TEST_ENV });
  await writeFile(FIXTURE, stdout);
  const fixture = JSON.parse(stdout) as { participants: unknown[]; room_id: string };
  console.log(
    `[test-env] ${fixture.participants.length} participants in ${fixture.room_id}`,
  );
}
