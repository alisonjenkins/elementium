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
  log(`session seeded for ${cfg.userId}`);
}

/** The main window: get into the room, then open the call. */
async function driveElementWeb(cfg: AutoJoinConfig): Promise<void> {
  seedSession(cfg);
  if (!location.hash.includes(cfg.roomId)) location.hash = `#/room/${cfg.roomId}`;

  await waitFor("the room view", () =>
    document.querySelector<HTMLElement>(".mx_RoomView, [class*='RoomView']"),
  );
  log("room open; starting the call");
  (await waitFor("the video call button", byName(/^video call$/i))).click();
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
 * The Element Call widget: join the call.
 *
 * Joining uses the camera and microphone. Element Call takes both in its lobby, before any
 * control to decline them exists, so there is no version of this that quietly avoids the
 * webcam -- only `video: false` in the config, which refuses it at the request.
 */
async function driveElementCall(cfg: AutoJoinConfig): Promise<void> {
  if (!cfg.video) refuseVideo();
  await waitFor("the lobby", byName(/^join call$/i));

  (await waitFor("the join button", byName(/^join call$/i))).click();
  log("join clicked");

  await waitFor("the in-call controls", byName(/end call|leave/i));
  log("in the call");
}

if (CONFIG) {
  const inWidget = location.pathname.includes("/widgets/element-call/");
  const run = inWidget ? driveElementCall : driveElementWeb;
  log(`starting in ${inWidget ? "the Element Call widget" : "Element Web"}`);
  // Deliberately not awaited at module scope: this runs before Element Web boots, and a
  // rejection here must not stop it loading. Failures are reported and left alone.
  run(CONFIG).catch((e: unknown) => {
    console.error(`[Elementium autojoin] ${String(e)}`);
  });
}
