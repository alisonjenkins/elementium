import { defineConfig } from "@playwright/test";
import { readdirSync } from "node:fs";

/**
 * Browser-driving tests for the WebRTC receive path.
 *
 * `executablePath` is set explicitly rather than relying on Playwright's own browser
 * download: nix supplies the browsers (see `PLAYWRIGHT_BROWSERS_PATH` in flake.nix), and
 * its Chromium revision does not match the one the npm package expects, so the usual
 * revision-directory lookup fails. Pointing at the binary sidesteps version coupling
 * entirely and keeps the shell hermetic.
 */
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
/** Looped by Chromium for the whole run; a whole number of cycles, so the join is silent. */
const TONE = path.resolve(HERE, "../test-env/tone-440.wav");

const browsersPath = process.env.PLAYWRIGHT_BROWSERS_PATH;
const chromiumPath = process.env.ELEMENTIUM_CHROMIUM;

export default defineConfig({
  testDir: "./tests",
  // The stack these tests need -- an SFU, a homeserver, and the service that lets a
  // Matrix identity authenticate to the SFU -- is brought up here rather than being
  // a step in a comment that everyone forgets exactly once. See tests/global-setup.ts.
  globalSetup: "./tests/global-setup.ts",
  globalTeardown: "./tests/global-teardown.ts",
  // Serial by default: these tests share one SFU and homeserver, and spawn a
  // publisher process each.
  workers: 1,
  fullyParallel: false,
  reporter: [["list"]],
  // One retry: the SFU does not always announce a newly-joined publisher to a subscriber
  // that is already in the room, and that race is not what these tests are measuring. It
  // is tracked separately -- see the note in receive-path.spec.ts.
  retries: 1,
  use: {
    launchOptions: {
      executablePath: chromiumPath ?? findChromium(browsersPath),
      args: [
        "--no-sandbox",
        // Deterministic media: no real devices are involved, and a headless container has
        // none anyway.
        "--use-fake-ui-for-media-stream",
        "--use-fake-device-for-media-stream",
        // A continuous 440Hz tone instead of the fake device's own audio. Chromium's fake
        // microphone is a *pulsed* beep -- roughly one per second, with silence between --
        // so listening to it says nothing: a gap is what it sounds like when it is working.
        // A solid tone makes any gap mean something, by ear and in the concealment numbers.
        `--use-file-for-fake-audio-capture=${TONE}`,
        // Insertable streams back livekit's E2EE worker; without this the encrypted test
        // would fail for a reason unrelated to what it is measuring.
        "--enable-blink-features=RTCInsertableStreams",
      ],
    },
  },
});

/** Locate the Chromium binary inside a nix `playwright-browsers` tree. */
function findChromium(root: string | undefined): string | undefined {
  if (!root) return undefined;
  // Revision directories are named `chromium-<rev>`; there is normally exactly one.
  const dir = readdirSync(root)
    .filter((d) => d.startsWith("chromium-"))
    .sort()
    .pop();
  return dir ? `${root}/${dir}/chrome-linux64/chrome` : undefined;
}
