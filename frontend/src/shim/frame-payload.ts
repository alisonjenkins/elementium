/**
 * Reading a video frame back from Rust, whichever way the IPC delivered it.
 *
 * `get_video_frame` returns raw bytes. How they arrive depends on which transport the
 * webview ended up using: Tauri's custom-protocol IPC delivers an `ArrayBuffer`, and its
 * postMessage fallback delivers the same bytes as an ordinary array of numbers. WebKitGTK
 * forces the fallback whenever the page is served over http, which is every release build
 * since the frontend moved to loopback HTTP so that login could complete.
 *
 * Assuming the first shape costs a picture and says nothing: `.byteLength` on an array is
 * `undefined`, `undefined > 8` is false, and the frame is skipped without an error, a
 * counter, or a log line. It cost the self-view once and then, because the remote renderer
 * is a second copy of the same loop in another file, it cost the remote participant's video
 * as well -- a black tile while Rust decoded her frames and handed every one of them over.
 *
 * Shared rather than duplicated for exactly that reason: two copies of this decision is how
 * the same bug gets fixed once and shipped twice.
 */

/** Logged once, so a transport that changes shape says so rather than going quiet. */
let reported = false;

/** Coerce whatever the IPC handed back into an `ArrayBuffer`, or null if it is not bytes. */
export function frameBytes(value: unknown): ArrayBuffer | null {
  if (!reported) {
    reported = true;
    const shape = value instanceof ArrayBuffer
      ? "ArrayBuffer"
      : ArrayBuffer.isView(value)
        ? `${value.constructor.name} view`
        : Array.isArray(value)
          ? `Array(${value.length})`
          : typeof value;
    console.log(`[Elementium] video frame IPC payload arrives as ${shape}`);
  }
  if (value instanceof ArrayBuffer) return value;
  if (ArrayBuffer.isView(value)) {
    return value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength) as ArrayBuffer;
  }
  if (Array.isArray(value)) return new Uint8Array(value).buffer;
  return null;
}

/**
 * Fetch one frame's bytes for `trackId`, over HTTP rather than over Tauri's IPC.
 *
 * The app is served from a loopback HTTP server, so the webview's origin is not a `tauri://`
 * one and Tauri's binary custom-protocol IPC is unavailable -- every log this project has
 * produced opens with "IPC custom protocol failed, Tauri will now use the postMessage
 * interface instead". Over postMessage a `Vec<u8>` is serialised as a JSON array of numbers,
 * so a 1280x720 RGBA frame -- 3.7MB -- crosses as about three and a half million
 * comma-separated integers. Measured cost: 14fps a track against a target of 30, with the
 * decoder and the whole backend idle.
 *
 * The frame endpoint is on the same origin the page was served from, so this is a relative
 * URL and works identically in the main window and the Element Call iframe.
 *
 * Returns `null` rather than throwing on a failed fetch: a missed frame is one dropped
 * frame, and the caller already counts and reports those.
 */
export async function fetchFrameBytes(trackId: string): Promise<ArrayBuffer | null> {
  const response = await fetch(`/__elementium/frame/${encodeURIComponent(trackId)}`, {
    cache: "no-store",
  });
  if (!response.ok) return null;
  return response.arrayBuffer();
}
