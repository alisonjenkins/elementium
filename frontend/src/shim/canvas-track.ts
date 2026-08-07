/**
 * Turning a canvas into a `MediaStreamTrack` without tearing.
 *
 * `canvas.captureStream(fps)` samples the canvas on its own timer, unsynchronised with
 * whatever is drawing into it. Our frame loops draw a full frame at a time — a ~900KB
 * `putImageData` — and a sample landing part-way through one produces an image made of
 * slices of consecutive draws: horizontal bands, each showing a different moment, each
 * offset from the last. It looks exactly like a broken encoder, and the pixels going in
 * are perfect. This was measured: frames dumped from the Rust side of the same pipeline
 * are pixel-perfect while the displayed track is torn.
 *
 * `captureStream(0)` disables the timer entirely and hands over a track that emits a frame
 * only when `requestFrame()` is called. Calling it after a draw completes means a frame
 * can never be sampled mid-write.
 */

/** A canvas-backed track, and the way to publish a completed frame to it. */
export interface CanvasTrack {
  /** The track to hand to the caller. Null if the browser produced no video track. */
  track: MediaStreamTrack | null;
  /** The stream the track belongs to. */
  stream: MediaStream;
  /**
   * Publish the canvas's current contents as one frame.
   *
   * Call after a draw completes, never during one. A no-op on a browser without
   * `requestFrame`, where the track falls back to timer sampling.
   */
  present: () => void;
}

/**
 * Wrap `canvas` in a manually-driven capture stream.
 *
 * `fallbackFps` is used only where `requestFrame` is unavailable; a browser that has it
 * gets no timer at all.
 */
export function createCanvasTrack(canvas: HTMLCanvasElement, fallbackFps = 30): CanvasTrack {
  // A zero frame rate means "emit only on requestFrame".
  let stream = canvas.captureStream(0);
  let track = stream.getVideoTracks()[0] ?? null;
  let capture = track as CanvasCaptureMediaStreamTrack | null;
  const manual = typeof capture?.requestFrame === "function";

  if (!manual) {
    // No manual control: fall back to timer sampling rather than a track that never
    // emits. Tearing is a defect; a frozen picture is a worse one.
    stream.getTracks().forEach((t) => t.stop());
    stream = canvas.captureStream(fallbackFps);
    track = stream.getVideoTracks()[0] ?? null;
    capture = null;
    console.warn(
      "[Elementium] canvas.requestFrame unavailable; falling back to timer capture, which can tear",
    );
  }

  return {
    track,
    stream,
    present: () => capture?.requestFrame(),
  };
}
