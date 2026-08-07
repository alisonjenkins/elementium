/**
 * Media devices shim that routes getUserMedia / enumerateDevices
 * to the Rust backend via Tauri IPC.
 */

import { invoke } from "@tauri-apps/api/core";
import { createCanvasTrack } from "./canvas-track";

interface NativeMediaDevice {
  id: string;
  label: string;
  kind: "audioInput" | "audioOutput" | "videoInput";
}

// TrackId is a Rust newtype struct TrackId(String), which serde serializes as a plain string.
type NativeTrackId = string;

interface NativeCaptureSource {
  id: string;
  name: string;
  kind: "monitor" | "window";
}

function debugLog(msg: string): void {
  console.log(`[Elementium] ${msg}`);
}

/**
 * Install the media devices shim, replacing navigator.mediaDevices.
 */
export function setupMediaDevicesShim(): void {
  const original = navigator.mediaDevices;

  const shimmedDevices: MediaDevices = {
    ...original,

    getSupportedConstraints(): MediaTrackSupportedConstraints {
      // `resizeMode` is a real constraint that WebKit/Chrome expose, but it is absent
      // from the TS DOM lib's `MediaTrackSupportedConstraints`, so the literal is widened
      // rather than dropping a constraint callers legitimately probe for.
      return {
        width: true,
        height: true,
        aspectRatio: true,
        frameRate: true,
        facingMode: true,
        resizeMode: true,
        sampleRate: true,
        sampleSize: true,
        echoCancellation: true,
        autoGainControl: true,
        noiseSuppression: true,
        latency: true,
        channelCount: true,
        deviceId: true,
        groupId: true,
      } as MediaTrackSupportedConstraints;
    },

    async enumerateDevices(): Promise<MediaDeviceInfo[]> {
      try {
        const devices = await invoke<NativeMediaDevice[]>("enumerate_devices");
        return devices.map((d) => ({
          deviceId: d.id,
          groupId: "",
          kind: mapDeviceKind(d.kind),
          label: d.label,
          toJSON: () => ({ deviceId: d.id, kind: mapDeviceKind(d.kind), label: d.label, groupId: "" }),
        }));
      } catch (e) {
        console.error("[Elementium] enumerateDevices failed:", e);
        return [];
      }
    },

    async getUserMedia(constraints?: MediaStreamConstraints): Promise<MediaStream> {
      console.log("[Elementium] getUserMedia called with:", constraints);

      const nativeConstraints = {
        audio: constraints?.audio ? {
          deviceId: typeof constraints.audio === "object" ?
            (constraints.audio as MediaTrackConstraints).deviceId as string | undefined : undefined,
          echoCancellation: typeof constraints.audio === "object" ?
            (constraints.audio as MediaTrackConstraints).echoCancellation as boolean | undefined : true,
          noiseSuppression: typeof constraints.audio === "object" ?
            (constraints.audio as MediaTrackConstraints).noiseSuppression as boolean | undefined : true,
          autoGainControl: typeof constraints.audio === "object" ?
            (constraints.audio as MediaTrackConstraints).autoGainControl as boolean | undefined : true,
        } : null,
        video: constraints?.video ? {
          deviceId: typeof constraints.video === "object" ?
            (constraints.video as MediaTrackConstraints).deviceId as string | undefined : undefined,
          width: typeof constraints.video === "object" ?
            extractConstraintValue((constraints.video as MediaTrackConstraints).width) : undefined,
          height: typeof constraints.video === "object" ?
            extractConstraintValue((constraints.video as MediaTrackConstraints).height) : undefined,
          frameRate: typeof constraints.video === "object" ?
            extractConstraintValue((constraints.video as MediaTrackConstraints).frameRate) : undefined,
        } : null,
      };

      try {
        debugLog("getUserMedia: calling invoke get_user_media...");
        const trackIds = await invoke<NativeTrackId[]>("get_user_media", {
          constraints: nativeConstraints,
        });
        debugLog(`getUserMedia: got ${trackIds.length} tracks: ${JSON.stringify(trackIds)}`);

        // Create a synthetic MediaStream with tracks
        const stream = new MediaStream();

        for (const tid of trackIds) {
          const id = tid;
          if (id.startsWith("audio-")) {
            // Create a silent audio track (real audio is in Rust)
            try {
              const audioCtx = new AudioContext();
              const oscillator = audioCtx.createOscillator();
              const dest = audioCtx.createMediaStreamDestination();
              oscillator.connect(dest);
              oscillator.frequency.value = 0;
              oscillator.start();
              const audioTrack = dest.stream.getAudioTracks()[0];
              if (audioTrack) {
                stream.addTrack(audioTrack);
              }
              debugLog(`audio track added: ${audioTrack?.id}`);
            } catch (e) {
              debugLog(`audio track error: ${e}`);
            }
          } else if (id.startsWith("video-")) {
            debugLog(`video track ${id}: creating canvas...`);
            // Create a canvas-based video track fed by native camera frames
            const canvas = document.createElement("canvas");
            // Size the canvas to the real capture geometry *before* captureStream, which
            // fixes the resulting track's frame size at the moment it is called. Resizing
            // afterwards leaves the track describing one geometry while the backing store
            // holds another: rows are then read at the wrong stride, so the picture shears
            // into horizontal bands whose colours rotate as the byte offset drifts through
            // the RGBA quad. That is a rendering fault, not a camera fault -- the captured
            // frames are pixel-perfect.
            const geometry = await firstFrameGeometry(id);
            canvas.width = geometry?.width ?? 640;
            canvas.height = geometry?.height ?? 480;
            debugLog(`video track: canvas sized ${canvas.width}x${canvas.height}`);
            // Attach to DOM (hidden) so captureStream works reliably in WebKitGTK
            canvas.style.position = "fixed";
            canvas.style.top = "-9999px";
            canvas.style.left = "-9999px";
            canvas.style.pointerEvents = "none";
            (document.body || document.documentElement).appendChild(canvas);
            debugLog("video track: canvas in DOM");
            // Draw an initial black frame so captureStream has content immediately
            const initCtx = canvas.getContext("2d");
            if (initCtx) {
              initCtx.fillStyle = "#000";
              initCtx.fillRect(0, 0, canvas.width, canvas.height);
            }
            debugLog(`video track: captureStream available? ${typeof canvas.captureStream}`);
            // Manually driven: a frame is emitted only once a draw has completed, so the
            // track can never sample a half-written canvas. See `createCanvasTrack`.
            const canvasTrack = createCanvasTrack(canvas);
            const videoTrack = canvasTrack.track;
            debugLog(`video track: captureStream returned track? ${!!videoTrack} readyState=${videoTrack?.readyState}`);
            if (videoTrack) {
              stream.addTrack(videoTrack);
              // Start fetching real camera frames from the Rust backend
              startLocalVideoFrameFetch(canvas, id, canvasTrack.present);
            }
          }
        }

        debugLog(`getUserMedia returning stream with ${stream.getTracks().length} tracks`);
        return stream;
      } catch (e) {
        console.error("[Elementium] getUserMedia failed:", e);
        throw new DOMException("Could not start media source", "NotAllowedError");
      }
    },

    async getDisplayMedia(_constraints?: DisplayMediaStreamOptions): Promise<MediaStream> {
      console.log("[Elementium] getDisplayMedia called");
      try {
        // Get available capture sources
        const sources = await invoke<NativeCaptureSource[]>("get_capture_sources");

        let sourceId = "default";
        if (sources.length > 0) {
          // Use the first monitor source, or the first available source
          const monitor = sources.find(s => s.kind === "monitor");
          sourceId = (monitor || sources[0]).id;
        }

        // Start screen capture for the selected source
        const trackId = await invoke<NativeTrackId>("get_display_media", { sourceId });
        const id = trackId;

        // Create a canvas-based MediaStream for the screen capture
        const stream = new MediaStream();
        const canvas = document.createElement("canvas");
        canvas.width = 1920;
        canvas.height = 1080;
        const videoTrack = createCanvasTrack(canvas).track;
        if (videoTrack) {
          stream.addTrack(videoTrack);
        }

        console.log(`[Elementium] getDisplayMedia started with source: ${sourceId}, track: ${id}`);
        return stream;
      } catch (e) {
        console.error("[Elementium] getDisplayMedia failed:", e);
        throw new DOMException("Could not start screen capture", "NotAllowedError");
      }
    },

    // Forward events
    ondevicechange: original?.ondevicechange ?? null,
    addEventListener: original?.addEventListener?.bind(original) ?? (() => {}),
    removeEventListener: original?.removeEventListener?.bind(original) ?? (() => {}),
    dispatchEvent: original?.dispatchEvent?.bind(original) ?? (() => false),
  };

  Object.defineProperty(navigator, "mediaDevices", {
    value: shimmedDevices,
    writable: false,
    configurable: true,
  });

  console.log("[Elementium] mediaDevices shim installed");
}

function mapDeviceKind(kind: string): MediaDeviceKind {
  switch (kind) {
    case "audioInput": return "audioinput";
    case "audioOutput": return "audiooutput";
    case "videoInput": return "videoinput";
    default: return "audioinput";
  }
}

/**
 * Extract a numeric value from a MediaTrackConstraints constraint value.
 * Handles plain numbers, ConstrainLong, and ConstrainDouble.
 */
function extractConstraintValue(value: unknown): number | undefined {
  if (typeof value === "number") return value;
  if (typeof value === "object" && value !== null) {
    const obj = value as Record<string, unknown>;
    if ("ideal" in obj) return obj.ideal as number;
    if ("exact" in obj) return obj.exact as number;
  }
  return undefined;
}

/**
 * Fetch video frames from the Rust backend via Tauri IPC and render onto a canvas.
 *
 * Uses invoke("get_video_frame") instead of fetch("elementium://...") because
 * WebKitGTK blocks custom protocol fetches from http:// origins in dev mode.
 * Uses setTimeout instead of requestAnimationFrame because rAF does not
 * fire reliably for detached (off-DOM) canvases, especially inside iframes.
 */
/**
 * Read the capture geometry from the first frame the backend produces.
 *
 * The size is not known until the device has negotiated a format, and the canvas has to be
 * the right size before `captureStream` is called. Bounded so a camera that never produces
 * a frame cannot hang `getUserMedia` -- the caller falls back to a default size, which
 * gives a wrongly-scaled preview rather than no call at all.
 */
/// Target preview period: 30fps is plenty for a self-view and halves the IPC volume of 60.
const TARGET_FRAME_MS = 33;

// Measured on this machine: PipeWire negotiated the camera 3.35s after getUserMedia was
// called, so a 3s probe missed the first frame by 350ms and fell back to 640x480 for the
// whole session. The camera cannot be hurried; the probe can wait.
const GEOMETRY_PROBE_MS = 8000;

async function firstFrameGeometry(
  trackId: string,
  timeoutMs = GEOMETRY_PROBE_MS,
): Promise<{ width: number; height: number } | null> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const buf = await invoke<ArrayBuffer>("get_video_frame", { trackId });
      if (buf && buf.byteLength > 8) {
        const view = new DataView(buf);
        const width = view.getUint32(0, true);
        const height = view.getUint32(4, true);
        if (width > 1 && height > 1) return { width, height };
      }
    } catch {
      // Backend not ready yet; keep waiting until the deadline.
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  debugLog(`firstFrameGeometry: no frame within ${timeoutMs}ms for ${trackId}`);
  return null;
}

function startLocalVideoFrameFetch(
  canvas: HTMLCanvasElement,
  trackId: string,
  present: () => void = () => {},
): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  let running = true;
  let timerId: ReturnType<typeof setTimeout> | null = null;

  let frameCount = 0;
  let scratch: HTMLCanvasElement | null = null;
  // Rolling stats: a preview that is merely slow and one that is broken look the same to a
  // user, and neither is visible in any existing log.
  let windowStart = Date.now();
  let windowFrames = 0;
  let windowFetchMs = 0;

  const fetchLoop = async () => {
    if (!running) return;
    const started = Date.now();

    try {
      // invoke returns ArrayBuffer when Rust returns tauri::ipc::Response
      const buf = await invoke<ArrayBuffer>("get_video_frame", { trackId });
      frameCount++;
      if (buf && buf.byteLength > 8) {
        const view = new DataView(buf);
        const width = view.getUint32(0, true);
        const height = view.getUint32(4, true);

        if (width > 1 && height > 1) {
          const rgba = new Uint8ClampedArray(buf, 8);
          const imageData = new ImageData(rgba, width, height);
          if (canvas.width === width && canvas.height === height) {
            ctx.putImageData(imageData, 0, 0);
          } else {
            // Geometry changed after the track was created (device switch, or a camera
            // that renegotiates). Resizing the canvas now would silently corrupt the
            // track, so scale through a scratch canvas instead and keep the track's
            // geometry stable.
            if (!scratch) scratch = document.createElement("canvas");
            if (scratch.width !== width || scratch.height !== height) {
              scratch.width = width;
              scratch.height = height;
            }
            const sctx = scratch.getContext("2d");
            if (sctx) {
              sctx.putImageData(imageData, 0, 0);
              // Letterbox rather than stretch: the fallback canvas is 4:3 and the camera
              // is 16:9, so filling the canvas would make everyone look short and wide.
              const scale = Math.min(canvas.width / width, canvas.height / height);
              const drawW = Math.round(width * scale);
              const drawH = Math.round(height * scale);
              const dx = Math.round((canvas.width - drawW) / 2);
              const dy = Math.round((canvas.height - drawH) / 2);
              ctx.fillStyle = "#000";
              ctx.fillRect(0, 0, canvas.width, canvas.height);
              ctx.drawImage(scratch, dx, dy, drawW, drawH);
            }
          }
          // The draw is complete: publish it as one frame. Doing this instead of letting
          // the track sample on a timer is what stops a half-written canvas reaching the
          // wire.
          present();
        }
      }
    } catch (err) {
      debugLog(`fetchLoop error: ${err}`);
    }

    const elapsed = Date.now() - started;
    windowFrames += 1;
    windowFetchMs += elapsed;
    if (Date.now() - windowStart >= 5000) {
      const secs = (Date.now() - windowStart) / 1000;
      debugLog(
        `preview ${trackId}: ${(windowFrames / secs).toFixed(1)} fps, ` +
          `${(windowFetchMs / windowFrames).toFixed(1)}ms avg per frame ` +
          `(${canvas.width}x${canvas.height})`,
      );
      windowStart = Date.now();
      windowFrames = 0;
      windowFetchMs = 0;
    }

    if (running) {
      // Schedule against the target period rather than sleeping a fixed amount *after* the
      // work: the previous form made the real period "IPC time + 33ms", so a 30ms fetch
      // halved the frame rate. A frame that took longer than the period is followed
      // immediately by the next.
      const delay = Math.max(0, TARGET_FRAME_MS - elapsed);
      timerId = setTimeout(fetchLoop, delay);
    }
  };

  debugLog(`fetchLoop: starting for ${trackId}`);
  timerId = setTimeout(fetchLoop, TARGET_FRAME_MS);

  // Store cleanup reference on the canvas for stop_track
  (canvas as unknown as Record<string, unknown>).__stopFetch = () => {
    running = false;
    if (timerId !== null) clearTimeout(timerId);
  };
}
