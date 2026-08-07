/**
 * What Matrix identities can do in a call, as opposed to what a bare SFU allows.
 *
 * The other suite points a browser at an SFU with a hand-minted dev token. That is
 * right for measuring what a receiver makes of our media, and it cannot reach the
 * two faults that need a homeserver:
 *
 * - how long a key takes to arrive, which is a property of to-device delivery
 * - why someone joining or leaving can silence everyone already in the call, which
 *   needs real membership events and at least three participants
 *
 * These tests establish that the environment supports both: that a Matrix identity
 * authenticates to the SFU the way Element Call makes it, and that several such
 * identities meet in one room and can see each other. The fault-reproducing tests
 * build on that.
 */
import { test, expect } from "@playwright/test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const FIXTURE = path.join(REPO, "target/test-env-fixture.json");
const LK_DIST = path.join(REPO, "frontend/node_modules/livekit-client/dist");

const SYNAPSE = "http://localhost:8008";
const LK_JWT = "http://localhost:8090";

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

/**
 * Get an SFU token for a participant the way Element Call does: ask the homeserver
 * for an OpenID token proving who you are, then exchange it at lk-jwt-service.
 *
 * Deliberately not a shortcut around that exchange. The exchange is part of what is
 * under test -- a token minted directly with the dev secret would pass even if
 * Matrix authentication were completely broken.
 */
async function sfuToken(
  who: Participant,
  room: string,
): Promise<{ url: string; jwt: string }> {
  const openid = await fetch(
    `${SYNAPSE}/_matrix/client/v3/user/${encodeURIComponent(who.user_id)}/openid/request_token`,
    {
      method: "POST",
      headers: { Authorization: `Bearer ${who.access_token}` },
      body: "{}",
    },
  ).then((r) => r.json() as Promise<Record<string, string>>);

  const res = await fetch(`${LK_JWT}/sfu/get`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      room,
      openid_token: {
        access_token: openid["access_token"],
        token_type: openid["token_type"],
        matrix_server_name: openid["matrix_server_name"],
      },
      device_id: who.device_id,
    }),
  });
  expect(res.status, "lk-jwt-service refused a valid Matrix identity").toBe(200);
  return (await res.json()) as { url: string; jwt: string };
}

test.describe("MatrixRTC membership", () => {
  test("a Matrix identity authenticates to the SFU with publish rights", async () => {
    const env = await fixture();
    const who = env.participants[0]!;
    const { url, jwt } = await sfuToken(who, env.room_id);

    expect(url).toContain("7880");

    // The identity in the token is the caller's, not an anonymous one: the SFU uses
    // it to attribute published tracks, and Element Call keys E2EE material by it.
    const claims = JSON.parse(
      Buffer.from(jwt.split(".")[1]!, "base64url").toString("utf8"),
    ) as { sub: string; video: { canPublish: boolean; canSubscribe: boolean } };

    expect(claims.sub).toBe(`${who.user_id}:${who.device_id}`);
    expect(claims.video.canPublish, "a participant that cannot publish is mute").toBe(true);
    expect(claims.video.canSubscribe).toBe(true);
  });

  test("a token is scoped to one room", async () => {
    const env = await fixture();
    const who = env.participants[0]!;

    const a = await sfuToken(who, env.room_id);
    const b = await sfuToken(who, `${env.room_id}-other`);

    const roomOf = (jwt: string) =>
      (
        JSON.parse(Buffer.from(jwt.split(".")[1]!, "base64url").toString("utf8")) as {
          video: { room: string };
        }
      ).video.room;

    // Different rooms must not share a grant, or one call's participant could join
    // another's. The value is a hash rather than the room id, which is why this
    // compares the two against each other rather than against the id.
    expect(roomOf(a.jwt)).not.toBe(roomOf(b.jwt));
  });

  test("three participants meet in one room and see each other", async ({ browser }) => {
    const env = await fixture();
    expect(
      env.participants.length,
      "the fault this environment exists for needs three participants",
    ).toBeGreaterThanOrEqual(3);

    const tokens = await Promise.all(
      env.participants.map((p) => sfuToken(p, env.room_id)),
    );

    // One browser context each: livekit-client keeps per-participant state, and
    // sharing a context would let them see each other's internals rather than only
    // what the SFU tells them.
    const contexts = await Promise.all(env.participants.map(() => browser.newContext()));
    try {
      const pages = await Promise.all(contexts.map((c) => c.newPage()));
      await Promise.all(pages.map((p) => p.goto(pageUrl())));

      const counts = await Promise.all(
        pages.map((page, i) =>
          page.evaluate(
            async ([url, jwt]) => {
              const lk = await import("/lk/livekit-client.esm.mjs");
              const room = new lk.Room();
              await room.connect(url as string, jwt as string);
              // Everyone connects at once, so wait for the room to settle rather
              // than reading immediately -- otherwise this measures scheduling.
              await new Promise((r) => setTimeout(r, 3000));
              const seen = room.remoteParticipants.size;
              await room.disconnect();
              return seen;
            },
            [tokens[i]!.url, tokens[i]!.jwt] as const,
          ),
        ),
      );

      for (const [i, seen] of counts.entries()) {
        expect(
          seen,
          `participant ${i} saw ${seen} others; each should see the other two`,
        ).toBe(env.participants.length - 1);
      }
    } finally {
      await Promise.all(contexts.map((c) => c.close()));
    }
  });
});

/**
 * A real origin serving livekit-client.
 *
 * Not `file://` or an injected script tag: insertable streams need a secure context
 * and the E2EE worker must be same-origin. `127.0.0.1` counts as secure.
 */
let server: { origin: string; close: () => void } | null = null;

function pageUrl(): string {
  if (!server) throw new Error("page server not started");
  return server.origin;
}

test.beforeAll(async () => {
  const { createServer } = await import("node:http");
  const httpd = createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    if (url.pathname.startsWith("/lk/")) {
      readFile(path.join(LK_DIST, path.basename(url.pathname))).then(
        (buf) => {
          res.writeHead(200, { "content-type": "text/javascript" });
          res.end(buf);
        },
        () => res.writeHead(404).end(),
      );
      return;
    }
    res.writeHead(200, { "content-type": "text/html" });
    res.end("<!doctype html><meta charset=utf-8><title>participant</title>");
  });
  await new Promise<void>((resolve) => httpd.listen(0, "127.0.0.1", resolve));
  const addr = httpd.address();
  const port = typeof addr === "object" && addr ? addr.port : 0;
  server = { origin: `http://127.0.0.1:${port}`, close: () => httpd.close() };
});

test.afterAll(() => {
  server?.close();
  server = null;
});
