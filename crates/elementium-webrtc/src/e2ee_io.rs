//! Shared E2EE encrypt/decrypt-or-drop logic for inbound/outbound `PcEvent`s.
//!
//! Both the native peer-connection I/O loop (`engine.rs`) and the `LiveKit` SFU transport
//! (`livekit/transport.rs`) need identical fail-closed framing around
//! [`E2eeContext::encrypt_frame`]/[`E2eeContext::decrypt_frame_any`]. This used to be
//! duplicated in both files; when the inbound fail-open bug (playing back still-encrypted
//! bytes on decrypt failure instead of dropping the frame) was fixed in one copy, the other
//! silently kept the bug. Living in one place means there's only one implementation to get
//! right and keep right.

use elementium_e2ee::{E2eeContext, MediaKind as E2eeMediaKind};

use elementium_types::{PlaintextMedia, WireMedia};

use crate::peer_connection::{PcEvent, WirePcEvent};

/// Whether a connection encrypts media with E2EE, or deliberately does not.
///
/// Replaces the previous `Option<E2eeContext>` used at connection-setup boundaries
/// (`Transport::new_with_e2ee`, `LiveKitRoom::connect`, `WebRtcEngine::e2ee`). `None` let
/// "unencrypted" happen by omission -- a caller that simply forgot to wire up E2EE (as
/// `commands/livekit.rs` did until this was found) type-checked identically to one that
/// deliberately chose no encryption. Naming the unencrypted state forces every call site to
/// write `ExplicitlyUnencrypted` on purpose; a forgotten wire-up is a compile error instead
/// of a silent security gap.
#[derive(Clone, Default)]
pub enum EncryptionPolicy {
    /// No E2EE: media is sent/received as-is (still protected by DTLS-SRTP transport
    /// encryption, but not by the app-layer E2EE key).
    #[default]
    ExplicitlyUnencrypted,
    /// E2EE active with the given context.
    Encrypted(E2eeContext),
}

impl EncryptionPolicy {
    /// Borrow the active `E2eeContext`, or `None` if explicitly unencrypted.
    #[must_use]
    pub const fn as_context(&self) -> Option<&E2eeContext> {
        match self {
            Self::Encrypted(ctx) => Some(ctx),
            Self::ExplicitlyUnencrypted => None,
        }
    }
}

/// Encrypt an outbound frame if E2EE is active, or drop it with a warning if encryption is
/// configured but fails -- fail-closed, never sends plaintext when E2EE is supposed to be
/// protecting the frame.
pub(crate) fn encrypt_or_drop(
    e2ee: Option<&E2eeContext>,
    data: PlaintextMedia,
    kind: E2eeMediaKind,
    label: &str,
) -> Option<WireMedia> {
    let Some(ctx) = e2ee else {
        // Deliberate unencrypted send: named at the call into `WireMedia` so a forgotten
        // key can never silently become "shipped plaintext" -- it has to be written down.
        return Some(WireMedia::deliberately_unencrypted(data));
    };
    ctx.encrypt_frame(&data, kind).map_or_else(
        || {
            tracing::warn!(
                reason = "e2ee_encrypt_failed",
                label,
                "Dropping outbound frame: E2EE encryption failed"
            );
            None
        },
        Some,
    )
}

/// Attempt to decrypt an inbound audio/video event if E2EE is active.
///
/// Uses `decrypt_frame_any` which tries all known participant keys, since we don't know
/// which participant sent a particular RTP frame via the SFU.
///
/// Returns `None` to drop the event (E2EE active but decryption failed) rather than passing
/// through still-encrypted/undecryptable bytes as if they were valid media -- feeding
/// ciphertext straight to the Opus/VP8 decoder produces garbage output (audible as noise),
/// which is worse than silently dropping the frame.
pub(crate) fn maybe_decrypt_event(
    event: WirePcEvent,
    e2ee: Option<&E2eeContext>,
) -> Option<PcEvent> {
    // Non-media variants carry no payload, so they cross the boundary unchanged. Written
    // out explicitly rather than via a catch-all: the payload type differs on each side,
    // so the compiler forces every variant to be considered whenever one is added.
    let passthrough = |event: WirePcEvent| -> Option<PcEvent> {
        match event {
            PcEvent::IceConnectionStateChange(s) => Some(PcEvent::IceConnectionStateChange(s)),
            PcEvent::ConnectionStateChange(s) => Some(PcEvent::ConnectionStateChange(s)),
            PcEvent::IceCandidate(c) => Some(PcEvent::IceCandidate(c)),
            PcEvent::IceGatheringComplete => Some(PcEvent::IceGatheringComplete),
            PcEvent::KeyframeRequested { mid } => Some(PcEvent::KeyframeRequested { mid }),
            PcEvent::Connected => Some(PcEvent::Connected),
            PcEvent::RemoteTrackAdded { mid, kind } => Some(PcEvent::RemoteTrackAdded { mid, kind }),
            PcEvent::EgressStats { mid, loss, rtt_ms, packets, nacks } => {
                Some(PcEvent::EgressStats { mid, loss, rtt_ms, packets, nacks })
            }
            PcEvent::AudioData { .. } | PcEvent::VideoData { .. } => None,
        }
    };

    let Some(ctx) = e2ee else {
        // No E2EE context: media is passed through byte-for-byte. That is correct only if
        // the *remote* is also sending unencrypted. If the remote is encrypting (as
        // Element Call / MatrixRTC does by default), this hands raw ciphertext to the
        // Opus/VP8 decoder -- and libopus turns that into noise rather than an error, so
        // it surfaces as unexplained "digital screeching" with a clean-looking log.
        // Warned once per process (not per frame) so the condition is impossible to miss
        // without drowning the log at 50 frames/sec.
        return match event {
            PcEvent::AudioData { mid, data, contiguous } => {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    tracing::warn!(
                        "Inbound audio is being passed through WITHOUT decryption (no E2EE key \
                         configured). If the remote peer is encrypting, the decoder is being fed \
                         ciphertext and will output noise."
                    );
                });
                Some(PcEvent::AudioData {
                    mid,
                    data: PlaintextMedia::assume_peer_sends_unencrypted(data),
                    contiguous,
                })
            }
            PcEvent::VideoData { mid, data } => Some(PcEvent::VideoData {
                mid,
                data: PlaintextMedia::assume_peer_sends_unencrypted(data),
            }),
            other => passthrough(other),
        };
    };

    match event {
        PcEvent::AudioData { mid, data, contiguous } => {
            match ctx.decrypt_frame_any(&data, E2eeMediaKind::Audio) {
                Ok(Some(decrypted)) => {
                    Some(PcEvent::AudioData { mid, data: decrypted, contiguous })
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(
                        %mid,
                        reason = %e,
                        "E2EE dropping inbound audio frame: decrypt failed"
                    );
                    None
                }
            }
        }
        PcEvent::VideoData { mid, data } => {
            match ctx.decrypt_frame_any(&data, E2eeMediaKind::Video) {
                Ok(Some(decrypted)) => Some(PcEvent::VideoData { mid, data: decrypted }),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(
                        %mid,
                        reason = %e,
                        "E2EE dropping inbound video frame: decrypt failed"
                    );
                    None
                }
            }
        }
        other => passthrough(other),
    }
}

#[cfg(test)]
mod tests {
    use elementium_e2ee::E2eeOptions;
    use elementium_observability_test::LogCapture;

    use super::*;

    /// The default `EncryptionPolicy` must be the explicit unencrypted variant, not a bare
    /// absence -- this is the whole point of the type: a connection wired up without
    /// deliberately choosing a policy gets a named, greppable "unencrypted" state instead
    /// of silently defaulting through an `Option::None`.
    #[test]
    fn default_policy_is_explicitly_unencrypted() {
        assert!(matches!(
            EncryptionPolicy::default(),
            EncryptionPolicy::ExplicitlyUnencrypted
        ));
        assert!(EncryptionPolicy::default().as_context().is_none());
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn encrypted_policy_exposes_its_context() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        let policy = EncryptionPolicy::Encrypted(ctx);
        assert!(policy.as_context().is_some());
    }

    /// Regression test for the outbound fail-open E2EE bug: when a frame can't be encrypted
    /// (e.g. no key set for the local participant), `encrypt_or_drop` must drop it rather
    /// than let it through as plaintext, and must emit a structured "frame dropped" warning
    /// with a `reason` field so the drop is visible in logs, not just inferred from an
    /// absent return value.
    #[test]
    #[allow(clippy::expect_used)]
    fn encrypt_or_drop_emits_structured_warning_when_no_key_set() {
        // No key set for any participant -> encrypt_frame returns None.
        let ctx = E2eeContext::new(E2eeOptions::default());
        let capture = LogCapture::new();

        let result = capture.run(|| {
            encrypt_or_drop(Some(&ctx), PlaintextMedia::from_encoder(b"plaintext-frame".to_vec()), E2eeMediaKind::Audio, "audio")
        });

        // Fail closed: the frame must be dropped, never sent as plaintext.
        assert!(result.is_none());

        let event = capture
            .find_event("Dropping outbound frame")
            .expect("a structured 'frame dropped' warning should have been emitted");
        assert_eq!(event.level, tracing::Level::WARN);
        assert!(event.field("reason").is_some());
        assert_eq!(event.field("label"), Some("audio"));
    }

    /// Sanity check: when no E2EE context is configured at all, `encrypt_or_drop` passes the
    /// frame through unmodified and emits no drop warning.
    #[test]
    fn encrypt_or_drop_passes_through_when_no_e2ee_configured() {
        let capture = LogCapture::new();
        let result = capture.run(|| {
            encrypt_or_drop(None, PlaintextMedia::from_encoder(b"plaintext-frame".to_vec()), E2eeMediaKind::Audio, "audio")
        });
        assert_eq!(result.map(WireMedia::into_bytes), Some(b"plaintext-frame".to_vec()));
        assert!(capture.find_event("Dropping outbound frame").is_none());
    }

    /// Regression test for the INBOUND fail-open E2EE bug (the actual root cause of a
    /// real-world "digital screeching" audio report): when an inbound frame can't be
    /// decrypted (e.g. no participant keys known at all), `maybe_decrypt_event` must drop
    /// the event rather than passing the still-encrypted bytes through as if they were valid
    /// media -- feeding ciphertext to an Opus/VP8 decoder produces audible garbage, not a
    /// clean error. This exact bug existed independently in two separate, duplicated
    /// implementations (`engine.rs` and `livekit/transport.rs`) before both were unified to
    /// call this shared function.
    #[test]
    #[allow(clippy::expect_used)]
    fn maybe_decrypt_event_drops_audio_when_no_keys_known() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        let capture = LogCapture::new();

        let result = capture.run(|| {
            maybe_decrypt_event(
                PcEvent::AudioData { mid: "1".to_string(), data: WireMedia::from_network(b"ciphertext-looking-bytes".to_vec()), contiguous: true },
                Some(&ctx),
            )
        });

        // Fail closed: the event must be dropped, never forwarded as if it were decrypted.
        assert!(result.is_none());
    }

    /// Positive path: with a correctly configured key, `maybe_decrypt_event` returns the
    /// real decrypted plaintext, not the still-encrypted input.
    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn maybe_decrypt_event_decrypts_with_known_key() {
        let sender = E2eeContext::new(E2eeOptions::default());
        sender.set_local_identity("alice");
        sender.set_key("alice", 0, b"test-key-material-1234567890abc");
        let plaintext = b"hello-from-alice";
        let encrypted = sender
            .encrypt_frame(&PlaintextMedia::from_encoder(plaintext.to_vec()), elementium_e2ee::MediaKind::Audio)
            .expect("encrypt should succeed with a key set");

        let receiver = E2eeContext::new(E2eeOptions::default());
        receiver.set_key("alice", 0, b"test-key-material-1234567890abc");

        let result = maybe_decrypt_event(
            PcEvent::AudioData { mid: "1".to_string(), data: encrypted, contiguous: true },
            Some(&receiver),
        );
        let PcEvent::AudioData { data: decrypted, .. } = result.expect("decrypt should succeed")
        else {
            panic!("expected AudioData variant");
        };
        assert_eq!(decrypted.as_bytes(), plaintext);
    }

    /// Sanity check: when no E2EE context is configured, events pass through unmodified.
    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn maybe_decrypt_event_passes_through_when_no_e2ee_configured() {
        let event = PcEvent::AudioData { mid: "1".to_string(), data: WireMedia::from_network(b"raw-unencrypted-opus".to_vec()), contiguous: true };
        let result = maybe_decrypt_event(event, None);
        let PcEvent::AudioData { data, .. } = result.expect("no-op passthrough should return Some")
        else {
            panic!("expected AudioData variant");
        };
        assert_eq!(data.as_bytes(), b"raw-unencrypted-opus");
    }
}
