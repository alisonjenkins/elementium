//! LiveKit SFU WebSocket signaling client.
//!
//! Connects to the LiveKit SFU via WebSocket, sends/receives protobuf-encoded
//! `SignalRequest`/`SignalResponse` messages, and exposes an mpsc channel for
//! the room to consume incoming messages.

use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use livekit_protocol::signal_request;
use livekit_protocol::signal_response;
use livekit_protocol::{SignalRequest, SignalResponse};

#[derive(Error, Debug)]
pub enum SignalError {
    #[error("Connection failed: {0}")]
    Connection(String),
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("Send channel closed")]
    ChannelClosed,
}

/// Incoming signal messages from the SFU, delivered via channel.
pub type SignalReceiver = mpsc::UnboundedReceiver<signal_response::Message>;

/// Handle for sending signal requests to the SFU.
#[derive(Clone)]
pub struct SignalSender {
    tx: mpsc::UnboundedSender<SignalRequest>,
}

impl SignalSender {
    /// Send a signaling request to the SFU.
    pub fn send(&self, msg: signal_request::Message) -> Result<(), SignalError> {
        let req = SignalRequest {
            message: Some(msg),
        };
        self.tx.send(req).map_err(|_| SignalError::ChannelClosed)
    }
}

/// LiveKit SFU WebSocket signaling client.
pub struct SignalClient {
    sender: SignalSender,
    receiver: Option<SignalReceiver>,
    shutdown_tx: Option<mpsc::Sender<()>>,
}

impl SignalClient {
    /// Connect to the LiveKit SFU.
    ///
    /// Opens a WebSocket to `wss://<sfu>/rtc?access_token=<token>&auto_subscribe=true&sdk=rust&protocol=9`.
    /// Spawns a background task that reads from the WebSocket and forwards
    /// `SignalResponse` messages to the returned receiver channel.
    pub async fn connect(sfu_url: &str, token: &str) -> Result<Self, SignalError> {
        let ws_url = build_ws_url(sfu_url, token)?;
        tracing::info!(url = %ws_url, "Connecting to LiveKit SFU");

        let (ws_stream, _resp) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| SignalError::Connection(format!("WebSocket connect failed: {e}")))?;

        tracing::info!("WebSocket connected to LiveKit SFU");

        let (write, read) = ws_stream.split();

        // Channel: room → WebSocket (outgoing requests)
        let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalRequest>();
        // Channel: WebSocket → room (incoming responses)
        let (in_tx, in_rx) = mpsc::unbounded_channel::<signal_response::Message>();
        // Shutdown signal
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        // Spawn writer task: reads from out_rx, encodes, sends to WebSocket
        tokio::spawn(ws_writer_loop(out_rx, write));

        // Spawn reader task: reads from WebSocket, decodes, sends to in_tx
        tokio::spawn(ws_reader_loop(read, in_tx, shutdown_rx));

        let sender = SignalSender { tx: out_tx };

        Ok(Self {
            sender,
            receiver: Some(in_rx),
            shutdown_tx: Some(shutdown_tx),
        })
    }

    /// Take the signal receiver (can only be taken once).
    pub fn take_receiver(&mut self) -> Option<SignalReceiver> {
        self.receiver.take()
    }

    /// Get a cloneable sender for sending requests to the SFU.
    pub fn sender(&self) -> SignalSender {
        self.sender.clone()
    }

    /// Gracefully disconnect from the SFU.
    pub async fn disconnect(&mut self) {
        tracing::info!("Disconnecting from LiveKit SFU");
        // Send Leave request
        let _ = self.sender.send(signal_request::Message::Leave(
            livekit_protocol::LeaveRequest {
                can_reconnect: false,
                reason: livekit_protocol::DisconnectReason::ClientInitiated as i32,
                ..Default::default()
            },
        ));
        // Signal shutdown to reader
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
    }
}

/// Build the WebSocket URL for LiveKit signaling.
fn build_ws_url(sfu_url: &str, token: &str) -> Result<String, SignalError> {
    let mut url = Url::parse(sfu_url)
        .map_err(|e| SignalError::Connection(format!("Invalid SFU URL: {e}")))?;

    // Ensure we use wss:// scheme
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|_| SignalError::Connection("Failed to set scheme".into()))?,
        "http" => url
            .set_scheme("ws")
            .map_err(|_| SignalError::Connection("Failed to set scheme".into()))?,
        "wss" | "ws" => {}
        s => {
            return Err(SignalError::Connection(format!(
                "Unsupported URL scheme: {s}"
            )));
        }
    }

    // Set the path to /rtc
    url.set_path("/rtc");

    // Add query parameters
    url.query_pairs_mut()
        .append_pair("access_token", token)
        .append_pair("auto_subscribe", "true")
        .append_pair("sdk", "rust")
        .append_pair("protocol", "9");

    Ok(url.to_string())
}

type WsWrite = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<TcpStream>>,
    WsMessage,
>;
type WsRead = futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// Background task: reads outgoing SignalRequests, encodes to protobuf, sends to WebSocket.
async fn ws_writer_loop(
    mut rx: mpsc::UnboundedReceiver<SignalRequest>,
    mut ws_write: WsWrite,
) {
    while let Some(req) = rx.recv().await {
        let mut buf = Vec::with_capacity(req.encoded_len());
        if let Err(e) = req.encode(&mut buf) {
            tracing::error!("Failed to encode SignalRequest: {e}");
            continue;
        }
        if let Err(e) = ws_write.send(WsMessage::Binary(buf.into())).await {
            tracing::error!("WebSocket send error: {e}");
            break;
        }
    }
    tracing::info!("Signal writer loop ended");
    let _ = ws_write.close().await;
}

/// Background task: reads from WebSocket, decodes SignalResponse, forwards to channel.
async fn ws_reader_loop(
    mut ws_read: WsRead,
    tx: mpsc::UnboundedSender<signal_response::Message>,
    mut shutdown_rx: mpsc::Receiver<()>,
) {
    loop {
        tokio::select! {
            msg = ws_read.next() => {
                match msg {
                    Some(Ok(WsMessage::Binary(data))) => {
                        match SignalResponse::decode(data.as_ref()) {
                            Ok(resp) => {
                                if let Some(message) = resp.message {
                                    if tx.send(message).is_err() {
                                        tracing::info!("Signal receiver dropped, stopping reader");
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to decode SignalResponse: {e}");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        tracing::info!("WebSocket closed by server");
                        break;
                    }
                    Some(Ok(_)) => {
                        // Ping/Pong/Text frames — ignore
                    }
                    Some(Err(e)) => {
                        tracing::error!("WebSocket read error: {e}");
                        break;
                    }
                    None => {
                        tracing::info!("WebSocket stream ended");
                        break;
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("Signal reader received shutdown");
                break;
            }
        }
    }
    tracing::info!("Signal reader loop ended");
}
