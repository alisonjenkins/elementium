/**
 * Console bridge: forwards all JS console output to Rust via Tauri IPC.
 *
 * Uses __TAURI_INTERNALS__ directly (available before npm packages load).
 * Works in both the main window and Element Call iframe (after IPC bridge is set up).
 */
export function setupConsoleBridge(): void {
  const w = window as unknown as Record<string, unknown>;
  if (w.__elementium_console_bridged) return;
  w.__elementium_console_bridged = true;

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
        try {
          strs.push(typeof args[i] === "string" ? args[i] : JSON.stringify(args[i]));
        } catch {
          strs.push(String(args[i]));
        }
      }
      const t = w.__TAURI_INTERNALS__ as { invoke?: (cmd: string, args: unknown) => Promise<void> } | undefined;
      if (t?.invoke) {
        t.invoke("console_log", { level, args: strs }).catch(() => {});
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
    send("error", [
      `[UnhandledRejection] ${e.reason?.stack ?? String(e.reason)}`,
    ] as unknown as IArguments);
  });
}
