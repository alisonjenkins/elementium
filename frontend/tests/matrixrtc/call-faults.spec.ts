/**
 * The two reported protocol faults, tested against real Element Call. Neither reproduces.
 *
 * - keys take a long time to arrive, so a participant who joins cannot hear anyone for a while
 * - someone joining or leaving silences everyone already in the call
 *
 * Four scenarios: three participants joining in sequence, one leaving, and hanging up and
 * calling again. All work, with keys arriving and audio decoding. Whatever produces the
 * reported symptom is therefore not Element Call and Matrix alone on this stack -- which
 * leaves Elementium as a participant, or something about a real homeserver that this one
 * does not reproduce.
 *
 * One scenario did fail, and it was this harness: reusing an access token and device id in a
 * fresh browser context keeps the device but discards the crypto store, so the other
 * participants encrypt their keys to a device that can no longer read them. Zero samples
 * against 1,500 packets, indistinguishable from the real thing, and entirely self-inflicted.
 * `freshSessions` exists to keep tests from doing it by accident.
 *
 * Both are about Element Call's key handling and Matrix's to-device delivery, so both are
 * driven through real Element Call in real Element Web against the local homeserver. A
 * version built on livekit-client would supply its own keys on its own schedule and could
 * reproduce neither.
 *
 * What is measured is not "is there a participant tile" but "is audio arriving and usable".
 * A tile appears when someone is in the call; it says nothing about whether their key ever
 * reached you, which is the entire subject.
 */
import { test, expect, type BrowserContext } from "@playwright/test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  startElementWeb,
  createCallRoom,
  freshSessions,
  type Credentials,
  type ElementWebServer,
} from "./element-web";
import {
  openRoom,
  joinCall,
  keysInstalled,
  leaveCall,
  inboundAudio,
  usable,
  type InboundAudio,
  type Participant,
} from "./element-call";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const FIXTURE = path.join(REPO, "target/test-env-fixture.json");

/**
 * How long a participant may wait to hear the others.
 *
 * Element Call delays adopting a new key by `useKeyDelay` (5s), and the key still has to
 * travel as a to-device message and be picked up by the receiver's sync loop. 30 seconds is
 * far longer than that should ever take; the point is to catch "never", not to police a
 * tight bound, and a bound that fails on a slow machine would teach nobody anything.
 */
const AUDIBLE_WITHIN_MS = 30_000;

/**
 * How old the call's key must be before a joiner causes a rotation.
 *
 * Element Call reuses a key younger than ten seconds when someone joins. Waiting past that
 * is the difference between testing a rotation and testing nothing.
 */
const KEY_AGE_FOR_ROTATION_MS = 12_000;

interface Fixture {
  room_id: string;
  participants: Credentials[];
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

/** The highest key index this participant has installed for anyone, or -1 for none. */
function highestKeyIndex(keys: { keyIndex: number }[]): number {
  return keys.reduce((max, k) => Math.max(max, k.keyIndex), -1);
}

/** Wait until this participant is receiving usable audio from `expected` others. */
async function expectHears(p: Participant, expected: number, label: string): Promise<void> {
  // The last reading is kept so the failure can say *how* it failed. "Expected 2, got 0" is
  // the same message whether no packets arrived, packets arrived and could not be decrypted,
  // or they decrypted into concealed noise -- and those have three different causes.
  let last: InboundAudio[] = [];
  try {
    await expect
      .poll(
        async () => {
          last = await inboundAudio(p);
          return usable(last).length;
        },
        { timeout: AUDIBLE_WITHIN_MS },
      )
      .toBeGreaterThanOrEqual(expected);
  } catch (e) {
    const pcs = await p
      .widget()
      .evaluate(() => (window as unknown as { __pcCount?: number }).__pcCount ?? -1)
      .catch(() => -1);
    // Which keys this participant installed, and when. Packets without samples means no key
    // worked; this says whether any arrived at all, for whom, and at what index.
    const keys = await keysInstalled(p);
    // Printed as well as thrown: a test marked `test.fail()` is reported as passing, and its
    // error never reaches the output -- which would hide the only evidence it exists for.
    console.log(
      `  DIAGNOSTIC ${label} (${p.who.user_id}): ` +
        `streams=${JSON.stringify(last)} keys=${JSON.stringify(keys)}`,
    );
    throw new Error(
      `${label} (${p.who.user_id}): expected usable audio from ${expected} other ` +
        `participants, saw ${usable(last).length} of ${last.length} inbound streams ` +
        `across ${pcs} peer connections.\n` +
        `  no peer connections -> the harness is looking at the wrong frame\n` +
        `  connections but no stream -> the SFU never announced or we never subscribed\n` +
        `  packets but no samples -> the key never arrived\n` +
        `  samples but concealed -> the key arrived and was wrong\n` +
        `  streams: ${JSON.stringify(last)}\n` +
        `  keys installed (${keys.length}): ${JSON.stringify(keys)}\n` +
        `${String(e).split("\n")[0]}`,
    );
  }
}

test.describe("MatrixRTC call faults", () => {
  // Serial, and generous: each test drives three real browsers through a real homeserver,
  // and they share one SFU.
  test.describe.configure({ mode: "serial", timeout: 300_000 });

  /**
   * Without this, every test below passes for the wrong reason.
   *
   * Element Call performs frame encryption -- key generation, distribution over to-device
   * messages, rotation on every leaver -- only in an encrypted room. In a plain one it skips
   * all of it, and a call test then exercises none of the machinery these faults live in
   * while looking perfectly healthy. That is exactly what happened when this suite was first
   * written, and nothing in the results said so.
   */
  test("the room these tests use is encrypted", async ({ browser }) => {
    const env = await fixture();
    // A fresh login, like every other test here. The fixture names one device per provision,
    // and a new browser context around it is a device the homeserver holds keys for against
    // a crypto store that no longer has them -- the client never finishes starting, and the
    // room never appears.
    const [who] = await freshSessions(1);
    const context = await browser.newContext();
    try {
      const p = await openRoom(context, server!, who!, env.room_id);
      const encrypted = await p.page.evaluate((roomId) => {
        const peg = (window as unknown as Record<string, unknown>)["mxMatrixClientPeg"] as
          | { get?: () => { isRoomEncrypted?: (id: string) => boolean } | null }
          | undefined;
        return peg?.get?.()?.isRoomEncrypted?.(roomId) ?? null;
      }, env.room_id);
      expect(
        encrypted,
        "an unencrypted room makes every key-handling test here vacuous",
      ).toBe(true);
    } finally {
      await context.close();
    }
  });

  test("a participant who joins last hears everyone already in the call", async ({ browser }) => {
    const three = await freshSessions(3);
    // A room of its own, and devices of their own. A call leaves membership and keys behind, and a test that inherits
    // them is measuring the test before it -- as this suite found out.
    const roomId = await createCallRoom(three, "joins last");
    const contexts = await Promise.all(three.map(() => browser.newContext()));
    const joined: Participant[] = [];
    try {
      for (const [i, who] of three.entries()) {
        const p = await openRoom(contexts[i]!, server!, who, roomId);
        await joinCall(p);
        joined.push(p);
      }

      // The last to join is the interesting one: everyone else's key was distributed before
      // they were in the room to receive it, so hearing them depends on a re-send.
      await expectHears(joined[2]!, 2, "the participant who joined last");
      // And the first, who has to learn two keys distributed after they joined.
      await expectHears(joined[0]!, 2, "the participant who joined first");
    } finally {
      await hangUp(joined, contexts);
    }
  });

  test("the remaining participants keep hearing each other when one leaves", async ({
    browser,
  }) => {
    const three = await freshSessions(3);
    const roomId = await createCallRoom(three, "one leaves");
    const contexts = await Promise.all(three.map(() => browser.newContext()));
    const joined: Participant[] = [];
    try {
      for (const [i, who] of three.entries()) {
        const p = await openRoom(contexts[i]!, server!, who, roomId);
        await joinCall(p);
        joined.push(p);
      }
      await expectHears(joined[0]!, 2, "before anyone leaves");

      // Element Call rotates its key on *every* leaver, so this is the moment the fault is
      // reported at: the two who remain must survive a rotation they did not cause.
      await leaveCall(joined[2]!);

      await expectHears(joined[0]!, 1, "after the third participant left");
      await expectHears(joined[1]!, 1, "after the third participant left");
    } finally {
      await hangUp(joined, contexts);
    }
  });

  /**
   * A fourth participant joining a call that is already running.
   *
   * The leaver case above rotates unconditionally. A joiner does not: Element Call keeps the
   * existing key if it is young, on the reasoning that a key distributed moments ago has not
   * yet been anywhere the new arrival could have seen it. So a test that joins the fourth
   * participant immediately exercises no rotation at all and passes for a reason unrelated to
   * what it claims to check.
   *
   * Hence the wait. Twelve seconds is over the ten-second threshold with enough margin that a
   * loaded machine does not quietly turn this back into the no-rotation case.
   */
  test("everyone keeps hearing each other when a fourth participant joins", async ({
    browser,
  }) => {
    const four = await freshSessions(4);
    const roomId = await createCallRoom(four, "fourth joins");
    const contexts = await Promise.all(four.map(() => browser.newContext()));
    const joined: Participant[] = [];
    try {
      for (const [i, who] of four.slice(0, 3).entries()) {
        const p = await openRoom(contexts[i]!, server!, who, roomId);
        await joinCall(p);
        joined.push(p);
      }
      await expectHears(joined[0]!, 2, "before the fourth joins");

      // See the note above: without this the join is too soon to rotate anything.
      await joined[0]!.page.waitForTimeout(KEY_AGE_FOR_ROTATION_MS);

      // What "a rotation happened" looks like from inside a participant: a key installed at
      // an index higher than any it held before. Recorded so the assertion after the join can
      // show the rotation occurred rather than assume it.
      const highestBefore = highestKeyIndex(await keysInstalled(joined[0]!));

      const fourth = await openRoom(contexts[3]!, server!, four[3]!, roomId);
      await joinCall(fourth);
      joined.push(fourth);

      // Without this the test could pass with no rotation at all -- which is precisely the
      // failure mode the wait above exists to avoid, and it would be invisible.
      await expect
        .poll(async () => highestKeyIndex(await keysInstalled(joined[0]!)), {
          timeout: AUDIBLE_WITHIN_MS,
        })
        .toBeGreaterThan(highestBefore);

      // The three who were already in the call must survive a rotation they did not cause,
      // and the arrival must be heard by all of them rather than only seen.
      await expectHears(joined[0]!, 3, "after the fourth joined");
      await expectHears(joined[1]!, 3, "after the fourth joined");
      await expectHears(fourth, 3, "the fourth participant");
    } finally {
      await hangUp(joined, contexts);
    }
  });

  /**
   * Hanging up and calling again, in the browser that made the first call.
   *
   * This is what a person does after any join or leave that dropped them, and it is the
   * closest thing here to the reported symptom. It works.
   *
   * It is kept because getting it to work is what found the harness fault below, and
   * because if rejoining ever does break, this is where it will show.
   */
  test("a second call, after hanging up the first, works", async ({ browser }) => {
    const three = await freshSessions(3);
    const roomId = await createCallRoom(three, "second call");
    const contexts = await Promise.all(three.map(() => browser.newContext()));
    const joined: Participant[] = [];
    try {
      for (const [i, who] of three.entries()) {
        const p = await openRoom(contexts[i]!, server!, who, roomId);
        await joinCall(p);
        joined.push(p);
      }
      await expectHears(joined[0]!, 2, "the first call");

      // Leave, but keep the browsers. A person who hangs up keeps their crypto store, and
      // discarding it here is what made an earlier version of this test appear to reproduce
      // the reported fault -- see the note on `freshSessions`.
      await Promise.all(joined.map((p) => leaveCall(p)));
      await joined[0]!.page.waitForTimeout(5_000);

      for (const p of joined) await joinCall(p);
      await expectHears(joined[0]!, 2, "the second call");
    } finally {
      await hangUp(joined, contexts);
    }
  });
});

/**
 * Leave the call before closing the browser, as a person would.
 *
 * Closing a context kills the client without posting a leave, so the room keeps a call
 * membership for a device that is gone. Every test here reuses the same users *and the same
 * device ids*, so the next test's participants collide with their own stale membership and
 * fail to exchange keys -- which looked exactly like the fault under investigation, and was
 * this cleanup missing. Failures are swallowed: a test that has already failed must not be
 * reported as failing in its teardown instead.
 */
async function hangUp(joined: Participant[], contexts: BrowserContext[]): Promise<void> {
  await Promise.all(joined.map((p) => leaveCall(p).catch(() => undefined)));
  await Promise.all(contexts.map((c) => c.close()));
}
