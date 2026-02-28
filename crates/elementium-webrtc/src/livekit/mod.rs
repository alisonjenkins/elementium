//! LiveKit SFU client for group calls.
//!
//! This module implements the LiveKit signaling protocol over WebSocket
//! to connect to a LiveKit SFU server. It replaces the JavaScript
//! livekit-client library that Element Call normally uses.
//!
//! ## Architecture
//!
//! 1. Receive SFU URL + JWT from Element Call widget via JS bridge
//! 2. Open WebSocket to LiveKit SFU
//! 3. Send/receive protobuf-encoded signaling messages
//! 4. Manage room state: participants, tracks, subscriptions
//! 5. Use str0m for the actual WebRTC transport to the SFU
//!
//! ## Key message types (from LiveKit protocol)
//!
//! - `JoinRequest` / `JoinResponse` — Room join
//! - `AddTrackRequest` — Publish a track
//! - `TrackPublishedResponse` — Track accepted by SFU
//! - `SignalRequest` / `SignalResponse` — SDP offer/answer exchange
//! - `UpdateSubscription` — Subscribe/unsubscribe to remote tracks
//! - `ParticipantUpdate` — Participant join/leave events

pub mod signaling;

pub use signaling::LiveKitClient;
