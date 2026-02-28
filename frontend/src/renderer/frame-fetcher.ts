/**
 * Frame fetcher — manages polling for video frames from the Rust backend.
 *
 * Uses the custom `elementium://video-frame/{trackId}` protocol.
 * Supports adaptive polling: slows down when no new frames are available.
 */

export interface FrameData {
  width: number;
  height: number;
  rgba: Uint8ClampedArray;
}

/**
 * Fetch the latest video frame for a track from the Rust backend.
 */
export async function fetchFrame(trackId: string): Promise<FrameData | null> {
  try {
    const resp = await fetch(`elementium://localhost/video-frame/${trackId}`);
    if (!resp.ok) return null;

    const width = parseInt(resp.headers.get("X-Frame-Width") || "0", 10);
    const height = parseInt(resp.headers.get("X-Frame-Height") || "0", 10);

    if (width === 0 || height === 0) return null;

    const buf = await resp.arrayBuffer();
    return {
      width,
      height,
      rgba: new Uint8ClampedArray(buf),
    };
  } catch {
    return null;
  }
}
