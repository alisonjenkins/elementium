/**
 * Drive Elementium into a call without a person clicking, for testing only.
 *
 * Every remaining question about the reported faults needs Elementium itself in a call with
 * other participants, and Playwright cannot drive it: it is a Tauri application with a
 * native WebRTC stack behind a WebKit webview. So the application drives itself.
 *
 * This is *not* part of the product. It is built as a separate bundle and injected only when
 * `ELEMENTIUM_AUTOJOIN=1` is set at patch time, so nothing reaches a release build. It is
 * kept out of `src/shim/` for that reason.
 *
 * Runs in both the main window and the Element Call widget, which are separate documents
 * with the same script injected. Each one recognises where it is and does its half.
 */

interface AutoJoinConfig {
  homeserver: string;
  userId: string;
  accessToken: string;
  deviceId: string;
  roomId: string;
  /** Whether to publish video. Either way the microphone is used; video also opens the camera. */
  video?: boolean;
  /** Turn on the peer-connection trace and livekit-client's own debug logging. */
  tracePc?: boolean;
  /**
   * Share the screen once the harness asks for it. See {@link shareWhenAsked}.
   *
   * Not "share on join": a screen share published from the moment the call starts would be in
   * every measurement of the camera, and the test that reconciles encoded against decoded
   * frames would have two outbound tracks to disentangle before it could say anything about
   * either.
   */
  screenShare?: boolean;
}

const CONFIG = (window as unknown as Record<string, unknown>)[
  "__ELEMENTIUM_AUTOJOIN"
] as AutoJoinConfig | undefined;

/** How long to wait for any one element before giving up and saying which. */
const WAIT_MS = 90_000;

function log(message: string): void {
  console.log(`[Elementium autojoin] ${message}`);
}

/** Wait for the first element matching `test`, or report what was never found. */
function waitFor(
  what: string,
  test: () => HTMLElement | null,
  timeout = WAIT_MS,
): Promise<HTMLElement> {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeout;
    const tick = () => {
      const found = test();
      if (found) {
        resolve(found);
        return;
      }
      if (Date.now() > deadline) {
        reject(new Error(`autojoin: ${what} never appeared`));
        return;
      }
      setTimeout(tick, 250);
    };
    tick();
  });
}

/** A clickable element whose accessible name matches, searched by label then by text. */
function byName(pattern: RegExp): () => HTMLElement | null {
  return () => {
    const candidates = Array.from(
      document.querySelectorAll<HTMLElement>("button,[role=button]"),
    );
    return (
      candidates.find((el) => pattern.test(el.getAttribute("aria-label") ?? "")) ??
      candidates.find((el) => pattern.test(el.textContent ?? "")) ??
      null
    );
  };
}

/**
 * An element by its `data-testid`.
 *
 * Element Call's in-call controls are icon-only buttons whose visible name comes from a
 * tooltip, so they have neither text content nor an `aria-label` for {@link byName} to match --
 * which is why the screen-share button "never appeared" while sitting in the DOM the whole
 * time. The test id is the handle upstream provides for exactly this.
 */
function byTestId(id: string): () => HTMLElement | null {
  return () => document.querySelector<HTMLElement>(`[data-testid="${id}"]`);
}

/**
 * Answer `https://localhost/.well-known/matrix/client` from the homeserver's own copy.
 *
 * matrix-js-sdk discovers well-known from the *server name*, not the homeserver URL, and
 * builds `https://<server_name>/.well-known/matrix/client` with the scheme hardcoded. This
 * homeserver is named `localhost`, so that is port 443 -- which nothing in this stack serves
 * and could not without root and a certificate.
 *
 * The request fails, the client caches an empty well-known, and Element Call refuses to
 * start with "Call is not supported. The server is not configured to work with Element Call"
 * -- which sends you to the server configuration, where the focus is present and correct.
 *
 * The Playwright harness solves this with `page.route`; the application has no equivalent,
 * so the fetch is patched here. Test-only, like the rest of this file.
 */
function serveWellKnown(cfg: AutoJoinConfig): void {
  const target = "https://localhost/.well-known/matrix/client";
  const original = window.fetch.bind(window);
  window.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    if (url.startsWith(target)) {
      log("answering well-known discovery from the local homeserver");
      return original(`${cfg.homeserver}/.well-known/matrix/client`, init);
    }
    return original(input, init);
  };
}

/**
 * Put the session into storage before Element Web reads it.
 *
 * The same approach the Playwright harness uses, and for the same reason: driving the login
 * form would test the login form. This script is injected ahead of Element Web's own
 * bundles, so the values are in place before it looks.
 */
function seedSession(cfg: AutoJoinConfig): void {
  if (window.localStorage.getItem("mx_access_token") === cfg.accessToken) return;
  window.localStorage.setItem("mx_hs_url", cfg.homeserver);
  window.localStorage.setItem("mx_user_id", cfg.userId);
  window.localStorage.setItem("mx_access_token", cfg.accessToken);
  window.localStorage.setItem("mx_device_id", cfg.deviceId);
  window.localStorage.setItem("mx_has_access_token", "true");
  window.localStorage.setItem("mx_seen_analytics_toast", "true");
  if (cfg.tracePc) {
    // livekit-client logs through `loglevel`, which reads its level from this key. At the
    // default level it says nothing between "starting to negotiate" and a connection
    // timeout, which is precisely the interval worth seeing when negotiation stalls.
    window.localStorage.setItem("loglevel:livekit", "DEBUG");
    window.localStorage.setItem("loglevel:lk-e2ee", "DEBUG");
  }
  log(`session seeded for ${cfg.userId}`);
}

/**
 * Click away the "Verify this session" prompt if it appears.
 *
 * Polled for a while rather than waited on once: it arrives after the initial sync, which is
 * some seconds after the page is ready. Dismissed rather than satisfied -- verifying would
 * test Element Web's verification flow, which is not what this is for.
 */
function dismissVerificationPrompt(): void {
  const deadline = Date.now() + 30_000;
  const tick = () => {
    const later = byName(/^(later|skip|dismiss)$/i)();
    if (later) {
      later.click();
      log("dismissed the session verification prompt");
      return;
    }
    if (Date.now() < deadline) setTimeout(tick, 500);
  };
  tick();
}

/** The main window: get into the room, then open the call. */
async function driveElementWeb(cfg: AutoJoinConfig): Promise<void> {
  serveWellKnown(cfg);
  seedSession(cfg);
  if (!location.hash.includes(cfg.roomId)) location.hash = `#/room/${cfg.roomId}`;

  // Every provisioned login is a new device, and once any session has set up cross-signing
  // the rest are asked to verify. The prompt sits over the room, so the call button is never
  // clickable and it looks as though the room never loaded.
  dismissVerificationPrompt();

  // Waiting for the call button rather than for a room-view container: the button only
  // exists once the room is open, it is what this needs next, and it does not depend on a
  // class name that changes when Element Web is restyled.
  log("waiting for the room");
  // One reload if the room does not appear. The homeserver can report this account joined
  // while the client still shows "There's no preview, would you like to join?" -- the room
  // reaches it through sync, and a client that started mid-sync sometimes settles only on a
  // second load. Clicking the offered join button is not the answer: it joins by room id
  // with no via-servers and fails.
  try {
    (await waitFor("the video call button", byName(/^video call$/i), 90_000)).click();
  } catch {
    // What the page was actually showing. "Did not appear" covers a room still loading, a
    // room preview for an account that is joined, and an error dialog -- and the reload that
    // follows is only the right move for one of them.
    log(
      `room did not appear; page showing: ` +
        `${document.body?.innerText?.replace(/\s+/g, " ").slice(0, 200) ?? ""}`,
    );
    location.reload();
    return;
  }
  log("call opened");
}

/**
 * Refuse video at the source, for a run that asked not to use the camera.
 *
 * Clicking "Stop video" in the lobby is too late: Element Call acquires the camera to show
 * the preview *before* that control exists, so the webcam light is already on by the time
 * anything could turn it off. The only point early enough is the request itself.
 *
 * Deferred by a tick because the media shim installs synchronously after this script, and
 * whichever wraps `getUserMedia` last wins.
 */
function refuseVideo(): void {
  setTimeout(() => {
    const media = navigator.mediaDevices;
    if (!media?.getUserMedia) return;
    const original = media.getUserMedia.bind(media);
    media.getUserMedia = (constraints?: MediaStreamConstraints) =>
      original({ ...constraints, video: false });
    log("video stripped from getUserMedia; the camera will not be opened");
  }, 0);
}

/**
 * The path the harness creates when it wants a screen share started.
 *
 * A file, served by the same `http-server` that serves this page, because there is no channel
 * *into* a Tauri webview from a test: Playwright cannot attach to it, and the application's
 * own log only goes outward. Element Web's dist directory is a build output, so dropping a
 * file into it costs nothing and is gone with the next build.
 */
const SHARE_SIGNAL = "/elementium-share-now";

/**
 * Wait for the harness to ask for a screen share, then click the control a person would.
 *
 * Clicking the real button rather than calling `getDisplayMedia` directly: the question is
 * whether *a screen share works*, which includes Element Call deciding what to request,
 * publishing a second track, and telling the SFU about it. A direct call would skip all of it
 * and test the shim alone.
 */
async function shareWhenAsked(): Promise<void> {
  const deadline = Date.now() + 15 * 60 * 1000;
  let asked = false;
  while (Date.now() < deadline) {
    try {
      // Cache-busted: `http-server` is started with `-c-1`, but a 404 cached anywhere between
      // here and there would make the signal permanently invisible.
      const res = await fetch(`${SHARE_SIGNAL}?t=${Date.now()}`, { cache: "no-store" });
      if (res.ok) {
        asked = true;
        break;
      }
    } catch {
      /* the server not answering yet is not a reason to stop asking */
    }
    await new Promise((r) => setTimeout(r, 1000));
  }
  if (!asked) {
    log("no screen-share signal arrived before the deadline; not sharing");
    return;
  }
  log("screen-share signal seen; clicking share");
  const share = byTestId("incall_screenshare");
  try {
    (await waitFor("the share screen button", () => share() ?? byName(/^share screen$/i)(), 60_000)).click();
  } catch (e) {
    // Which controls *are* there. Element Call omits the screen-share button entirely when it
    // decides the platform cannot share, and "the button never appeared" cannot tell that
    // apart from a renamed selector.
    const ids = Array.from(document.querySelectorAll("[data-testid]"))
      .map((el) => el.getAttribute("data-testid"))
      .join(",");
    log(`share button not found; controls present: ${ids}`);
    throw e;
  }
  // `aria-checked` flips on the same button once the share is running, so this is the share
  // actually starting rather than the click having been delivered.
  await waitFor(
    "the share to start",
    () => (share()?.getAttribute("aria-checked") === "true" ? share() : null),
    120_000,
  );
  log("screen share started");
}

/**
 * The Element Call widget: join the call.
 *
 * Joining uses the camera and microphone. Element Call takes both in its lobby, before any
 * control to decline them exists, so there is no version of this that quietly avoids the
 * webcam -- only `video: false` in the config, which refuses it at the request.
 */
async function driveElementCall(cfg: AutoJoinConfig): Promise<void> {
  // The widget is a separate document with its own client, so it repeats the discovery the
  // main window just did and needs the same answer.
  serveWellKnown(cfg);
  if (!cfg.video) refuseVideo();
  await waitFor("the lobby", byName(/^join call$/i));

  (await waitFor("the join button", byName(/^join call$/i))).click();
  log("join clicked");

  // Joined is "the lobby is gone", not "a hang-up button matching some name appeared".
  // Matching a control by name made this report failure while the call was up and
  // publishing, because the name differs between Element Call builds.
  await waitFor("the lobby to close", () => (byName(/^join call$/i)() ? null : document.body));
  log("in the call");

  // Not awaited: the share is started on the harness's schedule, minutes later, and this
  // function returning is what says the join succeeded.
  if (cfg.screenShare) {
    shareWhenAsked().catch((e: unknown) => {
      console.error(`[Elementium autojoin] screen share: ${String(e)}`);
    });
  }
}

if (CONFIG) {
  const inWidget = location.pathname.includes("/widgets/element-call/");
  const run = inWidget ? driveElementCall : driveElementWeb;
  log(`starting in ${inWidget ? "the Element Call widget" : "Element Web"}`);
  // Deliberately not awaited at module scope: this runs before Element Web boots, and a
  // rejection here must not stop it loading. Failures are reported and left alone.
  run(CONFIG).catch((e: unknown) => {
    // What the page was showing when it gave up. Without it the report says only that
    // something never appeared, which is true of a login screen, a room preview, an error
    // dialog and a blank page alike.
    const showing = document.body?.innerText?.replace(/\s+/g, " ").slice(0, 300) ?? "";
    console.error(`[Elementium autojoin] ${String(e)} | page showing: ${showing}`);
  });
}
