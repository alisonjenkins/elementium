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
