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
//! ## Dual PeerConnection model
//!
//! LiveKit uses two PeerConnections per client:
//! - **Publisher**: Client → SFU. Client creates SDP offers when publishing tracks.
//! - **Subscriber**: SFU → Client. SFU creates SDP offers when remote tracks appear.

pub mod room;
pub mod signaling;
pub mod transport;

pub use room::{LiveKitRoom, RoomEvent};
pub use signaling::{SignalClient, SignalError};
pub use transport::Transport;
