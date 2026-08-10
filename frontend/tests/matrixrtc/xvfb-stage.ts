/**
 * Put something worth capturing on the virtual display Elementium is running on.
 *
 * A screen share is only testable if there is a screen. `just app-join` runs the application
 * under `xvfb-run`, whose display starts as an empty root window: capture it and the far end
 * receives a rectangle of black, which decodes at full rate and proves nothing about a capture
 * path that has to read pixels, convert them and encode them.
 *
 * So a stage is arranged deliberately: a Chromium window sized to the whole display, showing a
 * page whose background alternates between near-black and near-white with a bar sweeping across
 * it. Measured at the far end, the share does carry this stage -- its luminance is the stage's,
 * not the application window's.
 *
 * # What is not established
 *
 * That the stage *animates* once it is up. On a display with no window manager Chromium can
 * decide its window is not visible and freeze the page, which stops an in-page timer dead; the
 * animation is therefore driven from out here over the devtools protocol, with a one-pixel
 * window resize to force a recomposite. That is written and has not been confirmed to work --
 * the run that would have measured it was killed when the machine ran out of memory. The
 * screen-share test asserts what the stage is definitely good for (a real, non-black picture to
 * capture) and does not assert that consecutive frames differ. See `app-call-video.spec.ts`.
 *
 * # Finding the display
 *
 * `xvfb-run -a` picks the display number itself and tells only its child, so the number is not
 * knowable in advance from here. Rather than change `just app-join` to fix one -- which would
 * make concurrent runs collide -- the Xvfb process is found by diffing `/proc` across the
 * application's start: whichever `Xvfb` appeared is ours, and its own command line carries both
 * the display and the `-auth` file needed to connect to it.
 */
import { chromium, type Browser, type Page } from "@playwright/test";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

/** An Xvfb server: which display it serves, and the cookie file needed to talk to it. */
export interface XvfbDisplay {
  pid: number;
  display: string;
  /** The `-auth` file `xvfb-run` generated, or `undefined` if it started without one. */
  xauthority?: string;
  /** The screen geometry, from `-screen 0 WxHxD`. What a whole-monitor share will capture. */
  width: number;
  height: number;
}

/** Read one process's argv, or `undefined` if it is gone or not ours to read. */
function cmdline(pid: number): string[] | undefined {
  try {
    return readFileSync(`/proc/${pid}/cmdline`, "utf8").split("\0").filter(Boolean);
  } catch {
    return undefined;
  }
}

/** Every Xvfb server currently running that this user can see. */
export function runningXvfb(): XvfbDisplay[] {
  const out: XvfbDisplay[] = [];
  for (const entry of readdirSync("/proc")) {
    const pid = Number(entry);
    if (!Number.isInteger(pid) || pid <= 0) continue;
    const argv = cmdline(pid);
    if (!argv || !argv[0] || path.basename(argv[0]) !== "Xvfb") continue;
    const display = argv.find((a) => /^:\d+$/.test(a));
    if (!display) continue;
    const authAt = argv.indexOf("-auth");
    // `-screen 0 1280x800x24`. Read rather than assumed: it is what a monitor share will
    // capture, and therefore the resolution the far end should report for the share.
    const geometry = /^(\d+)x(\d+)x\d+$/.exec(argv[argv.indexOf("-screen") + 2] ?? "");
    out.push({
      pid,
      display,
      xauthority: authAt >= 0 ? argv[authAt + 1] : undefined,
      width: Number(geometry?.[1] ?? 0),
      height: Number(geometry?.[2] ?? 0),
    });
  }
  return out;
}

/**
 * Wait for an Xvfb server that was not running before, and say which one it is.
 *
 * Bounded, and loud when it expires: the alternative is a screen-share test that waits forever
 * for a display that the application never got as far as creating.
 */
export async function newXvfb(before: XvfbDisplay[], timeoutMs: number): Promise<XvfbDisplay> {
  const known = new Set(before.map((x) => x.pid));
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const fresh = runningXvfb().filter((x) => !known.has(x.pid));
    if (fresh[0]) return fresh[0];
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(
    `no new Xvfb server appeared within ${timeoutMs}ms; ` +
      `${before.length} were already running. \`just app-join\` starts one through ` +
      `\`xvfb-run -a\`, so this means the application never got that far.`,
  );
}

/**
 * The page put on the virtual display.
 *
 * Deliberately cheap to encode and impossible to confuse with a still: the background is one
 * flat colour at any instant, so an encoder spends almost nothing on it, but it alternates
 * between the two ends of the luminance range every 500ms, which no frozen frame can imitate.
 * The sweeping bar changes every displayed frame, so the *frame* differs even between colour
 * changes -- otherwise a stream that delivered two frames a second would look identical to one
 * delivering thirty.
 */
const STAGE_HTML = `<!doctype html>
<meta charset="utf-8">
<title>Elementium screen-share stage</title>
<style>
  html, body { margin: 0; height: 100%; overflow: hidden; background: #101010; }
  #bar { position: absolute; top: 0; bottom: 0; width: 12%; background: #ff2d55; }
  #caption { position: absolute; left: 4%; top: 44%; font: 700 7vw/1 sans-serif;
             color: #2d7dff; letter-spacing: 0.1em; }
</style>
<body>
  <div id="bar"></div>
  <div id="caption">ELEMENTIUM STAGE</div>
</body>`;

/** Locate the Chromium binary the way `playwright.config.ts` does. */
function chromiumPath(): string | undefined {
  const explicit = process.env["ELEMENTIUM_CHROMIUM"];
  if (explicit) return explicit;
  const root = process.env["PLAYWRIGHT_BROWSERS_PATH"];
  if (!root) return undefined;
  const dir = readdirSync(root)
    .filter((d) => d.startsWith("chromium-"))
    .sort()
    .pop();
  return dir ? `${root}/${dir}/chrome-linux64/chrome` : undefined;
}

export interface Stage {
  close: () => Promise<void>;
}

/**
 * Show the stage full-screen on `display`, and return a handle that takes it down again.
 *
 * Started *after* the application, so it is the topmost window and is what a root-window
 * capture sees. Its own window is on the virtual display, so nothing appears on the real one.
 */
export async function startStage(display: XvfbDisplay): Promise<Stage> {
  const file = path.join(tmpdir(), `elementium-stage-${process.pid}.html`);
  writeFileSync(file, STAGE_HTML, "utf8");

  const env: Record<string, string> = {
    ...(process.env as Record<string, string>),
    DISPLAY: display.display,
    // The same trap `just app-join` documents, and it is not enough to blank `WAYLAND_DISPLAY`:
    // Chromium's ozone auto-selection reads `XDG_SESSION_TYPE` and tried the compositor anyway,
    // failing with "Failed to initialize Wayland platform" before it ever looked at `DISPLAY`.
    // The variable is *removed*, not emptied, and the platform is named outright below.
    GDK_BACKEND: "x11",
    XDG_SESSION_TYPE: "x11",
  };
  delete env["WAYLAND_DISPLAY"];
  if (display.xauthority) env["XAUTHORITY"] = display.xauthority;

  let browser: Browser | undefined;
  let page: Page | undefined;
  browser = await chromium.launch({
    headless: false,
    executablePath: chromiumPath(),
    env,
    args: [
      "--no-sandbox",
      // Named rather than left to auto-detection -- see the note on the environment above.
      "--ozone-platform=x11",
      // Sized explicitly, and *not* with `--kiosk` or `--start-fullscreen`. Both of those ask
      // a window manager to make the window fullscreen, and an `xvfb-run` display has no
      // window manager at all: the window came up at its default size in the corner, so the
      // share captured a strip of the stage and the rest of the application's own window,
      // and the far end's picture "changed" by a quarter of the luminance range instead of
      // all of it. An explicit size covering the screen needs nobody's cooperation.
      `--window-size=${display.width},${display.height}`,
      "--window-position=0,0",
      "--force-device-scale-factor=1",
      // Without a window manager Chromium's occlusion detection can decide this window is
      // hidden and stop painting it, which is the same failure the timer above works around.
      "--disable-features=CalculateNativeWinOcclusion",
      // The stage is a decoration for a capture, not a browsing session; none of this
      // machinery should compete for the CPU the encoder needs.
      "--disable-extensions",
      "--disable-background-timer-throttling",
    ],
  });
  page = await browser.newPage({ viewport: null });
  // Sized through the devtools protocol rather than by `--window-size`. Playwright launches
  // Chromium with `--no-startup-window` and creates the page's window itself, so the command
  // line never decides this window's size -- it came up at Chromium's default, covering a
  // corner of the display, and the share captured the application's own window around it.
  const cdp = await page.context().newCDPSession(page);
  const { windowId } = (await cdp.send("Browser.getWindowForTarget")) as { windowId: number };
  await cdp.send("Browser.setWindowBounds", {
    windowId,
    bounds: { left: 0, top: 0, width: display.width, height: display.height },
  });
  await page.goto(`file://${file}`);
  // A page Chromium believes is not on screen gets frozen, and a frozen page is a still
  // picture -- exactly what this stage exists not to be. On a display with no window manager
  // that is what happened: the first paint landed and nothing ever moved again.
  await cdp.send("Page.setWebLifecycleState", { state: "active" }).catch(() => undefined);

  // The animation is driven from out here rather than by a timer inside the page, for the same
  // reason. A renderer that has stopped running its own timers still services these calls, so
  // the stage keeps moving whatever Chromium decides about the window's visibility.
  let step = 0;
  const paint = async (): Promise<void> => {
    const n = step++;
    await page
      ?.evaluate((i: number) => {
        document.body.style.background = i % 4 < 2 ? "#050505" : "#fafafa";
        const bar = document.getElementById("bar");
        if (bar) bar.style.left = `${(i * 7) % 88}%`;
      }, n)
      .catch(() => undefined);
    // And a one-pixel resize, which forces the window to be recomposited. Changing the DOM is
    // not by itself a guarantee that anything reaches the X server when nothing is asking the
    // browser to present a frame; changing the window's geometry is.
    await cdp
      .send("Browser.setWindowBounds", {
        windowId,
        bounds: { left: 0, top: 0, width: display.width, height: display.height - (n % 2) },
      })
      .catch(() => undefined);
  };
  await paint();
  // 250ms, so a full dark/light cycle takes a second -- slower than the X11 capture path
  // manages here (~3 frames a second), so a capture cannot alias with it and miss both ends.
  const timer = setInterval(() => void paint(), 250);

  return {
    close: async () => {
      clearInterval(timer);
      await page?.close().catch(() => undefined);
      await browser?.close().catch(() => undefined);
    },
  };
}
