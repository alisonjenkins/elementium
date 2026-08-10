/**
 * Decode a remote track in the page, from encoded frames streamed by Rust.
 *
 * The alternative -- and what still runs when this cannot -- is Rust decoding to RGBA and
 * shipping 3.7MB a frame across the IPC boundary, which measured about 14fps a track while
 * the backend sat idle. An encoded VP8 frame is 20-60kB. The saving is not in how the bytes
 * are moved but in how many there are.
 *
 * ## Frames must arrive in order, all of them
 *
 * A VP8 interframe references the frame before it. That is why this reads a single response
 * that stays open rather than polling for the latest frame: a reader that skips frames feeds
 * the decoder a broken reference chain and produces nothing but errors -- the frozen picture
 * this project has already diagnosed twice, arrived at from a third direction.
 *
 * It is also why decoding does not begin until a keyframe arrives. Handing `VideoDecoder` a
 * delta frame with no reference is an error it does not recover from on its own, so the
 * stream is skipped forward to the first keyframe and only then configured.
 */

/** The wire header Rust writes before each frame: `[u32 len][u8 keyframe][u64 timestamp]`. */
const HEADER_BYTES = 13;

/** What a caller needs to drive one decoded track. */
export interface WebCodecsRendererHandle {
  /** Stop reading and release the decoder. */
  stop: () => void;
}

interface VideoDecoderCtor {
  new (init: {
    output: (frame: VideoFrameLike) => void;
    error: (e: unknown) => void;
  }): VideoDecoderLike;
  isConfigSupported?: (config: unknown) => Promise<{ supported?: boolean }>;
}

interface VideoDecoderLike {
  configure: (config: unknown) => void;
  decode: (chunk: unknown) => void;
  close: () => void;
  state?: string;
}

/** The parts of a `VideoFrame` used here. `drawImage` accepts one directly. */
interface VideoFrameLike {
  displayWidth?: number;
  displayHeight?: number;
  close: () => void;
}

/** Whether this runtime can decode the codec we negotiate. */
export function webCodecsAvailable(): boolean {
  const ctor = (globalThis as Record<string, unknown>)["VideoDecoder"];
  const chunk = (globalThis as Record<string, unknown>)["EncodedVideoChunk"];
  return typeof ctor === "function" && typeof chunk === "function";
}

/**
 * Split a growing byte buffer into whole frames.
 *
 * Returned as a function over accumulated bytes rather than a generator, because a chunk
 * from the network boundary has no relationship to a frame boundary: one read can carry half
 * a frame, or three frames and a fragment.
 */
export function takeFrames(buffer: Uint8Array<ArrayBufferLike>): {
  frames: { keyframe: boolean; timestamp: number; data: Uint8Array<ArrayBufferLike> }[];
  rest: Uint8Array<ArrayBufferLike>;
} {
  // `ArrayBufferLike` rather than `ArrayBuffer`: a `ReadableStream` reader yields views whose
  // buffer type is not narrowed, and asserting the narrower one here would be a claim about
  // someone else's allocation rather than a fact.
  const frames: { keyframe: boolean; timestamp: number; data: Uint8Array<ArrayBufferLike> }[] =
    [];
  let offset = 0;
  while (buffer.byteLength - offset >= HEADER_BYTES) {
    const view = new DataView(buffer.buffer, buffer.byteOffset + offset);
    const length = view.getUint32(0, true);
    const total = HEADER_BYTES + length;
    if (buffer.byteLength - offset < total) break;
    frames.push({
      keyframe: view.getUint8(4) === 1,
      // Microseconds, as `EncodedVideoChunk` expects. `getBigUint64` would be exact but
      // returns a BigInt the chunk constructor rejects; a stream would have to run for
      // nearly three hundred years before the double loses microsecond precision.
      timestamp: Number(view.getBigUint64(5, true)),
      data: buffer.subarray(offset + HEADER_BYTES, offset + total),
    });
    offset += total;
  }
  return { frames, rest: buffer.subarray(offset) };
}

/** Join two byte arrays. */
function concat(
  a: Uint8Array<ArrayBufferLike>,
  b: Uint8Array<ArrayBufferLike>,
): Uint8Array<ArrayBufferLike> {
  if (a.byteLength === 0) return b;
  const out = new Uint8Array(a.byteLength + b.byteLength);
  out.set(a, 0);
  out.set(b, a.byteLength);
  return out;
}

/**
 * Start decoding `trackId` into `canvas`, calling `present` once per painted frame.
 *
 * Returns `null` when the runtime cannot decode, so the caller keeps the established path
 * rather than showing nothing.
 */
export function startWebCodecsRender(
  canvas: HTMLCanvasElement,
  trackId: string,
  present: () => void,
): WebCodecsRendererHandle | null {
  if (!webCodecsAvailable()) return null;
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;

  const DecoderCtor = (globalThis as Record<string, unknown>)[
    "VideoDecoder"
  ] as unknown as VideoDecoderCtor;
  const ChunkCtor = (globalThis as Record<string, unknown>)["EncodedVideoChunk"] as unknown as new (
    init: unknown,
  ) => unknown;

  let running = true;
  let decoded = 0;
  let errors = 0;
  let sawKeyframe = false;
  let controller: AbortController | null = null;

  const decoder = new DecoderCtor({
    output: (frame) => {
      try {
        decoded += 1;
        // The frame is scaled into the canvas's fixed geometry, preserving aspect: the
        // canvas size is what `captureStream` fixed the outgoing track at, and resizing it
        // now would leave the track describing one geometry and the backing store another.
        const width = frame.displayWidth ?? canvas.width;
        const height = frame.displayHeight ?? canvas.height;
        const scale = Math.min(canvas.width / width, canvas.height / height);
        const dw = Math.round(width * scale);
        const dh = Math.round(height * scale);
        ctx.fillStyle = "#000";
        ctx.fillRect(0, 0, canvas.width, canvas.height);
        ctx.drawImage(frame as unknown as CanvasImageSource, (canvas.width - dw) / 2, (canvas.height - dh) / 2, dw, dh);
        present();
      } finally {
        // Not optional: a `VideoFrame` holds a GPU buffer, and leaking them stalls the
        // decoder within a second or two.
        frame.close();
      }
      // Progress while it is happening, not only a total when it stops.
      //
      // Until now the only line about a remote track's decoding was written in `finally`,
      // when the stream ended -- so during a call, "we are decoding this person at thirty a
      // second" and "we have decoded nothing since the first keyframe" looked identical, and
      // both are faults that happened. Every thirtieth frame is about once a second per
      // track; the counters are cumulative so a reader (or a test) can measure a rate
      // between any two lines.
      if (decoded % 30 === 0) {
        console.log(
          `[Elementium] WebCodecs render progress ${trackId}: decoded=${decoded} errors=${errors}`,
        );
      }
    },
    error: (e) => {
      errors += 1;
      if (errors === 1 || errors % 50 === 0) {
        console.warn(
          `[Elementium] WebCodecs decode error on ${trackId} (${errors} so far, ` +
            `${decoded} frames decoded): ${String(e).slice(0, 120)}`,
        );
      }
    },
  });

  const pump = async () => {
    try {
      controller = new AbortController();
      const response = await fetch(`/__elementium/stream/${encodeURIComponent(trackId)}`, {
        cache: "no-store",
        signal: controller.signal,
      });
      const body = response.body;
      if (!body) {
        console.warn(`[Elementium] encoded stream for ${trackId} has no readable body`);
        return;
      }
      const reader = body.getReader();
      let pending: Uint8Array<ArrayBufferLike> = new Uint8Array(0);

      while (running) {
        // eslint-disable-next-line no-await-in-loop
        const { done, value } = await reader.read();
        if (done) break;
        if (!value) continue;
        pending = concat(pending, value);
        const { frames, rest } = takeFrames(pending);
        pending = rest;
        for (const frame of frames) {
          // A zero-length record is the server's keepalive: it exists so that writing to a
          // vanished client fails and the stream can end. There is nothing to decode.
          if (frame.data.byteLength === 0) continue;
          if (!sawKeyframe) {
            if (!frame.keyframe) continue;
            sawKeyframe = true;
            decoder.configure({ codec: "vp8", optimizeForLatency: true });
            console.log(`[Elementium] WebCodecs decoding ${trackId} from its first keyframe`);
          }
          decoder.decode(
            new ChunkCtor({
              type: frame.keyframe ? "key" : "delta",
              timestamp: frame.timestamp,
              data: frame.data,
            }),
          );
        }
      }
    } catch (e) {
      if (running) {
        console.warn(
          `[Elementium] encoded stream for ${trackId} ended: ${String(e).slice(0, 120)}`,
        );
      }
    } finally {
      console.log(
        `[Elementium] WebCodecs render stopped for ${trackId}: ` +
          `${decoded} frames decoded, ${errors} errors`,
      );
    }
  };

  void pump();

  return {
    stop: () => {
      running = false;
      controller?.abort();
      try {
        if (decoder.state !== "closed") decoder.close();
      } catch {
        /* already closed */
      }
    },
  };
}
