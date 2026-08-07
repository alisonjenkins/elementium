/**
 * Put other people in a call and leave them there, so Elementium can join them.
 *
 * Every scenario Element Call is tested with here passes, which means the one configuration
 * the fault has actually been seen in is the one not covered: Elementium as a participant.
 * That cannot be driven from Playwright -- it is a Tauri application with a native WebRTC
 * stack -- so instead this supplies the other side and waits.
 *
 *     just call-peers        # in one terminal: two participants join and stay
 *     just dev-test-env      # in another: log in as tester1 and join the same call
 *
 * Skipped in ordinary runs. It never ends on its own, which is the point.
 */
import { test } from "@playwright/test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startElementWeb, freshSessions, type Credentials } from "./element-web";
import { openRoom, joinCall, keysInstalled, inboundAudio, usable } from "./element-call";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const FIXTURE = path.join(REPO, "target/test-env-fixture.json");

/** How long the participants stay in the call before giving up on anyone joining them. */
const HOLD_MS = 2 * 60 * 60 * 1000;
/** How often to report what the participants can hear. */
const REPORT_EVERY_MS = 15_000;

test("hold a call open for Elementium to join", async ({ browser }) => {
  test.skip(
    process.env["ELEMENTIUM_HOLD_PEERS"] !== "1",
    "run with `just call-peers`; this never ends on its own",
  );
  test.setTimeout(HOLD_MS + 60_000);

  const env = JSON.parse(await readFile(FIXTURE, "utf8")) as {
    room_id: string;
    participants: Credentials[];
  };

  // Everyone except the first: `tester1` is left free for whoever is driving Elementium, and
  // two clients logged in as the same user with different devices is a different situation
  // from two people, which is not the one being set up.
  // Fresh logins, not the fixture's tokens. Those name one device per user for the whole
  // environment, and a new browser context around an old device id is a device whose keys
  // the server already holds against a crypto store that no longer has them -- the server
  // rejects the key upload ("One time key already exists") and the session never works.
  const all = await freshSessions(env.participants.length);
  const peers = all.slice(1);
  if (peers.length === 0) throw new Error("provision.sh produced nobody to be the other side");
  console.log(`  room ${env.room_id}`);

  const server = await startElementWeb();
  const contexts = await Promise.all(peers.map(() => browser.newContext()));
  const joined = [];
  for (const [i, who] of peers.entries()) {
    const p = await openRoom(contexts[i]!, server, who, env.room_id);
    await joinCall(p);
    joined.push(p);
  }

  console.log("");
  console.log("  ┌─ the other side is in the call ─────────────────────────────");
  console.log(`  │ room     ${env.room_id}`);
  for (const p of joined) console.log(`  │ joined   ${p.who.user_id}`);
  console.log("  │");
  console.log("  │ Now run `just dev-test-env`, log in as tester1 (test-password-1)");
  console.log("  │ and join the video call in the same room.");
  console.log("  │");
  console.log("  │ These participants publish a continuous 440Hz tone, not Chromium's");
  console.log("  │ own fake microphone -- that one is a pulsed beep, so a gap in it means");
  console.log("  │ nothing. With a solid tone, any gap you hear is real.");
  console.log("  │");
  console.log("  │ Below, each line is what these participants can hear. If Elementium");
  console.log("  │ publishes audio they can use, the count goes up when it joins.");
  console.log("  └─────────────────────────────────────────────────────────────");

  const until = Date.now() + HOLD_MS;
  while (Date.now() < until) {
    await joined[0]!.page.waitForTimeout(REPORT_EVERY_MS);
    for (const p of joined) {
      const streams = await inboundAudio(p);
      const keys = await keysInstalled(p);
      // Key senders rather than key count: the question when Elementium joins is whether its
      // key ever arrives, and that is answered by whose keys are present, not how many.
      const senders = [...new Set(keys.map((k) => k.participantIdentity))].sort();
      console.log(
        `  ${p.who.user_id}: hears ${usable(streams).length}/${streams.length} streams; ` +
          `keys from [${senders.join(", ")}]`,
      );
    }
  }

  await Promise.all(contexts.map((c) => c.close()));
  await server.close();
});
