/**
 * Run Elementium itself as a participant, and read its log while it is in the call.
 *
 * Playwright cannot drive this application -- it is Tauri with a native WebRTC stack behind a
 * WebKit webview -- which is why every fault so far has been found by a person joining a call
 * and describing what they saw. The two halves that close that gap already existed and had
 * never been put together: `just app-join` makes the application join by itself on a virtual
 * display, and its log is JSON lines. This is the join between them, so a test can start the
 * application, watch what it says, and stop it.
 *
 * # Nothing here prints a raw log line
 *
 * The autojoin build carries a live access token, and the application logs SDP at debug. One
 * careless `console.log(line)` in a test helper puts either in a CI transcript for good. So
 * lines are parsed, and only the target, the message and *numeric or boolean* fields are ever
 * rendered -- a string field is dropped even when it would have been harmless. That is a rule
 * a reader can check at a glance, which "be careful what you print" is not.
 */
import { spawn, type ChildProcess } from "node:child_process";
import { createWriteStream, mkdirSync, type WriteStream } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../../..");

/** One `tracing` event, as the JSON layer in `src-tauri/src/main.rs` writes it. */
export interface AppEvent {
  /** Milliseconds since the application was started, so rates can be measured. */
  at: number;
  level: string;
  target: string;
  message: string;
  fields: Record<string, unknown>;
}

export interface ElementiumApp {
  /** Every event so far, oldest first. */
  events: AppEvent[];
  /** The most recent event matching `match`, or `undefined`. */
  latest: (match: (e: AppEvent) => boolean) => AppEvent | undefined;
  /**
   * Wait for an event matching `match`, or fail saying what was never seen.
   *
   * `failOn` names events that mean the wait can only end in the timeout -- the application
   * has already reported that the thing being waited for will not happen. Without it a test
   * spends its whole deadline on a question that was answered in the first half-minute.
   */
  waitFor: (
    what: string,
    match: (e: AppEvent) => boolean,
    timeoutMs: number,
    failOn?: (e: AppEvent) => boolean,
  ) => Promise<AppEvent>;
  /** A redacted tail, for a failure message. Numbers and booleans only -- see the header. */
  tail: (count: number) => string[];
  /**
   * A redacted tail of everything else the launch printed.
   *
   * Elementium failing to *start* -- a build error, a missing fixture, a homeserver that
   * answered 502 -- says so on the same stream as `just`'s own output and never reaches the
   * JSON log at all. Without this the report for the most common failure of the whole test
   * was "no log events seen", which names nothing.
   */
  output: (count: number) => string[];
  /**
   * Where this run's redacted log is being written, for a reader to open afterwards.
   *
   * A failing run prints a tail; a passing one used to print nothing and keep everything in
   * memory, so questions asked after the fact -- did every forwarder stop, did the peer
   * connection count balance, what did the census say -- could only be answered by running
   * the whole call again. Same redaction as `tail`: numeric and boolean fields only.
   */
  logPath: string;
  stop: () => Promise<void>;
}

/**
 * Lines that must never be repeated into a test report, however useful they look.
 *
 * Synapse access tokens start `syt_`, the autojoin config carries one by name, and SDP lines
 * begin `a=`/`o=`. Matched against the raw stream rather than trusted not to appear: the
 * application does not log any of them today, and "today" is not a property a redaction rule
 * should depend on.
 */
const SECRET = /syt_|access_token|accessToken|__ELEMENTIUM_AUTOJOIN|^\s*[aomcs]=/;

/** Render one event with every string field removed. See the header for why. */
function redacted(e: AppEvent): string {
  const safe = Object.entries(e.fields)
    .filter(([, v]) => typeof v === "number" || typeof v === "boolean")
    .map(([k, v]) => `${k}=${String(v)}`)
    .join(" ");
  return `${(e.at / 1000).toFixed(1)}s ${e.level} ${e.target} ${e.message}${safe ? ` ${safe}` : ""}`;
}

/** Numeric field `name` on `e`, or `undefined` if it is absent or not a number. */
export function num(e: AppEvent | undefined, name: string): number | undefined {
  const v = e?.fields[name];
  return typeof v === "number" ? v : undefined;
}

/**
 * Start Elementium under `just app-join` and collect its log.
 *
 * `just app-join` rather than a `cargo tauri dev` spelled out again here: it already knows the
 * things that are not obvious -- the throwaway profile, `GDK_BACKEND=x11` with an empty
 * `WAYLAND_DISPLAY` so GTK does not ignore the virtual display and open a window on the real
 * desktop, and the autojoin injection with its symmetric removal. A second copy of that would
 * be a second thing to keep correct.
 *
 * Started in its own process group, because it is a tree: `just`, `cargo tauri dev`, the
 * `http-server` that Tauri's beforeDevCommand leaves running, `xvfb-run`, and the application.
 * Killing only the child leaves the rest holding port 5173, which the next run reads as a
 * mysteriously stale Element Web.
 */
export function startElementium(options: {
  video: boolean;
  /**
   * Extra environment for the application, e.g. `ELEMENTIUM_AUDIO_DUMP`.
   *
   * Passed through `just` to `cargo tauri dev` like everything else here. It exists because
   * the audio tests need the application to write its decoded inbound PCM to disk, which is
   * an environment switch and has no other trigger.
   */
  env?: Record<string, string>;
}): ElementiumApp {
  const events: AppEvent[] = [];
  const started = Date.now();

  // One file per application, named by start time and pid so two runs cannot interleave into
  // one file and be read as a single call.
  //
  // Under `target/`, not `test-results/`: Playwright empties its output directory at the
  // start of every run, so a log written there survives only until the next one -- which is
  // exactly the moment someone goes looking for it, having just watched a run fail and rerun
  // it. `target/` is already gitignored and is not Playwright's to clear.
  const logDir = path.join(REPO, "target", "app-logs");
  mkdirSync(logDir, { recursive: true });
  const logPath = path.join(logDir, `elementium-app-${started}-${process.pid}.log`);
  const log: WriteStream = createWriteStream(logPath, { flags: "a" });
  /**
   * Write one already-rendered line, unless it looks like it carries a secret.
   *
   * Silent after `stop` has ended the stream. The application goes on printing for as long as
   * it takes SIGTERM to be honoured -- up to ten seconds -- and a write to an ended stream
   * throws "write after end" from inside the stdout handler, which Playwright attributes to
   * whichever test happens to be running. That failed a passing run and named an innocent
   * test; the lines themselves are the tail of a call already measured, and dropping them
   * costs nothing.
   */
  const record = (line: string) => {
    if (log.writableEnded) return;
    if (!SECRET.test(line)) log.write(`${line}\n`);
  };

  const child: ChildProcess = spawn("just", ["app-join"], {
    cwd: REPO,
    detached: true,
    env: {
      ...process.env,
      ELEMENTIUM_AUTOJOIN_VIDEO: options.video ? "1" : "0",
      // The stack is Playwright's global setup's business, including whether this run is
      // allowed to stop it. Letting `app-join` bring it up again restarts the homeserver in
      // the middle of the call being measured -- see the comment on the recipe.
      ELEMENTIUM_STACK_READY: "1",
      ...(options.env ?? {}),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  // Printed, because a file nobody knows about is a file nobody reads. The path only; the
  // contents obey the same rule as everything else here.
  console.log(`  [elementium] log: ${logPath}`);

  let pending = "";
  /** The last few hundred non-event lines, for when the failure is that it never started. */
  const other: string[] = [];
  const consume = (chunk: Buffer) => {
    pending += chunk.toString("utf8");
    const lines = pending.split("\n");
    pending = lines.pop() ?? "";
    for (const line of lines) {
      // Build output, patch-script progress and http-server noise all share this stream.
      if (!line.startsWith("{")) {
        if (line.trim() !== "" && !SECRET.test(line)) {
          other.push(line.trimEnd());
          record(line.trimEnd());
        }
        if (other.length > 400) other.shift();
        continue;
      }
      try {
        const parsed = JSON.parse(line) as {
          level?: string;
          target?: string;
          fields?: Record<string, unknown>;
        };
        const fields = parsed.fields ?? {};
        const { message, ...rest } = fields;
        const event: AppEvent = {
          at: Date.now() - started,
          level: String(parsed.level ?? "?"),
          target: String(parsed.target ?? "?"),
          message: String(message ?? ""),
          fields: rest,
        };
        events.push(event);
        record(redacted(event));
      } catch {
        /* a partial or non-event line is not worth failing a call test over */
      }
    }
  };
  child.stdout?.on("data", consume);
  child.stderr?.on("data", consume);

  const latest = (match: (e: AppEvent) => boolean) => {
    for (let i = events.length - 1; i >= 0; i--) {
      const e = events[i];
      if (e && match(e)) return e;
    }
    return undefined;
  };

  const app: ElementiumApp = {
    events,
    latest,
    tail: (count: number) => events.slice(-count).map(redacted),
    output: (count: number) => other.slice(-count),
    logPath,
    waitFor: async (what, match, timeoutMs, failOn) => {
      const deadline = Date.now() + timeoutMs;
      // Bounded, and bounded loudly: an unbounded wait on a log line that never comes is how
      // a test run turns into an hour of nothing. Every deadline here is a stated number.
      while (Date.now() < deadline) {
        const found = latest(match);
        if (found) return found;
        // A wait that the application has already answered "no" to should not run its clock
        // out. Elementium said "another application is using the camera" twenty-four seconds
        // in; the test then waited twelve minutes to report that it had never seen video.
        const fatal = failOn ? latest(failOn) : undefined;
        if (fatal) {
          throw new Error(
            `Elementium will not be ${what}: it reported "${fatal.message}" ` +
              `after ${(fatal.at / 1000).toFixed(1)}s. Full log: ${logPath}`,
          );
        }
        if (child.exitCode !== null) {
          throw new Error(
            `Elementium exited (code ${child.exitCode}) before ${what}.\n` +
              `${app.tail(10).join("\n")}\n` +
              `last of its output:\n${app.output(25).join("\n")}`,
          );
        }
        await new Promise((r) => setTimeout(r, 500));
      }
      throw new Error(
        `Elementium never reported ${what} within ${timeoutMs}ms ` +
          `(${events.length} log events seen). Last events:\n${app.tail(20).join("\n")}\n` +
          `last of its output:\n${app.output(15).join("\n")}`,
      );
    },
    stop: async () => {
      // Closed first and unconditionally: an early return below (the process already gone)
      // would otherwise leave the stream open and the last writes unflushed, which is
      // precisely the run whose log someone would want to read.
      log.end();
      if (child.pid === undefined || child.exitCode !== null) return;
      // The whole group, and TERM before KILL: the application holds a camera and an audio
      // device, and a SIGKILLed webview leaves both claimed until something notices.
      try {
        process.kill(-child.pid, "SIGTERM");
      } catch {
        /* already gone */
      }
      const deadline = Date.now() + 10_000;
      while (child.exitCode === null && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 200));
      }
      try {
        process.kill(-child.pid, "SIGKILL");
      } catch {
        /* already gone */
      }
    },
  };
  return app;
}
