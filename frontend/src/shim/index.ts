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
import { Room, RoomEvent, ConnectionState, Track, RemoteTrack, LocalTrack, Participant, RemoteParticipant, LocalParticipant, TrackKind, TrackSource } from "./livekit-bridge";

// Secret storage must be first — before any code reads localStorage
setupSecretStorageShim();
setupWebRtcShim();
setupMediaDevicesShim();

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

console.log("[Elementium] Native shims installed");
