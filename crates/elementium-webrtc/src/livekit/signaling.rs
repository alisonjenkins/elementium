//! `LiveKit` SFU WebSocket signaling client.
//!
//! Connects to the `LiveKit` SFU via WebSocket, sends/receives protobuf-encoded
//! `SignalRequest`/`SignalResponse` messages, and exposes an mpsc channel for
//! the room to consume incoming messages.

use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::Instrument;
use url::Url;

use livekit_protocol::signal_request;
use livekit_protocol::signal_response;
use livekit_protocol::{SignalRequest, SignalResponse};

#[derive(Error, Debug)]
pub enum SignalError {
    /// The configured SFU URL is not a URL at all (e.g. missing scheme, malformed
    /// percent-encoding). `url::ParseError` is the actual diagnosis -- "Invalid SFU URL"
    /// alone told a caller nothing beyond what they already knew.
    #[error("invalid SFU URL {url:?}")]
    InvalidUrl {
        url: String,
        #[source]
        source: url::ParseError,
    },
    /// `Url::set_scheme` refused to rewrite `http`/`https` to `ws`/`wss`.
    ///
    /// The `url` crate reports this as a bare `Err(())` -- it only happens for a
    /// "cannot-be-a-base" URL (no `//` authority, e.g. `https:opaque-thing` rather than
    /// `https://host/...`), and carries no lower-level cause to preserve. The rejected URL
    /// and the scheme change attempted are the whole diagnosis.
    ///
    /// X8 (`specs/BACKLOG-2026-08-09-errors.md`): verified experimentally, not just by
    /// inspection, that `build_ws_url` cannot actually reach this. `http`/`https`/`ws`/`wss`
    /// are all "special" schemes under the WHATWG URL spec the `url` crate implements, and a
    /// special-scheme URL cannot finish `Url::parse` without a host -- a probe of
    /// `http://`, `http:///path`, `http://example.com`, `https://x`, and others (host
    /// present, port present, userinfo present) showed every URL that parsed successfully
    /// also had a host and accepted `set_scheme("wss"/"ws")`, and every host-less input
    /// failed at `Url::parse` first with "empty host", before `set_scheme` is ever called.
    /// So this variant cannot be constructed by `build_ws_url` today.
    ///
    /// Kept rather than deleted anyway: `set_scheme` is a fallible library call returning
    /// `Result<(), ()>`, and the workspace denies `unwrap_used`/`expect_used`/`panic`, so
    /// the honest alternative to matching it is an `#[allow]`-scoped unwrap -- strictly
    /// worse than a named, documented, unreachable-today error variant that keeps the
    /// `Result` truthful if a future `url` release (or a future edit to `build_ws_url`
    /// that parses a wider set of inputs) ever makes it reachable again.
    #[error("SFU URL {url:?} has no authority to rewrite scheme {from:?} to {to:?}")]
    SchemeRewriteRejected {
        url: String,
        from: &'static str,
        to: &'static str,
    },
    /// The SFU URL's scheme was not one this client can dial.
    #[error("unsupported SFU URL scheme {scheme:?} (expected http, https, ws, or wss)")]
    UnsupportedScheme { scheme: String },
    /// The WebSocket handshake to the SFU (TCP connect, TLS, or HTTP Upgrade) failed.
    ///
    /// This is the failure that, reported as a bare string with no source and no tracing
    /// call, once read so much like something livekit-client itself would emit that it was
    /// blamed on the SFU's JS client for hours -- see Constitution Principle I. Keeping the
    /// real `tungstenite::Error` here is what lets a caller (or a human reading the log)
    /// tell a DNS failure from a TLS failure from a rejected upgrade.
    #[error("WebSocket handshake with the SFU failed")]
    // Boxed: `tungstenite::Error` is >130 bytes on its own, which would make every
    // `Result<_, SignalError>` that large even on the success path -- clippy's
    // `result_large_err` catches exactly this.
    Handshake(#[source] Box<tokio_tungstenite::tungstenite::Error>),
    /// The writer task has stopped, so the outgoing signal channel is closed.
    #[error("signal send channel closed")]
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
    ///
    /// # Errors
    ///
    /// Returns [`SignalError::ChannelClosed`] if the writer task has stopped
    /// and the outgoing channel is closed.
    pub fn send(&self, msg: signal_request::Message) -> Result<(), SignalError> {
        let req = SignalRequest { message: Some(msg) };
        self.tx.send(req).map_err(|_| SignalError::ChannelClosed)
    }
}

/// `LiveKit` SFU WebSocket signaling client.
pub struct SignalClient {
    sender: SignalSender,
    shutdown_tx: Option<mpsc::Sender<()>>,
}

impl SignalClient {
    /// Connect to the `LiveKit` SFU.
    ///
    /// Opens a WebSocket to `wss://<sfu>/rtc?access_token=<token>&auto_subscribe=true&sdk=rust&protocol=9`.
    /// Spawns a background task that reads from the WebSocket and forwards
    /// `SignalResponse` messages to the returned receiver.
    ///
    /// Hands the receiver back directly, rather than stashing it in an `Option` field a
    /// caller then has to `take()`. A one-shot value that is only ever consumed once,
    /// immediately, by the one caller that has the freshly-returned `Self` in hand does
    /// not need a runtime "already taken" check -- there is no second caller who could
    /// race for it, so the only way the check can fail is a bug in this function, and the
    /// type system can say that more plainly than a `None` arm can. See X7 in
    /// `specs/BACKLOG-2026-08-09-errors.md`: the previous `Option`-and-`take` shape
    /// modelled an invariant as a runtime `ChannelClosed` error that nothing could ever
    /// legitimately trigger.
    ///
    /// # Errors
    ///
    /// Returns [`SignalError::InvalidUrl`], [`SignalError::SchemeRewriteRejected`], or
    /// [`SignalError::UnsupportedScheme`] if `sfu_url` is invalid, and
    /// [`SignalError::Handshake`] if the WebSocket handshake fails.
    pub async fn connect(sfu_url: &str, token: &str) -> Result<(Self, SignalReceiver), SignalError> {
        let ws_url = build_ws_url(sfu_url, token)?;
        tracing::info!(url = %redact_token(&ws_url), "connect attempt started");

        let (ws_stream, _resp) = match tokio_tungstenite::connect_async(&ws_url).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(reason = %e, "signaling connect failed");
                return Err(SignalError::Handshake(Box::new(e)));
            }
        };

        tracing::info!("websocket connected to LiveKit SFU");

        let (write, read) = ws_stream.split();

        // Channel: room → WebSocket (outgoing requests)
        let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalRequest>();
        // Channel: WebSocket → room (incoming responses)
        let (in_tx, in_rx) = mpsc::unbounded_channel::<signal_response::Message>();
        // Shutdown signal
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        // Propagate the ambient session span into these detached tasks so their
        // events keep the same correlation_id.
        let span = tracing::Span::current();

        // Spawn writer task: reads from out_rx, encodes, sends to WebSocket
        tokio::spawn(ws_writer_loop(out_rx, write).instrument(span.clone()));

        // Spawn reader task: reads from WebSocket, decodes, sends to in_tx
        tokio::spawn(ws_reader_loop(read, in_tx, shutdown_rx).instrument(span));

        let sender = SignalSender { tx: out_tx };

        Ok((
            Self {
                sender,
                shutdown_tx: Some(shutdown_tx),
            },
            in_rx,
        ))
    }

    /// Get a cloneable sender for sending requests to the SFU.
    #[must_use]
    pub fn sender(&self) -> SignalSender {
        self.sender.clone()
    }

    /// Gracefully disconnect from the SFU.
    pub async fn disconnect(&mut self) {
        tracing::info!("Disconnecting from LiveKit SFU");
        // Send Leave request. This only fails if the writer task's channel is already
        // closed, which means `Leave` can never reach the SFU over this connection --
        // the server has no other way to learn we are gone and will keep this
        // participant's session alive until its own timeout fires, which is visible to
        // every other participant in the room and can affect a fast reconnect (X9:
        // previously discarded with `let _ =` and no log, so this never surfaced).
        if let Err(e) = self.sender.send(signal_request::Message::Leave(
            livekit_protocol::LeaveRequest {
                can_reconnect: false,
                // `DisconnectReason` is a fieldless protobuf enum (no `From<_> for i32`
                // impl generated by prost); the enum-to-i32 cast is infallible.
                #[allow(clippy::as_conversions)]
                reason: livekit_protocol::DisconnectReason::ClientInitiated as i32,
                ..Default::default()
            },
        )) {
            tracing::warn!(
                reason = %e,
                "could not queue Leave request; the SFU may still believe this call is active"
            );
        }
        // Signal shutdown to reader. A failure here just means the reader task's own
        // shutdown channel is already gone -- it has already ended by itself (the
        // server closed the socket, or the read loop errored out and returned), so
        // there is nothing left to tell it. That is the ordinary shape of a shutdown
        // race, not a condition with a consequence for the SFU or the next call:
        // judged uninteresting and left unlogged.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
    }
}

/// Build the WebSocket URL for `LiveKit` signaling.
///
/// # Errors
///
/// Returns [`SignalError::InvalidUrl`] if `sfu_url` fails to parse,
/// [`SignalError::SchemeRewriteRejected`] if its scheme cannot be rewritten to a WebSocket
/// scheme, or [`SignalError::UnsupportedScheme`] if it names neither `http(s)` nor `ws(s)`.
fn build_ws_url(sfu_url: &str, token: &str) -> Result<String, SignalError> {
    let mut url = Url::parse(sfu_url).map_err(|e| SignalError::InvalidUrl {
        url: sfu_url.to_owned(),
        source: e,
    })?;

    // Ensure we use wss:// scheme
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|()| SignalError::SchemeRewriteRejected {
                url: sfu_url.to_owned(),
                from: "https",
                to: "wss",
            })?,
        "http" => url
            .set_scheme("ws")
            .map_err(|()| SignalError::SchemeRewriteRejected {
                url: sfu_url.to_owned(),
                from: "http",
                to: "ws",
            })?,
        "wss" | "ws" => {}
        s => {
            return Err(SignalError::UnsupportedScheme {
                scheme: s.to_owned(),
            });
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

/// Replace the `access_token` query parameter's value with a placeholder.
///
/// The join URL carries a `LiveKit` JWT that grants the right to join the room, publish and
/// subscribe, for as long as it is valid -- an hour, by default. Logged whole, it is a
/// working credential sitting in a log file, a terminal scrollback, a CI artefact or a
/// bug report, and anyone holding it can join the call. Nothing else in the URL is
/// sensitive, and the rest is genuinely useful when diagnosing a connection, so the token
/// alone is replaced rather than the line being dropped.
///
/// Deliberately not "log the first few characters": a prefix of a JWT is its header, which
/// is the same for every token this codebase mints and identifies nothing.
fn redact_token(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        // Unparseable here means the URL was built and then mangled. Returning it as-is
        // would put the token back in the log, which is the one outcome to avoid.
        return "<unparseable url>".to_owned();
    };
    let pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| {
            if k == "access_token" {
                (k.into_owned(), "<redacted>".to_owned())
            } else {
                (k.into_owned(), v.into_owned())
            }
        })
        .collect();
    parsed.query_pairs_mut().clear().extend_pairs(pairs);
    parsed.to_string()
}

type WsWrite =
    futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;
type WsRead = futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// Background task: reads outgoing `SignalRequests`, encodes to protobuf, sends to WebSocket.
async fn ws_writer_loop(mut rx: mpsc::UnboundedReceiver<SignalRequest>, mut ws_write: WsWrite) {
    while let Some(req) = rx.recv().await {
        let mut buf = Vec::with_capacity(req.encoded_len());
        if let Err(e) = req.encode(&mut buf) {
            tracing::error!(reason = %e, "encoding SignalRequest failed");
            continue;
        }
        if let Err(e) = ws_write.send(WsMessage::Binary(buf)).await {
            tracing::error!(reason = %e, "websocket send failed");
            break;
        }
    }
    tracing::info!("signal writer loop ended");
    // Best-effort: send a clean WebSocket close frame now that nothing more will be
    // written. If the socket already failed (the `send` error above, which is already
    // logged) this is expected to fail too and is not worth a second alarm; logged at
    // debug rather than left silent so a *surprise* failure -- the socket looked fine,
    // `Leave` above went out, and closing it still errored -- is still visible to
    // someone deliberately looking, without adding warning-level noise to every
    // ordinary disconnect.
    if let Err(e) = ws_write.close().await {
        tracing::debug!(reason = %e, "closing the SFU websocket after the writer loop ended failed");
    }
}

/// Background task: reads from WebSocket, decodes `SignalResponse`, forwards to channel.
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
                                if let Some(message) = resp.message
                                    && tx.send(message).is_err()
                                {
                                    tracing::info!("Signal receiver dropped, stopping reader");
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(reason = %e, "decoding SignalResponse failed");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        tracing::info!("websocket closed by server");
                        break;
                    }
                    Some(Ok(_)) => {
                        // Ping/Pong/Text frames — ignore
                    }
                    Some(Err(e)) => {
                        tracing::error!(reason = %e, "websocket read failed");
                        break;
                    }
                    None => {
                        tracing::info!("websocket stream ended");
                        break;
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("signal reader received shutdown");
                break;
            }
        }
    }
    tracing::info!("signal reader loop ended");
}

#[cfg(test)]
// Matches the precedent in `room.rs`'s test module: `expect`/`panic` on a clearly-labelled
// test assertion is the idiomatic way to fail a test with a message, and the workspace lint
// otherwise exists to keep them out of the paths users actually run.
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{SignalClient, SignalError, SignalRequest, build_ws_url, redact_token};
    use livekit_protocol::signal_request;
    use tokio::sync::mpsc;

    /// A token that appears in the URL must not appear in what gets logged.
    ///
    /// Asserted against the real `build_ws_url` output rather than a hand-written URL, so
    /// that adding another token-bearing parameter to the join URL breaks this test rather
    /// than quietly widening what reaches the logs.
    #[test]
    fn the_join_token_never_reaches_the_log_line() {
        let token = "eyJhbGciOiJIUzI1NiJ9.aVeryRealLookingClaimsSection.andASignature";
        let url = build_ws_url("http://127.0.0.1:7880", token).unwrap_or_default();
        assert!(url.contains(token), "the URL under test must carry the token");

        let logged = redact_token(&url);
        assert!(
            !logged.contains(token),
            "the join token must not survive into a log line: {logged}"
        );
        assert!(logged.contains("access_token=%3Credacted%3E") || logged.contains("<redacted>"));
        // The rest of the URL is what makes the line worth logging at all.
        assert!(logged.contains("127.0.0.1:7880"));
        assert!(logged.contains("protocol="));
    }

    /// A URL that cannot be parsed must not be logged verbatim as a fallback.
    #[test]
    fn an_unparseable_url_is_not_echoed_back() {
        assert_eq!(redact_token("not a url at all"), "<unparseable url>");
    }

    /// An `sfu_url` that isn't a URL at all must surface as `InvalidUrl` with the real
    /// `url::ParseError` attached, not a paraphrase of it.
    ///
    /// Before this variant existed, every parse failure collapsed into
    /// `Connection(format!("Invalid SFU URL: {e}"))` -- a caller could not match on it, and
    /// the underlying `ParseError` was only reachable by re-parsing the `Display` string.
    #[test]
    fn an_unparseable_sfu_url_is_reported_as_invalid_url_with_its_source() {
        let err = build_ws_url("not a url at all", "tok").expect_err("must fail to parse");
        match err {
            SignalError::InvalidUrl { url, source } => {
                assert_eq!(url, "not a url at all");
                // The point of the variant: the real parse error is still there to walk.
                let _: &dyn std::error::Error = &source;
            }
            other => panic!("expected InvalidUrl, got {other:?}"),
        }
    }

    /// A scheme this client cannot dial (anything but http/https/ws/wss) must surface as
    /// `UnsupportedScheme` naming the scheme it saw, not a formatted sentence a caller would
    /// have to parse back apart to find out what was wrong.
    #[test]
    fn an_unsupported_scheme_is_reported_by_name() {
        let err = build_ws_url("ftp://example.com", "tok").expect_err("ftp is not dialable");
        match err {
            SignalError::UnsupportedScheme { scheme } => assert_eq!(scheme, "ftp"),
            other => panic!("expected UnsupportedScheme, got {other:?}"),
        }
    }

    /// The signal channel closing while a caller still expects it open must surface as
    /// `ChannelClosed`, not an unwrap panic or a swallowed `Err(())`.
    #[test]
    fn send_after_the_writer_task_is_gone_reports_channel_closed() {
        let (tx, rx) = mpsc::unbounded_channel::<SignalRequest>();
        drop(rx); // stands in for the writer task having stopped
        let sender = super::SignalSender { tx };

        let err = sender
            .send(signal_request::Message::Leave(
                livekit_protocol::LeaveRequest::default(),
            ))
            .expect_err("no receiver is listening");
        assert!(matches!(err, SignalError::ChannelClosed));
    }

    /// X9 (`specs/BACKLOG-2026-08-09-errors.md`): a `disconnect()` whose `Leave` request
    /// cannot even be queued must say so, at `WARN`, not swallow it with `let _ =`.
    ///
    /// Before this test existed, that path *did* use `let _ =` with no log call at all:
    /// the assertion below was written first and run against that code, where it failed
    /// because `find_event` found nothing -- there was nothing to find. This pins the
    /// regression: a `Leave` that never reaches the SFU because the writer task is
    /// already gone, with nothing anywhere saying it happened, is exactly the shape of
    /// silence Constitution Principle I was ratified over, applied to teardown instead
    /// of setup.
    #[tokio::test]
    async fn a_leave_that_cannot_be_queued_is_logged_not_swallowed() {
        use elementium_observability_test::LogCapture;

        // Global, not `LogCapture::run`'s thread-local scoping: `disconnect` is async and
        // this crate's other tests never install a global capture, so there is nothing
        // for this one to collide with.
        let capture = LogCapture::new();
        capture.install_global();

        let (tx, rx) = mpsc::unbounded_channel::<SignalRequest>();
        drop(rx); // stands in for the writer task having already stopped
        let mut client = SignalClient {
            sender: super::SignalSender { tx },
            shutdown_tx: None,
        };

        client.disconnect().await;

        let event = capture
            .find_event("could not queue Leave request")
            .expect("a discarded Leave send must be logged, not silently dropped");
        assert_eq!(event.level, tracing::Level::WARN);
    }

    /// A WebSocket handshake that fails (here: nothing listening on the far end) must
    /// surface as `Handshake` carrying the real `tungstenite::Error`, not a bare string
    /// indistinguishable from a message livekit-client itself might emit -- the exact shape
    /// of the fault Constitution Principle I was ratified over.
    #[tokio::test]
    async fn a_refused_handshake_is_reported_as_handshake_with_its_source() {
        // Port 0 is never a real listener; connecting to it fails immediately at the
        // transport level, well before any WebSocket upgrade is attempted.
        // `SignalClient` deliberately has no `Debug` impl (it owns live channels and a
        // task handle, not inspectable state), so `expect_err`/`unwrap_err` cannot be
        // used here; match out the `Err` directly instead.
        let Err(err) = SignalClient::connect("http://127.0.0.1:0", "tok").await else {
            panic!("nothing is listening on port 0");
        };
        match err {
            SignalError::Handshake(source) => {
                let _: &dyn std::error::Error = &source;
            }
            other => panic!("expected Handshake, got {other:?}"),
        }
    }
}
