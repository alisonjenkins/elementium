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

interface NativeTrackId {
  "0": string;
}

/**
 * Install the media devices shim, replacing navigator.mediaDevices.
 */
export function setupMediaDevicesShim(): void {
  const original = navigator.mediaDevices;

  const shimmedDevices: MediaDevices = {
    ...original,

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
            (constraints.video as MediaTrackConstraints).width as number | undefined : undefined,
          height: typeof constraints.video === "object" ?
            (constraints.video as MediaTrackConstraints).height as number | undefined : undefined,
          frameRate: typeof constraints.video === "object" ?
            (constraints.video as MediaTrackConstraints).frameRate as number | undefined : undefined,
        } : null,
      };

      try {
        const trackIds = await invoke<NativeTrackId[]>("get_user_media", {
          constraints: nativeConstraints,
        });
        // Create a synthetic MediaStream with dummy tracks
        // Real tracks are handled entirely in Rust
        const stream = new MediaStream();
        console.log(`[Elementium] getUserMedia returned ${trackIds.length} tracks`);
        return stream;
      } catch (e) {
        console.error("[Elementium] getUserMedia failed:", e);
        throw new DOMException("Could not start media source", "NotAllowedError");
      }
    },

    async getDisplayMedia(_constraints?: DisplayMediaStreamOptions): Promise<MediaStream> {
      console.log("[Elementium] getDisplayMedia called");
      try {
        // First get available sources for a picker
        await invoke("get_capture_sources");
        // TODO: Show source picker UI, let user select
        // For now, just try to start with the first source
        await invoke("get_display_media", { sourceId: "default" });
        return new MediaStream();
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
