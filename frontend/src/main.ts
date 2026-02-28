/**
 * Elementium frontend entry point.
 *
 * This bootstraps Element Web inside the Tauri webview and injects
 * the WebRTC shim that routes media operations to the Rust backend.
 */

import { setupWebRtcShim } from "./shim/webrtc-shim";
import { setupMediaDevicesShim } from "./shim/media-devices";
import { Room, RoomEvent } from "./shim/livekit-bridge";

// Install shims before Element Web loads
setupWebRtcShim();
setupMediaDevicesShim();

// Make the LiveKit bridge available globally for Element Call widget
(window as unknown as Record<string, unknown>)["__elementium_livekit"] = { Room, RoomEvent };

console.log("[Elementium] Shims installed (WebRTC, mediaDevices, LiveKit), loading Element Web...");

// For now, show a placeholder until Element Web is integrated
const loading = document.getElementById("loading")!;
loading.textContent = "Elementium is running. Element Web integration pending.";
