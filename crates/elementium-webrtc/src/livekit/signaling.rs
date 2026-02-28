use thiserror::Error;

#[derive(Error, Debug)]
pub enum LiveKitError {
    #[error("Connection failed: {0}")]
    Connection(String),
    #[error("Signaling error: {0}")]
    Signaling(String),
    #[error("Room error: {0}")]
    Room(String),
}

/// LiveKit SFU WebSocket signaling client.
pub struct LiveKitClient {
    sfu_url: String,
    _token: String,
    connected: bool,
}

impl LiveKitClient {
    pub fn new(sfu_url: String, token: String) -> Self {
        Self {
            sfu_url,
            _token: token,
            connected: false,
        }
    }

    /// Connect to the LiveKit SFU via WebSocket.
    pub async fn connect(&mut self) -> Result<(), LiveKitError> {
        tracing::info!(url = %self.sfu_url, "Connecting to LiveKit SFU");
        // TODO: Establish WebSocket connection using tokio-tungstenite
        // TODO: Send JoinRequest with token
        // TODO: Handle JoinResponse
        self.connected = true;
        Ok(())
    }

    /// Disconnect from the SFU.
    pub async fn disconnect(&mut self) -> Result<(), LiveKitError> {
        tracing::info!("Disconnecting from LiveKit SFU");
        self.connected = false;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }
}
