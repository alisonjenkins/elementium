/**
 * Frame fetcher — manages polling for video frames from the Rust backend.
 *
 * Uses Tauri IPC (`invoke("get_video_frame")`) for binary frame delivery.
 * The custom `elementium://` protocol doesn't work from http:// origins
 * in WebKitGTK dev mode.
 */

import { invoke } from "@tauri-apps/api/core";

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
    const buf = await invoke<ArrayBuffer>("get_video_frame", { trackId });
    if (!buf || buf.byteLength <= 8) return null;

    const view = new DataView(buf);
    const width = view.getUint32(0, true);
    const height = view.getUint32(4, true);

    if (width === 0 || height === 0) return null;

    return {
      width,
      height,
      rgba: new Uint8ClampedArray(buf, 8),
    };
  } catch {
    return null;
  }
}
