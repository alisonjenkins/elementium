use std::collections::HashMap;
use std::net::UdpSocket;
use std::time::Instant;

use str0m::net::Protocol;
use str0m::{Event, Input, Output, Rtc};
use tokio::sync::mpsc;

use crate::peer_connection::PeerConnection;

/// The WebRTC engine runs the I/O loop for all active peer connections.
///
/// str0m is sans-I/O: we drive it by feeding network packets and time,
/// and it tells us what to send. This engine owns the UDP sockets and
/// the polling loop.
pub struct WebRtcEngine {
    connections: HashMap<String, PeerConnection>,
    _shutdown_tx: Option<mpsc::Sender<()>>,
}

impl WebRtcEngine {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            _shutdown_tx: None,
        }
    }

    /// Create a new peer connection and return its ID.
    pub fn create_connection(&mut self, id: String) -> &mut PeerConnection {
        let rtc = Rtc::builder().build(Instant::now());
        let pc = PeerConnection::new(id.clone(), rtc);
        self.connections.entry(id).or_insert(pc)
    }

    /// Get a mutable reference to a peer connection by ID.
    pub fn connection(&mut self, id: &str) -> Option<&mut PeerConnection> {
        self.connections.get_mut(id)
    }

    /// Remove and close a peer connection.
    pub fn remove_connection(&mut self, id: &str) -> Option<PeerConnection> {
        self.connections.remove(id)
    }

    /// Run the I/O loop for a single peer connection.
    ///
    /// This drives the str0m state machine:
    /// 1. Poll str0m for outputs (packets to send, events)
    /// 2. Read from the UDP socket
    /// 3. Feed received packets to str0m
    /// 4. Handle timeout-based polling
    pub async fn drive_connection(
        pc: &mut PeerConnection,
        socket: &UdpSocket,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut buf = vec![0u8; 2000];

        loop {
            let timeout = match pc.rtc.poll_output() {
                Ok(output) => match output {
                    Output::Transmit(transmit) => {
                        socket.send_to(&transmit.contents, transmit.destination)?;
                        continue;
                    }
                    Output::Timeout(t) => t,
                    Output::Event(event) => {
                        Self::handle_event(pc, event);
                        continue;
                    }
                },
                Err(e) => {
                    tracing::error!("str0m poll error: {e}");
                    break;
                }
            };

            let duration = timeout - Instant::now();
            socket.set_read_timeout(Some(duration))?;

            match socket.recv_from(&mut buf) {
                Ok((len, source)) => {
                    let input = Input::Receive(
                        Instant::now(),
                        str0m::net::Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: socket.local_addr()?,
                            contents: buf[..len].try_into()?,
                        },
                    );
                    pc.rtc.handle_input(input)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Timeout — let str0m handle internal timers
                    pc.rtc.handle_input(Input::Timeout(Instant::now()))?;
                }
                Err(e) => {
                    tracing::error!("Socket recv error: {e}");
                    break;
                }
            }
        }

        Ok(())
    }

    fn handle_event(pc: &mut PeerConnection, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) => {
                tracing::info!(pc_id = %pc.id, ?state, "ICE state changed");
            }
            Event::Connected => {
                tracing::info!(pc_id = %pc.id, "Peer connected");
            }
            Event::MediaAdded(media) => {
                tracing::info!(pc_id = %pc.id, ?media, "Media added");
            }
            _ => {
                tracing::debug!(pc_id = %pc.id, ?event, "WebRTC event");
            }
        }
    }
}
