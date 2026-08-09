/**
 * Media devices shim that routes getUserMedia / enumerateDevices
 * to the Rust backend via Tauri IPC.
 */

import { invoke } from "@tauri-apps/api/core";
import { createCanvasTrack } from "./canvas-track";
import { frameBytes } from "./frame-payload";

/**
 * Notice devices appearing and disappearing, and tell listeners.
 *
 * The shim used to forward `devicechange` to the real `navigator.mediaDevices`, which in
 * this webview never sees the devices the app actually uses — those are enumerated in Rust.
 * So plugging in a headset mid-call changed nothing the picker could see, and the user had
 * to leave and rejoin to select it.
 *
 * Polled rather than pushed, and only while somebody is listening. PipeWire and cpal both
 * have hotplug signals and forwarding them would be better, but that is a Rust-side change
 * of its own; this closes the user-visible gap without pretending to be event-driven.
 * Nothing polls when no listener is registered, so the cost is zero in the normal case.
 */
class DeviceWatcher extends EventTarget {
  private timer: ReturnType<typeof setInterval> | null = null;
  private fingerprint: string | null = null;
  private listeners = 0;

  /** Compare device lists by identity and label, not by count. */
  static fingerprintOf(devices: NativeMediaDevice[]): string {
    return devices
      .map((d) => `${d.kind}:${d.id}:${d.label}`)
      .sort()
      .join("|");
  }

  start(): void {
    this.listeners += 1;
    if (this.timer !== null) return;
    // Four seconds: fast enough that plugging something in feels responsive, slow enough
    // that an idle call is not enumerating audio devices constantly.
    this.timer = setInterval(() => void this.poll(), 4_000);
  }

  stop(): void {
    this.listeners = Math.max(0, this.listeners - 1);
    if (this.listeners > 0 || this.timer === null) return;
    clearInterval(this.timer);
    this.timer = null;
  }

  private async poll(): Promise<void> {
    let devices: NativeMediaDevice[];
    try {
      devices = await invoke<NativeMediaDevice[]>("enumerate_devices");
    } catch (e) {
      // An enumeration that failed says nothing about what is plugged in. Treating it as
      // "no devices" would fire a spurious change and blank the picker.
      debugLog(`device watch: enumeration failed: ${String(e)}`);
      return;
    }
    const next = DeviceWatcher.fingerprintOf(devices);
    const previous = this.fingerprint;
    this.fingerprint = next;
    if (previous !== null && previous !== next) {
      this.dispatchEvent(new Event("devicechange"));
    }
  }
}

const deviceWatcher = new DeviceWatcher();

/** The single `ondevicechange` property handler, kept apart from `addEventListener` ones. */
let ondevicechangeHandler: EventListener | null = null;

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

/** Response shape of the `get_display_media` command. */
interface NativeDisplayMedia {
  videoTrackId: NativeTrackId;
  audioTrackId: NativeTrackId | null;
  /** Which stream the backend actually captured for audio, or null if none was requested. */
  audioScope: "desktop_mix" | "application" | null;
  /**
   * True when the caller asked to capture a single application's audio and the platform
   * could not isolate it, so the backend captured the whole desktop mix instead. This is a
   * privacy-relevant difference from what was requested and must be surfaced, not swallowed.
   */
  audioScopeFallback: boolean;
}

/**
 * Prefix the backend puts on the one `get_display_media` failure that is not a fault: the
 * user closed the system picker without choosing anything.
 *
 * An agreed sentinel rather than a pattern matched against the message text. The two sides
 * have to distinguish "the user declined" from "the portal is broken" -- one is routine and
 * one wants investigating -- and a regex over prose would keep working right up until
 * somebody rewords an error, at which point every cancellation starts being logged as a
 * failure with nothing to indicate the reporting changed. Kept in step with
 * `PICKER_CANCELLED` in `src-tauri/src/commands/screen_capture.rs`.
 */
const PICKER_CANCELLED = "picker_cancelled";

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
            const audioTrack = createSilentAudioTrack();
            if (audioTrack) {
              // Both directions reach Rust: stopping releases the device, and muting stops
              // the media rather than only the icon. Neither happened before — a camera
              // stayed lit after the UI said it was off, and a muted microphone kept being
              // published. The requested deviceId is the one Rust actually opened (see
              // chosen_microphone in media_devices.rs), so it is also what getSettings()
              // reports back -- see wireNativeDeviceId.
              wireNativeTrack(audioTrack, id, "audio", "microphone", nativeConstraints.audio?.deviceId);
              stream.addTrack(audioTrack);
            }
          } else if (id.startsWith("video-")) {
            const videoTrack = await createNativeVideoTrack(id);
            if (videoTrack) {
              wireNativeTrack(videoTrack, id, "video", "camera", nativeConstraints.video?.deviceId);
              stream.addTrack(videoTrack);
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

    async getDisplayMedia(constraints?: DisplayMediaStreamOptions): Promise<MediaStream> {
      console.log("[Elementium] getDisplayMedia called with:", constraints);

      // The backend uses this as the guard on whether it opens any audio capture at all.
      // Defaulting to false when the caller did not ask for audio matters: capturing audio
      // nobody requested is a privacy failure, not a convenience.
      const wantsAudio = !!constraints?.audio;

      try {
        // Get available capture sources
        const sources = await invoke<NativeCaptureSource[]>("get_capture_sources");

        // An empty list is not "no sources exist" -- on Wayland it is returned *by design*
        // (see get_capture_sources on the Rust side): the compositor owns source selection
        // there and shows its own xdg-desktop-portal picker, so the application must not
        // present one of its own. That means an empty list has to be passed through as an
        // absent source id -- letting the portal take over -- rather than papered over with
        // a fabricated "default" that no backend, portal or otherwise, actually defines.
        let sourceId: string | undefined;
        if (sources.length > 0) {
          // Use the first monitor source, or the first available source
          const monitor = sources.find((s) => s.kind === "monitor");
          sourceId = (monitor || sources[0]).id;
        }

        debugLog(
          `getDisplayMedia: calling invoke get_display_media (sourceId=${sourceId ?? "<portal picker>"}, audio=${wantsAudio})...`,
        );
        const result = await invoke<NativeDisplayMedia>("get_display_media", {
          sourceId,
          audio: wantsAudio,
        });
        debugLog(
          `getDisplayMedia: got video=${result.videoTrackId} audio=${result.audioTrackId ?? "none"} ` +
            `scope=${result.audioScope ?? "none"} fallback=${result.audioScopeFallback}`,
        );

        if (result.audioScopeFallback) {
          // The caller asked to capture a single application's audio and the platform could
          // not isolate it, so the backend fell back to capturing the whole desktop mix
          // instead -- audio from windows the user never agreed to share. That has to be
          // disclosed rather than passed through silently as if it were what was requested.
          console.warn(
            "[Elementium] getDisplayMedia: requested application audio was unavailable; " +
              "the backend fell back to capturing the full desktop audio mix instead.",
          );
        }

        // Build the outgoing stream the same way getUserMedia does: a canvas-backed local
        // preview for video (the wire is encoded from the native capture in Rust, not from
        // this canvas), and a silent placeholder for audio (the real audio likewise lives
        // in Rust). See createNativeVideoTrack / createSilentAudioTrack.
        const stream = new MediaStream();

        const videoTrack = await createNativeVideoTrack(result.videoTrackId);
        if (videoTrack) {
          // Stopping a share from the Element Web UI has to reach the backend, or the
          // native capture, its threads and its portal session all keep running after the
          // page thinks the share ended. There is no per-device id for a screen/window
          // share the way there is for a camera or mic, so the capture source id -- the
          // thing that was actually opened -- stands in for it.
          wireNativeTrack(videoTrack, result.videoTrackId, "video", "screen_share", sourceId);
          stream.addTrack(videoTrack);
        }

        if (wantsAudio && result.audioTrackId) {
          const audioTrack = createSilentAudioTrack();
          if (audioTrack) {
            wireNativeTrack(audioTrack, result.audioTrackId, "audio", "screen_share_audio");
            stream.addTrack(audioTrack);
          }
        }
        // If the caller did not ask for audio, or the backend could not provide it, no
        // audio track is added -- silence here, not a silent placeholder standing in for
        // audio nobody requested.

        debugLog(`getDisplayMedia returning stream with ${stream.getTracks().length} tracks`);
        return stream;
      } catch (e) {
        // A cancelled picker is the ordinary way a user declines to share their screen, not
        // a failure -- and unmodified Element Web code already expects it to surface as
        // `NotAllowedError`, because that is what a real browser throws in this case. Logged
        // separately from a genuine capture failure so "the user said no" and "the portal is
        // broken" cannot be confused with each other while reading the log later.
        const message = e instanceof Error ? e.message : String(e);
        if (message.startsWith(PICKER_CANCELLED)) {
          debugLog(`getDisplayMedia: picker cancelled: ${message}`);
        } else {
          console.error("[Elementium] getDisplayMedia failed:", e);
        }
        throw new DOMException("Could not start screen capture", "NotAllowedError");
      }
    },

    // Events come from this shim's own watcher, not from the real `navigator.mediaDevices`.
    // The real one enumerates the webview's devices, which are not the ones this app uses,
    // so it could never fire for a device the user actually plugged in.
    set ondevicechange(handler: ((ev: Event) => unknown) | null) {
      if (ondevicechangeHandler) {
        deviceWatcher.removeEventListener("devicechange", ondevicechangeHandler);
        deviceWatcher.stop();
      }
      ondevicechangeHandler = handler as EventListener | null;
      if (ondevicechangeHandler) {
        deviceWatcher.addEventListener("devicechange", ondevicechangeHandler);
        deviceWatcher.start();
      }
    },
    get ondevicechange(): ((ev: Event) => unknown) | null {
      return ondevicechangeHandler as ((ev: Event) => unknown) | null;
    },
    addEventListener: (type: string, listener: EventListenerOrEventListenerObject | null) => {
      if (type !== "devicechange" || !listener) return;
      deviceWatcher.addEventListener(type, listener);
      deviceWatcher.start();
    },
    removeEventListener: (type: string, listener: EventListenerOrEventListenerObject | null) => {
      if (type !== "devicechange" || !listener) return;
      deviceWatcher.removeEventListener(type, listener);
      deviceWatcher.stop();
    },
    dispatchEvent: (event: Event) => deviceWatcher.dispatchEvent(event),
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
 * Create a silent placeholder audio track. The real audio for both the microphone
 * (getUserMedia) and a screen/window share's captured audio (getDisplayMedia) lives
 * entirely in Rust -- this track exists only so the page has a MediaStreamTrack object to
 * hold, mute, and hand to the rest of the WebRTC stack; it carries no signal of its own.
 */
function createSilentAudioTrack(): MediaStreamTrack | null {
  try {
    const audioCtx = new AudioContext();
    const oscillator = audioCtx.createOscillator();
    const dest = audioCtx.createMediaStreamDestination();
    oscillator.connect(dest);
    oscillator.frequency.value = 0;
    oscillator.start();
    const audioTrack = dest.stream.getAudioTracks()[0] ?? null;
    debugLog(`audio track added: ${audioTrack?.id}`);
    return audioTrack;
  } catch (e) {
    debugLog(`audio track error: ${e}`);
    return null;
  }
}

/**
 * Create a canvas-backed video track fed by native frames for `trackId`.
 *
 * Shared by getUserMedia (camera) and getDisplayMedia (screen/window share): both need the
 * same sequence -- size the canvas from the real capture geometry *before* calling
 * captureStream, attach it in-viewport at zero opacity, draw an initial frame, then pump
 * real frames from Rust -- and both have independently hit the bugs that sequence avoids
 * (see the comments below and in createCanvasTrack). Fixed once here rather than twice.
 */
async function createNativeVideoTrack(trackId: string): Promise<MediaStreamTrack | null> {
  debugLog(`video track ${trackId}: creating canvas...`);
  const canvas = document.createElement("canvas");
  // Size the canvas to the real capture geometry *before* captureStream, which fixes the
  // resulting track's frame size at the moment it is called. Resizing afterwards leaves the
  // track describing one geometry while the backing store holds another: rows are then read
  // at the wrong stride, so the picture shears into horizontal bands whose colours rotate as
  // the byte offset drifts through the RGBA quad. That is a rendering fault, not a capture
  // fault -- the captured frames are pixel-perfect.
  const geometry = await firstFrameGeometry(trackId);
  canvas.width = geometry?.width ?? 640;
  canvas.height = geometry?.height ?? 480;
  debugLog(`video track: canvas sized ${canvas.width}x${canvas.height}`);
  // In the DOM, inside the viewport, but invisible.
  //
  // It has to be attached for `captureStream` to work reliably in WebKitGTK, and it is kept
  // *within* the viewport deliberately: an element parked at -9999px is never painted, and a
  // capture implementation that samples the compositor rather than the backing store can
  // then return partially-updated tiles -- which looks exactly like the horizontal banding
  // being chased here. Zero opacity keeps it painted and unseen.
  canvas.style.position = "fixed";
  canvas.style.top = "0";
  canvas.style.left = "0";
  canvas.style.width = "1px";
  canvas.style.height = "1px";
  canvas.style.opacity = "0";
  canvas.style.zIndex = "-1";
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
  // Manually driven: a frame is emitted only once a draw has completed, so the track can
  // never sample a half-written canvas. See `createCanvasTrack`.
  const canvasTrack = createCanvasTrack(canvas);
  const videoTrack = canvasTrack.track;
  debugLog(`video track: captureStream returned track? ${!!videoTrack} readyState=${videoTrack?.readyState}`);
  if (videoTrack) {
    // Start fetching real frames from the Rust backend
    startLocalVideoFrameFetch(canvas, trackId, canvasTrack.present);
  }
  return videoTrack;
}

/**
 * Make `track.stop()` also tear down the native capture behind it.
 *
 * `MediaStreamTrack.stop()` only stops the local (canvas or placeholder-audio) object;
 * nothing about it reaches Rust on its own. Without this, ending a share from the Element
 * Web UI leaves the native capture, its threads and (for a portal-backed share) its portal
 * session running after the page believes it has stopped. `nativeTrackId` is the backend's
 * id for the real capture, which is deliberately not the same as `track.id` (the canvas
 * track's own, browser-assigned id) -- the wrong one would tell Rust to stop a capture that
 * doesn't exist and leave the real one running regardless.
 */
/**
 * Preview loops, by the track they are drawing, so stopping a track can stop its loop.
 *
 * Without this the loop outlives the track: a stopper was written and never called, so
 * every reconnect left another fetch running at 30 IPC calls a second, for the life of the
 * page. One session had three of them polling for tracks that no longer existed.
 */
const previewStoppers = new Map<string, () => void>();

/**
 * FIX 1: how many live JS handles point at each native pipeline.
 *
 * A cloned track (matrix-js-sdk's `CallFeed.clone()` makes one, and it ships in the bundle)
 * is a second `MediaStreamTrack` object backed by the *same* Rust capture. If `stop()` on
 * either handle unconditionally told the backend to tear the pipeline down, whichever handle
 * stopped first would silently kill the other's media. Counted by native track id rather
 * than by JS object, so "last handle out turns off the lights": the pipeline, the preview
 * fetch loop, and the `stop_track` invoke only fire once every handle sharing that id has
 * stopped.
 */
const nativeTrackRefCounts = new Map<NativeTrackId, number>();

function wireStopToBackend(track: MediaStreamTrack, nativeTrackId: NativeTrackId): void {
  nativeTrackRefCounts.set(nativeTrackId, (nativeTrackRefCounts.get(nativeTrackId) ?? 0) + 1);
  const originalStop = track.stop.bind(track);
  track.stop = () => {
    // The local object always stops -- that part is per-handle, not shared.
    originalStop();
    const remaining = (nativeTrackRefCounts.get(nativeTrackId) ?? 1) - 1;
    if (remaining > 0) {
      // Another clone of this same native track is still live; the pipeline stays up.
      nativeTrackRefCounts.set(nativeTrackId, remaining);
      return;
    }
    nativeTrackRefCounts.delete(nativeTrackId);
    previewStoppers.get(nativeTrackId)?.();
    previewStoppers.delete(nativeTrackId);
    invoke("stop_track", { trackId: nativeTrackId }).catch((e) => {
      console.error(`[Elementium] stop_track(${nativeTrackId}) failed:`, e);
    });
  };
}

/**
 * FIX 1 (continued): make `clone()` return a fully-wired track instead of an inert copy.
 *
 * The browser's own `clone()` (which this calls first, via the bound reference taken before
 * overriding) knows nothing about Rust: the copy it returns has no `TRACK_SOURCE` tag, its
 * `stop()` releases nothing native, and its `enabled` setter mutes nothing but itself. Every
 * fix in this file is re-applied to the clone with the *same* nativeTrackId, kind, source and
 * deviceId as the track it was cloned from, so the two JS objects behave as two handles onto
 * one pipeline -- including recursively, if the clone itself is cloned again.
 */
function wireCloneToBackend(
  track: MediaStreamTrack,
  nativeTrackId: NativeTrackId,
  kind: "audio" | "video",
  source: string,
  deviceId: string | undefined,
): void {
  const originalClone = track.clone.bind(track);
  track.clone = (): MediaStreamTrack => {
    const cloned = originalClone();
    wireNativeTrack(cloned, nativeTrackId, kind, source, deviceId);
    return cloned;
  };
}

/**
 * FIX 2: report the device Rust actually opened, not the synthetic canvas/AudioContext id.
 *
 * livekit-client verifies a device switch by checking
 * `track.getSettings().deviceId === <requested id>`. Our `deviceId` is whatever
 * `canvas.captureStream()` or `AudioContext`'s destination stream assigned -- an id for a
 * local rendering object, never the id of a camera or microphone. That made every switch
 * check read as a mismatch even when the backend had switched correctly. `deviceId` here is
 * the one already resolved at track-creation time from the caller's constraints (or, for a
 * screen/window share, the capture source id) -- the actual thing that was opened -- so
 * nothing is invented. Every other field still comes from the real implementation.
 */
function wireNativeDeviceId(track: MediaStreamTrack, deviceId: string | undefined): void {
  if (!deviceId) return; // Unknown (e.g. default device): leave the underlying report alone.
  const originalGetSettings = track.getSettings.bind(track);
  track.getSettings = (): MediaTrackSettings => ({
    ...originalGetSettings(),
    deviceId,
  });
}

/**
 * FIX 3: `applyConstraints` and `getCapabilities` have nothing native behind them.
 *
 * `grep -n "tauri::command" src-tauri/src/commands/media_devices.rs` turns up
 * `enumerate_devices`, `get_user_media`, `stop_track`, `get_video_frame` and the screen-share
 * pipeline/mute commands -- nothing that can reconfigure a running capture or report its real
 * capability ranges. `applyConstraints` resolving silently while changing nothing about real
 * capture is exactly the "plausible stub" shape this codebase keeps tripping over, so instead
 * of pretending, the request is logged loudly (which keys were asked for, and that they will
 * not reach Rust) and still resolved -- rejecting would break callers, notably livekit-client,
 * that treat `applyConstraints` as best-effort and do not expect it to throw. A native
 * implementation would need a new command (e.g. `apply_track_constraints`) that can
 * reconfigure or restart the running pipeline in media_devices.rs, plus one that reports the
 * opened device's real capability ranges for `getCapabilities` instead of the canvas/
 * AudioContext stand-in's.
 */
function wireConstraintDishonesty(track: MediaStreamTrack, kind: "audio" | "video"): void {
  const originalApply = track.applyConstraints?.bind(track);
  track.applyConstraints = async (constraints?: MediaTrackConstraints): Promise<void> => {
    const keys = constraints ? Object.keys(constraints) : [];
    console.warn(
      `[Elementium] applyConstraints(${kind}) cannot reach native capture: [${keys.join(", ")}] ` +
        `will change the local ${kind === "video" ? "canvas" : "AudioContext"} stand-in at most, ` +
        "not the device Rust is actually capturing from.",
    );
    if (originalApply) {
      // Best-effort against the stand-in, matching what the warning above told the caller.
      await originalApply(constraints).catch(() => {});
    }
  };

  const originalGetCapabilities = track.getCapabilities?.bind(track);
  if (originalGetCapabilities) {
    track.getCapabilities = (): MediaTrackCapabilities => {
      console.warn(
        `[Elementium] getCapabilities(${kind}) reports the local stand-in's capabilities ` +
          "(canvas geometry / AudioContext defaults), not the camera or microphone's real ranges.",
      );
      return originalGetCapabilities();
    };
  }
}

/**
 * Apply every per-instance fix in this file to one track: FIX 1 (stop refcounting and
 * clone wiring), FIX 2 (native deviceId in getSettings), FIX 3 (honest
 * applyConstraints/getCapabilities), plus the pre-existing stop and mute wiring. Called once
 * when a track is created, and again -- by `wireCloneToBackend` -- every time it is cloned,
 * so a clone ends up wired exactly like the track it came from.
 */
function wireNativeTrack(
  track: MediaStreamTrack,
  nativeTrackId: NativeTrackId,
  kind: "audio" | "video",
  source: string,
  deviceId?: string,
): void {
  wireStopToBackend(track, nativeTrackId);
  wireMuteToBackend(track, kind, source);
  wireCloneToBackend(track, nativeTrackId, kind, source, deviceId);
  wireNativeDeviceId(track, deviceId);
  wireConstraintDishonesty(track, kind);
}

/**
 * Make `track.enabled = false` actually stop the media.
 *
 * This is how every caller in this stack mutes: livekit-client and matrix-js-sdk both set
 * `enabled` on the `MediaStreamTrack` they were handed. That object is a local preview —
 * the frames and samples on the wire come from a capture pipeline in Rust, which has never
 * seen it. So muting changed the icon and nothing else: the user believed they were muted
 * and everyone else could still hear and see them.
 *
 * `enabled` is an accessor on `MediaStreamTrack.prototype`, so it is redefined on this
 * instance: the original still governs the canvas preview (a muted camera should stop
 * showing itself locally too), and the backend is told either way.
 *
 * Failures are logged loudly rather than swallowed. A mute that did not reach the backend
 * is precisely the fault being fixed, and a silent catch here would restore it.
 */
/**
 * Find the `enabled` accessor, wherever in the prototype chain it lives.
 *
 * It is declared on `MediaStreamTrack.prototype`, but the camera track is the one
 * `canvas.captureStream()` returns, whose immediate prototype is
 * `CanvasCaptureMediaStreamTrack.prototype` -- one level below. Looking only at the
 * immediate prototype found nothing for exactly that track, so video mute was reported as
 * unwireable and the camera went on capturing and publishing while the user believed it
 * was off. The microphone, whose track has no such subclass, was wired correctly, which is
 * why this survived the hot-mic fix.
 */
function enabledDescriptor(track: MediaStreamTrack): PropertyDescriptor | undefined {
  let proto: object | null = Object.getPrototypeOf(track) as object | null;
  while (proto) {
    const descriptor = Object.getOwnPropertyDescriptor(proto, "enabled");
    if (descriptor) return descriptor;
    proto = Object.getPrototypeOf(proto) as object | null;
  }
  return undefined;
}

/**
 * The property a track carries its backend source on, read by `addTransceiver`.
 *
 * A `MediaStreamTrack`'s own `id` is a browser UUID, so nothing downstream can tell the
 * camera from the screen share -- and the backend was therefore labelling *every* sending
 * video transceiver as the camera. Two video tracks then competed for one m-line: the
 * share went out down the camera's slot, so the far end saw the sender's face in the
 * screen-share tile and a black square where the camera should be.
 */
export const TRACK_SOURCE = "__elementiumSource";

function wireMuteToBackend(
  track: MediaStreamTrack,
  kind: "audio" | "video",
  source: string,
): void {
  // Recorded here because this is the one place that already knows which of the user's
  // tracks this is, for every track type.
  (track as unknown as Record<string, unknown>)[TRACK_SOURCE] = source;
  const original = enabledDescriptor(track);
  if (!original?.get || !original.set) {
    console.warn("[Elementium] MediaStreamTrack.enabled is not an accessor; mute will not reach the backend");
    return;
  }
  const get = original.get.bind(track);
  const set = original.set.bind(track);
  Object.defineProperty(track, "enabled", {
    configurable: true,
    get,
    set: (value: boolean) => {
      set(value);
      invoke("set_capture_muted", { kind, source, muted: !value }).catch((e) => {
        console.error(`[Elementium] set_capture_muted(${kind}/${source}) failed:`, e);
      });
    },
  });
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
      const buf = frameBytes(await invoke<unknown>("get_video_frame", { trackId }));
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
  let windowDrawn = 0;
  let windowPresentMs = 0;
  let windowMismatches = 0;
  let lastMismatch = "none";
  let lastGeometry = "none";

  const fetchLoop = async () => {
    if (!running) return;
    const started = Date.now();

    try {
      const buf = frameBytes(await invoke<unknown>("get_video_frame", { trackId }));
      frameCount++;
      if (buf && buf.byteLength > 8) {
        const view = new DataView(buf);
        const width = view.getUint32(0, true);
        const height = view.getUint32(4, true);

        if (width > 1 && height > 1) {
          // The header's geometry and the payload's length must agree exactly. If they do
          // not, every row is read at the wrong stride and the picture shears into
          // horizontal bands -- the symptom this whole area keeps producing. Checked
          // rather than assumed, and refused rather than drawn, because a wrong picture
          // is harder to diagnose than a missing one.
          const payload = buf.byteLength - 8;
          const expected = width * height * 4;
          if (payload !== expected) {
            windowMismatches += 1;
            lastMismatch = `${width}x${height} wants ${expected}, got ${payload}`;
            return scheduleNext();
          }
          windowDrawn += 1;
          lastGeometry = `${width}x${height} into ${canvas.width}x${canvas.height}`;
          const rgba = new Uint8ClampedArray(buf.slice(8));
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
          //
          // Timed separately from the fetch: the preview dropped from 30fps to 6.6fps
          // when this was introduced, and "the IPC got slower" and "requestFrame blocks"
          // are indistinguishable from one combined number.
          const presentStarted = Date.now();
          present();
          windowPresentMs += Date.now() - presentStarted;
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
          `(${canvas.width}x${canvas.height}), drawn=${windowDrawn} ` +
          `present=${(windowPresentMs / Math.max(1, windowDrawn)).toFixed(1)}ms ` +
          `mismatched=${windowMismatches} last=${lastMismatch} geom=${lastGeometry}`,
      );
      windowStart = Date.now();
      windowFrames = 0;
      windowFetchMs = 0;
      windowDrawn = 0;
      windowPresentMs = 0;
      windowMismatches = 0;
    }

    scheduleNext(elapsed);
  };

  // Schedule against the target period rather than sleeping a fixed amount *after* the
  // work: the previous form made the real period "IPC time + 33ms", so a 30ms fetch halved
  // the frame rate. A frame that took longer than the period is followed immediately by
  // the next.
  function scheduleNext(elapsed = 0): void {
    if (!running) return;
    timerId = setTimeout(fetchLoop, Math.max(0, TARGET_FRAME_MS - elapsed));
  }

  debugLog(`fetchLoop: starting for ${trackId}`);
  timerId = setTimeout(fetchLoop, TARGET_FRAME_MS);

  // Store cleanup reference on the canvas for stop_track
  previewStoppers.set(trackId, () => {
    running = false;
    if (timerId !== null) clearTimeout(timerId);
    debugLog(`preview loop for ${trackId} stopped with its track`);
  });
  (canvas as unknown as Record<string, unknown>).__stopFetch = () => {
    running = false;
    if (timerId !== null) clearTimeout(timerId);
  };
}
