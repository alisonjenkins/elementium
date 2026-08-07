/**
 * The foundation the call reproductions stand on: real Element Web clients, on the local
 * homeserver, in the shared room.
 *
 * Kept separate from the tests that place calls because when one of those fails, the first
 * question is whether the participant ever got as far as the room. Answering that here means
 * a call test's failure is about the call.
 */
import { test, expect } from "@playwright/test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startElementWeb, useSession, type ElementWebServer } from "./element-web";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const FIXTURE = path.join(REPO, "target/test-env-fixture.json");
const HOMESERVER = "http://localhost:8008";

interface Participant {
  user_id: string;
  access_token: string;
  device_id: string;
}
interface Fixture {
  homeserver: string;
  room_id: string;
  participants: Participant[];
}

async function fixture(): Promise<Fixture> {
  return JSON.parse(await readFile(FIXTURE, "utf8")) as Fixture;
}

let server: ElementWebServer | null = null;

test.beforeAll(async () => {
  server = await startElementWeb();
});

test.afterAll(async () => {
  await server?.close();
  server = null;
});

test.describe("Element Web on the local stack", () => {
  test.describe.configure({ timeout: 120_000 });

  test("serves an Element Web without Elementium's shims", async ({ page }) => {
    const origin = server!.origin;
    const html = await (await fetch(origin)).text();

    // The dist on disk is patched for the application. Serving that to a plain Chromium
    // would install an RTCPeerConnection that forwards to a Tauri backend which is not
    // there, and every call test would fail for a reason unrelated to what it measures.
    expect(html, "the shim script must not reach a browser without Tauri").not.toContain(
      "elementium-shims.js",
    );

    const config = (await (await fetch(`${origin}/config.json`)).json()) as {
      default_server_config: { "m.homeserver": { base_url: string } };
    };
    expect(config.default_server_config["m.homeserver"].base_url).toBe(HOMESERVER);

    await page.goto(origin);
    expect(
      await page.evaluate(() => document.querySelectorAll("script[src*='elementium']").length),
      "no shim script survived into the loaded document",
    ).toBe(0);
  });

  test("a provisioned session lands in the room without a login form", async ({ page }) => {
    const env = await fixture();
    const who = env.participants[0]!;
    await useSession(page, who, HOMESERVER);

    await page.goto(`${server!.origin}/#/room/${env.room_id}`);

    // The room's composer is the cheapest proof that the client authenticated, completed an
    // initial sync, and resolved the room -- all three, and nothing else, are prerequisites
    // for placing a call in it.
    await expect(
      page.getByRole("textbox", { name: /message|send a message/i }).first(),
    ).toBeVisible({ timeout: 90_000 });
  });
});
