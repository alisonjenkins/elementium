/**
 * LiveKit bridge — intercepts Element Call's LiveKit client initialization
 * and routes it to the native Rust LiveKit implementation.
 *
 * Element Call normally uses the `livekit-client` JS library. This bridge
 * intercepts the SFU URL + JWT and passes them to the Rust backend, which
 * handles all LiveKit signaling and WebRTC transport natively.
 */

// import { invoke } from "@tauri-apps/api/core";

/**
 * Intercept a LiveKit room connection from Element Call.
 *
 * Called when Element Call attempts to connect to a LiveKit SFU.
 * We take the connection parameters and hand them to the Rust backend.
 */
export async function connectToLiveKit(sfuUrl: string, _token: string): Promise<void> {
  console.log(`[Elementium] Connecting to LiveKit SFU: ${sfuUrl}`);

  // TODO: Implement Tauri command for LiveKit connection
  // await invoke("livekit_connect", { sfuUrl, token: _token });
}

/**
 * Disconnect from the current LiveKit room.
 */
export async function disconnectFromLiveKit(): Promise<void> {
  console.log("[Elementium] Disconnecting from LiveKit SFU");

  // TODO: Implement Tauri command for LiveKit disconnection
  // await invoke("livekit_disconnect");
}
