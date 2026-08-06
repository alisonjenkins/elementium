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
const browsersPath = process.env.PLAYWRIGHT_BROWSERS_PATH;
const chromiumPath = process.env.ELEMENTIUM_CHROMIUM;

export default defineConfig({
  testDir: "./tests/browser",
  // Serial by default: these tests share one local SFU and spawn a publisher process each.
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
