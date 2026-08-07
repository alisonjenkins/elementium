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

/** Remove the Elementium shim script tag from Element Web's entry page. */
function withoutShims(html: string): string {
  return html
    .replace(/<script src="elementium-shims\.js"[^>]*><\/script>/g, "")
    .replace(/<script src="elementium-shims\.js"[^>]*>[\s\S]*?<\/script>/g, "");
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
        if (pathname === "/index.html") {
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
