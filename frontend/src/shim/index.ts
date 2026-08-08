/**
 * Elementium shim entry point.
 *
 * Built as an IIFE and injected into Element Web's index.html
 * before any other scripts, so that our native WebRTC/media shims
 * are in place before Element Web initializes.
 */

import { setupSecretStorageShim } from "./secret-storage";
import { setupWebRtcShim } from "./webrtc-shim";
import { setupMediaDevicesShim } from "./media-devices";
import { setupConsoleBridge } from "./console-bridge";
import { setupE2eeBridge } from "./e2ee-bridge";
import { setupMembershipLog } from "./membership-log";
import { Room, RoomEvent, ConnectionState, Track, RemoteTrack, LocalTrack, Participant, RemoteParticipant, LocalParticipant, TrackKind, TrackSource } from "./livekit-bridge";
import { install, isPatched, record } from "./install-report";

// Each install is recorded with a predicate evaluated *after* setup, not with the fact that
// setup returned. A shim that ran and attached to nothing is the failure an Element Web
// upgrade produces, and it is invisible to "did the page load".
// See ./install-report.ts and specs/007-element-web-upgrade/contracts/shim-install.md.

// Set up console bridge to forward JS console output to Rust (works in iframes too)
install("console-bridge", "console.log", setupConsoleBridge, () => isPatched(console.log));

// Secret storage must be first — before any code reads localStorage
install(
  "secret-storage",
  "Storage.prototype.setItem",
  setupSecretStorageShim,
  () => isPatched(Storage.prototype.setItem),
  // It bails out without Tauri IPC, which is a legitimate outcome in a plain browser and
  // must not read as a failure to attach.
  () => Boolean((window as unknown as Record<string, unknown>)["__TAURI_INTERNALS__"]),
);

install(
  "webrtc",
  "RTCPeerConnection",
  setupWebRtcShim,
  // Checked by "is this still the browser's own", not by class name: the bundle is
  // minified, so the name is mangled and an identity check would fail on every real build
  // while passing in a dev server. That is the wrong way round for a release gate.
  () => isPatched((window as unknown as Record<string, unknown>)["RTCPeerConnection"]),
);

install(
  "media-devices",
  "navigator.mediaDevices.getUserMedia",
  setupMediaDevicesShim,
  () => isPatched(navigator.mediaDevices?.getUserMedia),
);

// Must be installed before Element Call imports its keys: the bridge captures raw key
// material at `crypto.subtle.importKey`, which only works for imports it front-runs.
install(
  "e2ee-bridge",
  "Worker.prototype.postMessage, crypto.subtle.importKey",
  setupE2eeBridge,
  () => isPatched(Worker.prototype.postMessage) && isPatched(crypto.subtle.importKey),
);

// Puts joins and leaves in the same stream as key changes, so a silence that follows a
// rotation can be attributed to the membership event that caused it.
install(
  "membership-log",
  "matrix client RoomState.events",
  setupMembershipLog,
  () =>
    Boolean(
      (window as unknown as Record<string, unknown>)["__elementium_membership_logged"],
    ),
);

// Expose LiveKit shim on window for Element Call widget integration
(window as unknown as Record<string, unknown>)["__elementium_livekit"] = {
  Room,
  RoomEvent,
  ConnectionState,
  Track,
  RemoteTrack,
  LocalTrack,
  Participant,
  RemoteParticipant,
  LocalParticipant,
  TrackKind,
  TrackSource,
};
record(
  "livekit-bridge",
  "window.__elementium_livekit",
  Boolean((window as unknown as Record<string, unknown>)["__elementium_livekit"]),
);

const report = (window as unknown as Record<string, Record<string, { installed: boolean; skipped?: boolean }>>)["__elementium_shims"];
const failed = Object.entries(report ?? {}).filter(([, s]) => !s.installed && !s.skipped);
if (failed.length > 0) {
  console.error(`[Elementium] ${failed.length} shim(s) did not install: ${failed.map(([n]) => n).join(", ")}`);
} else {
  console.log("[Elementium] Native shims installed");
}
