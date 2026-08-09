/**
 * Console bridge: forwards all JS console output to Rust via Tauri IPC.
 *
 * Uses __TAURI_INTERNALS__ directly (available before npm packages load).
 * Works in both the main window and Element Call iframe (after IPC bridge is set up).
 */
/**
 * Render one console argument as text worth reading.
 *
 * `JSON.stringify` on an Error yields `{}`, because name, message and stack are all
 * non-enumerable. Every error logged through this bridge therefore reached the log as
 * "something went wrong {}" -- which is how a failing `/sync` was recorded as
 * `sync /sync error %s {}`, and cost an evening of guessing at what the error was.
 *
 * Matrix errors carry the part that identifies them (`errcode`, `httpStatus`) as ordinary
 * properties, so those are pulled out explicitly rather than left to a stringify that skips
 * the inherited ones.
 */
function describe(value: unknown): string {
  if (typeof value === "string") return value;
  if (value instanceof Error) {
    const extra = value as unknown as Record<string, unknown>;
    const parts = [`${value.name}: ${value.message}`];
    for (const key of ["errcode", "httpStatus", "data"]) {
      if (extra[key] !== undefined) parts.push(`${key}=${safeJson(extra[key])}`);
    }
    const stack = value.stack?.split("\n")[1]?.trim();
    if (stack) parts.push(`at ${stack}`);
    return parts.join(" ");
  }
  return safeJson(value);
}

function safeJson(value: unknown): string {
  try {
    const json = JSON.stringify(value);
    // `undefined` stringifies to undefined, and a bare `{}` for an object with only
    // non-enumerable properties is worse than saying what it was.
    if (json === undefined || json === "{}") return String(value);
    return json;
  } catch {
    return String(value);
  }
}

/**
 * Which frame this bridge is running in.
 *
 * Every line from the main window and every line from the Element Call widget iframe land
 * in one log stream, and until now nothing distinguished them. That is not cosmetic: the
 * two run different Matrix clients, and a question as basic as "did the widget's client see
 * this membership change, or only Element Web's?" was unanswerable from a complete log.
 *
 * Detected by frame identity rather than by URL, because both are served from the same
 * loopback origin and their paths have changed more than once.
 */
function frameTag(): string {
  try {
    return window.parent === window ? "main" : "widget";
  } catch {
    // A cross-origin parent throws on comparison. Only the widget can be in that position.
    return "widget";
  }
}

export function setupConsoleBridge(): void {
  const w = window as unknown as Record<string, unknown>;
  if (w.__elementium_console_bridged) return;
  w.__elementium_console_bridged = true;
  const frame = frameTag();

  // Set by patch-element-web.sh from a hash of the exact shims file it copied into this
  // frame -- see the comment on `SHIM_FINGERPRINT` there. Two frames are supposed to run the
  // same build; when one of them does not, this is the one line that says so, instead of the
  // fact only turning up as an unexplained difference in behaviour between frames.
  const fingerprint = w.__ELEMENTIUM_SHIM_FINGERPRINT ?? "unknown";

  const orig = {
    log: console.log.bind(console),
    warn: console.warn.bind(console),
    error: console.error.bind(console),
    debug: console.debug.bind(console),
    info: console.info.bind(console),
  };

  function send(level: string, args: IArguments) {
    try {
      const strs: string[] = [];
      for (let i = 0; i < args.length; i++) {
        strs.push(describe(args[i]));
      }
      const t = w.__TAURI_INTERNALS__ as { invoke?: (cmd: string, args: unknown) => Promise<void> } | undefined;
      if (t?.invoke) {
        t.invoke("console_log", { level, args: [`[${frame}]`, ...strs] }).catch(() => {});
      }
    } catch {
      // Silently ignore bridge errors
    }
  }

  console.log = function () { orig.log.apply(console, arguments as unknown as unknown[]); send("info", arguments); };
  console.info = function () { orig.info.apply(console, arguments as unknown as unknown[]); send("info", arguments); };
  console.warn = function () { orig.warn.apply(console, arguments as unknown as unknown[]); send("warn", arguments); };
  console.error = function () { orig.error.apply(console, arguments as unknown as unknown[]); send("error", arguments); };
  console.debug = function () { orig.debug.apply(console, arguments as unknown as unknown[]); send("debug", arguments); };

  // Capture unhandled errors and promise rejections
  window.addEventListener("error", (e) => {
    send("error", [
      `[Uncaught] ${e.message} at ${e.filename}:${e.lineno}`,
    ] as unknown as IArguments);
  });
  window.addEventListener("unhandledrejection", (e) => {
    // `describe`, not `e.reason.stack`.
    //
    // WebKit's `Error.stack` is the frames alone, with no `name: message` header — unlike
    // V8, where the message is the first line. Logging the stack therefore threw away the
    // one part that says what went wrong, and every rejection in this project's logs read
    // as three anonymous frames into a minified bundle. The same mistake as reducing an
    // Error to `{}` with `JSON.stringify`, twenty lines up, and just as effective at hiding
    // a fault in someone else's code.
    //
    // The stack is kept too — `describe` appends the first frame — because for a rejection
    // it is often the only thing that says *where*.
    send("error", [`[UnhandledRejection] ${describe(e.reason)}`] as unknown as IArguments);
  });

  // Announced once, through `send` itself, so it carries the same `[frame]` tag as every
  // other line and lands in the one log stream a bug report is read from -- not a side
  // channel someone has to know to go looking for.
  send("info", [`[Elementium] shim installed, build=${fingerprint}`] as unknown as IArguments);
}
