use str0m::Rtc;

/// Wraps a str0m `Rtc` instance with Elementium-specific state.
pub struct PeerConnection {
    pub id: String,
    pub rtc: Rtc,
}

impl PeerConnection {
    pub fn new(id: String, rtc: Rtc) -> Self {
        Self { id, rtc }
    }
}
