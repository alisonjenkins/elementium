/**
 * Media devices shim that routes getUserMedia / enumerateDevices
 * to the Rust backend via Tauri IPC.
 */

import { invoke } from "@tauri-apps/api/core";

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
      };
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
            canvas.width = 640;
            canvas.height = 480;
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
              initCtx.fillRect(0, 0, 640, 480);
            }
            debugLog(`video track: captureStream available? ${typeof canvas.captureStream}`);
            const canvasStream = canvas.captureStream(30);
            const videoTrack = canvasStream.getVideoTracks()[0];
            debugLog(`video track: captureStream returned track? ${!!videoTrack} readyState=${videoTrack?.readyState}`);
            if (videoTrack) {
              stream.addTrack(videoTrack);
              // Start fetching real camera frames from the Rust backend
              startLocalVideoFrameFetch(canvas, id);
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
        const canvasStream = canvas.captureStream(30);
        const videoTrack = canvasStream.getVideoTracks()[0];
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
function startLocalVideoFrameFetch(canvas: HTMLCanvasElement, trackId: string): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  let running = true;
  let timerId: ReturnType<typeof setTimeout> | null = null;

  let frameCount = 0;
  const fetchLoop = async () => {
    if (!running) return;

    try {
      // invoke returns ArrayBuffer when Rust returns tauri::ipc::Response
      const buf = await invoke<ArrayBuffer>("get_video_frame", { trackId });
      frameCount++;
      if (buf && buf.byteLength > 8) {
        const view = new DataView(buf);
        const width = view.getUint32(0, true);
        const height = view.getUint32(4, true);

        if (width > 1 && height > 1) {
          if (canvas.width !== width || canvas.height !== height) {
            canvas.width = width;
            canvas.height = height;
          }
          const rgba = new Uint8ClampedArray(buf, 8);
          const imageData = new ImageData(rgba, width, height);
          ctx.putImageData(imageData, 0, 0);
        }
      }
    } catch (err) {
      debugLog(`fetchLoop error: ${err}`);
    }

    if (running) {
      timerId = setTimeout(fetchLoop, 33);
    }
  };

  debugLog(`fetchLoop: starting for ${trackId}`);
  timerId = setTimeout(fetchLoop, 33);

  // Store cleanup reference on the canvas for stop_track
  (canvas as unknown as Record<string, unknown>).__stopFetch = () => {
    running = false;
    if (timerId !== null) clearTimeout(timerId);
  };
}
