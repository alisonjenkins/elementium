/**
 * The two reported protocol faults, as reproductions.
 *
 * - keys take a long time to arrive, so a participant who joins cannot hear anyone for a while
 * - someone joining or leaving silences everyone already in the call
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
    throw new Error(
      `${label} (${p.who.user_id}): expected usable audio from ${expected} other ` +
        `participants, saw ${usable(last).length} of ${last.length} inbound streams ` +
        `across ${pcs} peer connections.\n` +
        `  no peer connections -> the harness is looking at the wrong frame\n` +
        `  connections but no stream -> the SFU never announced or we never subscribed\n` +
        `  packets but no samples -> the key never arrived\n` +
        `  samples but concealed -> the key arrived and was wrong\n` +
        `  ${JSON.stringify(last)}\n${String(e).split("\n")[0]}`,
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
    const context = await browser.newContext();
    try {
      const p = await openRoom(context, server!, env.participants[0]!, env.room_id);
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
   * REPRODUCTION of the reported fault: the second call a device takes part in is silent.
   *
   * `test.fail()` because the fault is present: this asserts the defect, and Playwright will
   * report a failure on the day it starts working, which is the notification wanted.
   *
   * What is observed, from the receiver's own statistics: roughly 1,500 RTP packets arrive
   * from each of the two other participants over thirty seconds, and `totalSamplesReceived`
   * stays at exactly zero for both. The media is there. Not one frame of it can be
   * decrypted, for the entire call.
   *
   * That is "someone rejoins and nobody can hear each other", with numbers attached: the
   * same three participants, one call after another, the first working and the second not.
   *
   * The device is what carries it, not the room. Before each test was given devices of its
   * own, the leave test failed in exactly this way purely because the test before it had run
   * -- a *different* room, the same devices. Fresh devices in the same room work; reused
   * devices in a new room do not.
   *
   * Deliberately not narrowed further here. Whether the second call's key is never sent,
   * sent and not received, or received at an index livekit has already stopped attempting
   * (see the 2026-08-07 finding in specs/003-call-media-faults/spec.md) is the next
   * question, and it wants the key-arrival logging rather than another browser test.
   */
  test("a second call, by devices that have already been in one, is silent", async ({
    browser,
  }) => {
    // Inside the test, not beside it: at describe scope this marks every test in the file.
    test.fail();
    const three = await freshSessions(3);
    const roomId = await createCallRoom(three, "second call");

    // The first call: joined, proven to work, and hung up properly.
    {
      const contexts = await Promise.all(three.map(() => browser.newContext()));
      const joined: Participant[] = [];
      try {
        for (const [i, who] of three.entries()) {
          const p = await openRoom(contexts[i]!, server!, who, roomId);
          await joinCall(p);
          joined.push(p);
        }
        await expectHears(joined[0]!, 2, "the first call");
      } finally {
        await hangUp(joined, contexts);
      }
    }

    // The second call, in the same room, by the same people.
    const contexts = await Promise.all(three.map(() => browser.newContext()));
    const joined: Participant[] = [];
    try {
      for (const [i, who] of three.entries()) {
        const p = await openRoom(contexts[i]!, server!, who, roomId);
        await joinCall(p);
        joined.push(p);
      }
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
