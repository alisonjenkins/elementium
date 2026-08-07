/**
 * Serve Element Web to Playwright participants, pointed at the local stack.
 *
 * These participants are "the other person" in a call: ordinary Element Web clients, not
 * Elementium. That distinction is not cosmetic. Elementium's shims replace
 * `RTCPeerConnection` with one that forwards to Rust over Tauri IPC, and in a plain Chromium
 * there is no Tauri to forward to -- installing them would produce a client that cannot
 * negotiate anything, and every test would fail for a reason unrelated to what it measures.
 *
 * So the copy served here has the shim script removed and the homeserver config replaced.
 * The files on disk are not touched: `element-web-dist/` stays patched for the application,
 * and the substitutions happen in the response.
 */
import { createServer, type Server } from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { Page } from "@playwright/test";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");
const DIST = path.join(REPO, "element-web-dist");
const TEST_CONFIG = path.join(REPO, "element-web-config/config.test-env.json");

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".woff2": "font/woff2",
  ".ttf": "font/ttf",
};

export interface ElementWebServer {
  origin: string;
  close: () => Promise<void>;
}

/**
 * Remove everything Elementium injects into the dist from a served page.
 *
 * Applied to every HTML document, not just the entry page: the Element Call widget has its
 * own `index.html` and carries its own copies. Missing one produced a participant that
 * reached the call and then failed at `enumerateDevices`, because the shim was asking a
 * Tauri backend that does not exist in a browser.
 *
 * The autojoin driver matters just as much and is easier to miss. `just app-join` injects it
 * into the same `element-web-dist/` this server reads, along with a session for tester1. A
 * page served with it still present logs itself in as tester1 and drives itself into a call,
 * silently replacing whatever session this harness seeded -- which presents as every
 * participant being the same user, in a room they are not in.
 */
function withoutShims(html: string): string {
  return html
    .replace(/<script[^>]*src="[^"]*elementium-shims\.js"[^>]*>\s*<\/script>/g, "")
    .replace(/<script[^>]*src="[^"]*elementium-autojoin\.js"[^>]*>\s*<\/script>/g, "")
    .replace(/<script>window\.__ELEMENTIUM_AUTOJOIN[\s\S]*?<\/script>/g, "");
}

/**
 * Start a static server for Element Web on 127.0.0.1.
 *
 * `127.0.0.1` rather than a LAN address because insertable streams -- which back livekit's
 * E2EE worker, and therefore anything these tests measure about encryption -- require a
 * secure context, and loopback counts as one where a LAN address does not.
 */
export async function startElementWeb(): Promise<ElementWebServer> {
  const testConfig = await readFile(TEST_CONFIG, "utf8");

  const httpd: Server = createServer((req, res) => {
    void (async () => {
      const url = new URL(req.url ?? "/", "http://127.0.0.1");
      let pathname = decodeURIComponent(url.pathname);

      if (pathname === "/config.json") {
        res.writeHead(200, { "content-type": MIME[".json"]! });
        res.end(testConfig);
        return;
      }

      // Element Web is a single-page app with hash routing, so any path that is not a real
      // file is the app itself. Without this, a reload on `#/room/...` 404s.
      const asFile = path.join(DIST, pathname);
      const isFile = path.extname(pathname) !== "" && asFile.startsWith(DIST);
      if (!isFile) pathname = "/index.html";

      const file = path.join(DIST, pathname);
      if (!file.startsWith(DIST)) {
        res.writeHead(403).end();
        return;
      }

      try {
        if (pathname.endsWith(".html")) {
          const html = await readFile(file, "utf8");
          res.writeHead(200, { "content-type": MIME[".html"]! });
          res.end(withoutShims(html));
          return;
        }
        const body = await readFile(file);
        res.writeHead(200, {
          "content-type": MIME[path.extname(pathname)] ?? "application/octet-stream",
        });
        res.end(body);
      } catch {
        res.writeHead(404).end();
      }
    })();
  });

  await new Promise<void>((resolve) => httpd.listen(0, "127.0.0.1", resolve));
  const addr = httpd.address();
  const port = typeof addr === "object" && addr ? addr.port : 0;

  return {
    origin: `http://127.0.0.1:${port}`,
    close: () =>
      new Promise<void>((resolve) => {
        httpd.closeAllConnections?.();
        httpd.close(() => resolve());
      }),
  };
}

export interface Credentials {
  user_id: string;
  access_token: string;
  device_id: string;
}

/**
 * Put an existing session into storage so Element Web starts logged in.
 *
 * Driving the login form instead would test Element Web's login form, which nothing here is
 * about, and would add a page of selectors that break whenever it is restyled. The tokens
 * come from `provision.sh`, so they are real sessions on the real homeserver.
 */
export async function useSession(page: Page, who: Credentials, homeserver: string): Promise<void> {
  await serveWellKnownOverHttps(page);
  await page.addInitScript(
    ([hs, userId, token, deviceId]) => {
      window.localStorage.setItem("mx_hs_url", hs as string);
      window.localStorage.setItem("mx_user_id", userId as string);
      window.localStorage.setItem("mx_access_token", token as string);
      window.localStorage.setItem("mx_device_id", deviceId as string);
      window.localStorage.setItem("mx_has_access_token", "true");
      // Element Web asks about analytics and notifications on first run; both render modal
      // toasts over the room view and would swallow the clicks these tests make.
      window.localStorage.setItem("mx_local_settings", JSON.stringify({ analyticsOptIn: false }));
      window.localStorage.setItem("mx_seen_analytics_toast", "true");
    },
    [homeserver, who.user_id, who.access_token, who.device_id] as const,
  );
}

/**
 * Answer `https://localhost/.well-known/matrix/client`, which nothing can serve here.
 *
 * matrix-js-sdk discovers well-known from the *server name*, not the homeserver URL, and it
 * builds `https://<server_name>/.well-known/matrix/client` with the scheme hardcoded. This
 * homeserver's name is `localhost`, so that is `https://localhost/` on port 443 -- which no
 * part of this stack listens on and could not, without root and a certificate.
 *
 * The request therefore fails, the client caches an empty well-known, and Element Call
 * refuses to start with `MISSING_MATRIX_RTC_FOCUS` -- the focus is advertised there and
 * nowhere else it looks. That presents as "the server is not configured to work with Element
 * Call", which sends you to the server config, where everything is correct.
 *
 * Fulfilled from the homeserver's own copy, so there is one source of truth: if `test-env`
 * changes its focus, this follows without being edited.
 */
async function serveWellKnownOverHttps(page: Page): Promise<void> {
  await page.route("https://localhost/.well-known/matrix/client", async (route) => {
    try {
      const upstream = await fetch("http://localhost:8008/.well-known/matrix/client");
      route.fulfill({
        status: upstream.status,
        contentType: "application/json",
        body: await upstream.text(),
      });
    } catch {
      route.fulfill({ status: 404, body: "" });
    }
  });
}

/**
 * Create a room configured the way a call needs, and put everyone in it.
 *
 * A room per test, rather than the shared one from `provision.sh`, because a call leaves
 * state behind -- membership, keys, and whatever the previous participants' devices believe
 * -- and a test that inherits it is measuring the test before it. Where that carry-over is
 * the subject, a test reuses a room deliberately and says so.
 *
 * The two settings are the ones without which a call cannot happen at all; both are
 * explained where `provision.sh` does the same thing.
 */
export async function createCallRoom(
  participants: Credentials[],
  name: string,
): Promise<string> {
  const [owner, ...rest] = participants;
  if (!owner) throw new Error("a room needs at least one participant");

  const created = (await (
    await fetch("http://localhost:8008/_matrix/client/v3/createRoom", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${owner.access_token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        preset: "public_chat",
        name,
        initial_state: [
          {
            type: "m.room.encryption",
            state_key: "",
            content: { algorithm: "m.megolm.v1.aes-sha2" },
          },
        ],
        power_level_content_override: {
          events: {
            "org.matrix.msc3401.call.member": 0,
            "m.call.member": 0,
            "m.rtc.member": 0,
          },
        },
      }),
    })
  ).json()) as { room_id?: string; error?: string };

  if (!created.room_id) throw new Error(`could not create a room: ${created.error ?? "?"}`);

  for (const who of rest) {
    await fetch(
      `http://localhost:8008/_matrix/client/v3/join/${encodeURIComponent(created.room_id)}`,
      { method: "POST", headers: { Authorization: `Bearer ${who.access_token}` } },
    );
  }
  return created.room_id;
}

/**
 * Log each test participant in again, producing a device that has never been in a call.
 *
 * The tokens from `provision.sh` name one device per user for the whole run, and a device
 * carries state a call leaves behind. Sharing them across tests made each test depend on the
 * one before it -- a failure that looked exactly like the fault under investigation and was
 * not. Where reusing a device is the point, a test does it deliberately and says so.
 *
 * Users are `testerN` / `test-password-N`, as `provision.sh` creates them.
 *
 * A fresh *login*, not a fresh browser context around an old token: reusing a device id
 * while discarding the crypto store leaves a device the other participants have cached keys
 * for and which can no longer decrypt anything sent to it. That produces exactly the symptom
 * being investigated -- packets arriving, not one frame decrypting -- and is entirely an
 * artefact. An earlier version of this suite reported it as a reproduction of the fault.
 */
export async function freshSessions(count: number): Promise<Credentials[]> {
  const out: Credentials[] = [];
  for (let i = 1; i <= count; i++) {
    const res = await fetch("http://localhost:8008/_matrix/client/v3/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        type: "m.login.password",
        identifier: { type: "m.id.user", user: `tester${i}` },
        password: `test-password-${i}`,
      }),
    });
    const body = (await res.json()) as Partial<Credentials> & { error?: string };
    if (!body.user_id || !body.access_token || !body.device_id) {
      throw new Error(`could not log tester${i} in: ${body.error ?? res.status}`);
    }
    out.push({
      user_id: body.user_id,
      access_token: body.access_token,
      device_id: body.device_id,
    });
  }
  return out;
}
