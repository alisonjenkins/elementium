/**
 * Does the shim injection still take, on the Element Web version in front of us?
 *
 * This is the gate for an upgrade. Every other test here serves Element Web with our shims
 * *removed*, because those tests are about the other person in a call. This one is the
 * exception: it serves the dist exactly as the application receives it, and asks whether
 * each shim attached to anything.
 *
 * "Attached to anything" rather than "ran". A shim that executes and lands on nothing looks
 * identical from outside — the page loads, the app starts, and the microphone is silent. It
 * is the failure mode an upstream change produces, and the one nothing else here would
 * catch.
 *
 * The widget frame is checked as well as the main window. They are separately injected
 * documents, and the widget is the half that carries the media.
 *
 * See `specs/007-element-web-upgrade/contracts/shim-install.md`.
 */
import { test, expect } from "@playwright/test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startElementWeb, type ElementWebServer } from "./element-web";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const BUILD_RECORD = path.join(REPO, "element-web-dist/.elementium-build.json");

interface ShimInstall {
  installed: boolean;
  detail: string;
  skipped?: boolean;
  reason?: string;
}

/**
 * Every shim `index.ts` installs.
 *
 * Listed here rather than derived from whatever the page reports, so a shim that stops
 * being installed at all is a failure rather than an absence nobody notices.
 */
const EXPECTED = [
  "console-bridge",
  "secret-storage",
  "webrtc",
  "media-devices",
  "e2ee-bridge",
  "membership-log",
  "livekit-bridge",
] as const;

let server: ElementWebServer | null = null;

test.beforeAll(async () => {
  server = await startElementWeb({ keepShims: true });
});

test.afterAll(async () => {
  await server?.close();
  server = null;
});

async function shimReport(
  page: import("@playwright/test").Page,
): Promise<Record<string, ShimInstall>> {
  await page
    .waitForFunction(
      () => Boolean((window as unknown as Record<string, unknown>)["__elementium_shims"]),
      undefined,
      { timeout: 30_000 },
    )
    .catch(() => undefined);
  return page.evaluate(
    () =>
      ((window as unknown as Record<string, unknown>)["__elementium_shims"] ?? {}) as Record<
        string,
        ShimInstall
      >,
  );
}

test.describe("Elementium shim contract", () => {
  test.describe.configure({ timeout: 120_000 });

  test("every shim installs, or says why it declined", async ({ browser }) => {
    const context = await browser.newContext();
    try {
      const page = await context.newPage();
      await page.goto(server!.origin);
      const report = await shimReport(page);

      // An absent key means the module never ran, which is a different fault from
      // `installed: false` — one is a broken injection, the other a broken shim.
      const missing = EXPECTED.filter((name) => !(name in report));
      expect(
        missing,
        `these shims never ran; the injection into index.html did not take. Report: ${JSON.stringify(report)}`,
      ).toEqual([]);

      const failed = Object.entries(report).filter(([, s]) => !s.installed && !s.skipped);
      expect(
        failed.map(([name, s]) => `${name} (${s.detail}): ${s.reason ?? "?"}`),
        "these shims ran but did not attach to anything",
      ).toEqual([]);
    } finally {
      await context.close();
    }
  });

  /**
   * The widget carries the media, and is injected separately from the main document. An
   * upgrade that changes only `widgets/element-call/index.html` breaks calls and nothing
   * else, which is indistinguishable from a working application until someone dials.
   */
  test("the Element Call widget document is injected too", async ({ browser }) => {
    const context = await browser.newContext();
    try {
      const page = await context.newPage();
      await page.goto(`${server!.origin}/widgets/element-call/index.html`);
      const report = await shimReport(page);

      expect(
        Object.keys(report).length,
        "the widget document has no shim report at all: nothing was injected into it",
      ).toBeGreaterThan(0);

      const failed = Object.entries(report).filter(([, s]) => !s.installed && !s.skipped);
      expect(
        failed.map(([name, s]) => `${name}: ${s.reason ?? "?"}`),
        "shims in the widget frame ran but did not attach",
      ).toEqual([]);
    } finally {
      await context.close();
    }
  });

  /**
   * The IPC bridge is not a shim and would not appear in the report, but without it the
   * widget cannot reach Rust at all — every native call in the half that carries the media
   * fails. Asserted at the document, because that is where it is injected.
   */
  test("the widget carries the Tauri IPC bridge", async () => {
    const html = await readFile(
      path.join(REPO, "element-web-dist/widgets/element-call/index.html"),
      "utf8",
    );
    expect(html, "IPC bridge missing from the Element Call widget").toContain(
      "__TAURI_INTERNALS__",
    );
  });

  /**
   * Not a shim, but the same class of silent failure: a build whose patch script did not
   * finish leaves no record, and every later question about "what was running" is
   * unanswerable.
   */
  test("the build record says what this was built from", async () => {
    const record = JSON.parse(await readFile(BUILD_RECORD, "utf8")) as Record<string, unknown>;
    for (const field of [
      "elementWebVersion",
      "source",
      "builtAt",
      "patches",
      "elementCallFingerprint",
      "autojoinInjected",
    ]) {
      expect(record[field], `build record is missing ${field}`).toBeDefined();
    }
    // Not a style check: the autojoin driver carries a live access token and dials a call on
    // startup. A dist with it injected must never be what a release is cut from.
    expect(
      record["autojoinInjected"],
      "this dist has the autojoin driver injected; it must not be released",
    ).toBe(false);
  });
});
