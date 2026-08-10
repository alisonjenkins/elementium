/**
 * Open the room as several browser participants at once, and get every prompt out of the way.
 *
 * `element-call.ts`'s `openRoom` handles the two toasts a provisioned tester sees. It does not
 * handle the *dialog* -- heading "Verify this device" -- that Element Web raises for an account
 * which already has other sessions, and every tester acquires other sessions the second time a
 * suite logs it in. The dialog sits over the room, so the composer never appears and the
 * failure reads as "the room never loaded" after two minutes.
 *
 * This is a local copy of `openRoom` with a wider dismissal, kept here rather than as an edit to
 * the shared file because that file is being changed concurrently.
 *
 * # What dismissing verification does and does not change
 *
 * These sessions stay unverified. Nothing here asserts anything about *who* a key came from --
 * the assertions are frame counts -- and an unverified device still receives Megolm keys and
 * Element Call's to-device frame keys on this homeserver, which is proved rather than assumed:
 * a frame that decodes is a frame that decrypted. A test that wanted to assert key provenance
 * would need verified sessions and should not use this.
 */
import type { BrowserContext, Page } from "@playwright/test";
import { useSession, type Credentials, type ElementWebServer } from "./element-web";
import { observeKeys, type Participant } from "./element-call";

const HOMESERVER = "http://localhost:8008";

/** Record every `RTCPeerConnection` the page makes, before Element Call can make one. */
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
 * Click away anything standing between this session and the room.
 *
 * Raced against the composer appearing rather than waited on individually: on most runs none of
 * these prompts show up at all, and a fixed wait for each costs half a minute per participant.
 */
async function clearPrompts(page: Page, timeoutMs: number): Promise<void> {
  const composer = page.getByRole("textbox", { name: /message|send a message/i }).first();
  // The toasts, and then the verification dialog's own wording -- "Skip verification for now"
  // and the "I'll verify later" it asks for in confirmation.
  const named = /^(later|skip|dismiss|not now)$/i;
  const verify = /skip verification|verify later|i'?ll verify later|continue without/i;
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await composer.isVisible().catch(() => false)) return;
    for (const pattern of [named, verify]) {
      for (const role of ["button", "link"] as const) {
        const control = page.getByRole(role, { name: pattern }).first();
        if (await control.isVisible().catch(() => false)) {
          await control.click().catch(() => undefined);
        }
      }
    }
    await page.waitForTimeout(500);
  }
  // What was actually on screen. "The composer never appeared" is true of a login page, a room
  // preview, an error dialog and a modal alike, and only one of those is worth retrying.
  const showing = await page
    .evaluate(() => document.body.innerText.replace(/\s+/g, " ").slice(0, 300))
    .catch(() => "<unreadable>");
  throw new Error(
    `the room never became usable within ${timeoutMs}ms. The page was showing: ${showing}`,
  );
}

/**
 * Make sure this account is a member of the room, over the API.
 *
 * `provision.sh` joins the participants it creates and no others, so any tester beyond its
 * count -- which is how this suite gets a third far-end participant -- arrives as a stranger.
 * Element Web then shows a room *preview* rather than the room, and the join button it offers
 * joins by room id with no via-servers and fails (the autojoin driver hit the same thing).
 * The room is `public_chat`, so a plain join over the API is all that is needed, and it is a
 * no-op for an account already in it.
 */
export async function ensureJoined(who: Credentials, roomId: string): Promise<void> {
  const res = await fetch(`${HOMESERVER}/_matrix/client/v3/join/${encodeURIComponent(roomId)}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${who.access_token}` },
    body: "{}",
  });
  if (!res.ok) {
    throw new Error(`${who.user_id} could not join ${roomId}: HTTP ${res.status}`);
  }
}

/** Open the room as `who`, in a context of their own, with the prompts cleared. */
export async function openRoomClean(
  context: BrowserContext,
  server: ElementWebServer,
  who: Credentials,
  roomId: string,
  timeoutMs = 120_000,
): Promise<Participant> {
  const page = await context.newPage();
  await recordPeerConnections(page);
  await observeKeys(page);
  await useSession(page, who, HOMESERVER);
  console.log(`  [${who.user_id}] loading the room`);
  await page.goto(`${server.origin}/#/room/${roomId}`);
  await clearPrompts(page, timeoutMs);
  return {
    who,
    page,
    context,
    widget: () => {
      const frame = page.frames().find((f) => f.url().includes("element-call"));
      if (!frame) throw new Error(`the Element Call widget is not loaded for ${who.user_id}`);
      return frame;
    },
  };
}
