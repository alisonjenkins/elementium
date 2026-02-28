pub mod audio_pipeline;
pub mod engine;
pub mod livekit;
pub mod peer_connection;
pub mod video_pipeline;

pub use audio_pipeline::AudioPipeline;
pub use engine::{IoCommand, VideoFrameBuffer, WebRtcEngine};
pub use livekit::{LiveKitRoom, RoomEvent};
pub use peer_connection::{PcEvent, PeerConnectionHandle};
pub use video_pipeline::VideoPipeline;

// Re-export E2EE types used by Tauri commands
pub use elementium_e2ee::{E2eeContext, E2eeOptions};
