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

use crate::peer_connection::PcEvent;

/// Encrypt an outbound frame if E2EE is active, or drop it with a warning if encryption is
/// configured but fails -- fail-closed, never sends plaintext when E2EE is supposed to be
/// protecting the frame.
pub(crate) fn encrypt_or_drop(
    e2ee: Option<&E2eeContext>,
    data: Vec<u8>,
    kind: E2eeMediaKind,
    label: &str,
) -> Option<Vec<u8>> {
    let Some(ctx) = e2ee else {
        return Some(data);
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
pub(crate) fn maybe_decrypt_event(event: PcEvent, e2ee: Option<&E2eeContext>) -> Option<PcEvent> {
    let Some(ctx) = e2ee else {
        return Some(event);
    };

    match event {
        PcEvent::AudioData(data) => match ctx.decrypt_frame_any(&data, E2eeMediaKind::Audio) {
            Ok(Some(decrypted)) => Some(PcEvent::AudioData(decrypted)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(reason = %e, "E2EE dropping inbound audio frame: decrypt failed");
                None
            }
        },
        PcEvent::VideoData(data) => match ctx.decrypt_frame_any(&data, E2eeMediaKind::Video) {
            Ok(Some(decrypted)) => Some(PcEvent::VideoData(decrypted)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(reason = %e, "E2EE dropping inbound video frame: decrypt failed");
                None
            }
        },
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use elementium_e2ee::E2eeOptions;
    use elementium_observability_test::LogCapture;

    use super::*;

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
            encrypt_or_drop(Some(&ctx), b"plaintext-frame".to_vec(), E2eeMediaKind::Audio, "audio")
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
            encrypt_or_drop(None, b"plaintext-frame".to_vec(), E2eeMediaKind::Audio, "audio")
        });
        assert_eq!(result, Some(b"plaintext-frame".to_vec()));
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
            maybe_decrypt_event(PcEvent::AudioData(b"ciphertext-looking-bytes".to_vec()), Some(&ctx))
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
            .encrypt_frame(plaintext, elementium_e2ee::MediaKind::Audio)
            .expect("encrypt should succeed with a key set");

        let receiver = E2eeContext::new(E2eeOptions::default());
        receiver.set_key("alice", 0, b"test-key-material-1234567890abc");

        let result = maybe_decrypt_event(PcEvent::AudioData(encrypted), Some(&receiver));
        let PcEvent::AudioData(decrypted) = result.expect("decrypt should succeed") else {
            panic!("expected AudioData variant");
        };
        assert_eq!(decrypted, plaintext);
    }

    /// Sanity check: when no E2EE context is configured, events pass through unmodified.
    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn maybe_decrypt_event_passes_through_when_no_e2ee_configured() {
        let event = PcEvent::AudioData(b"raw-unencrypted-opus".to_vec());
        let result = maybe_decrypt_event(event, None);
        let PcEvent::AudioData(data) = result.expect("no-op passthrough should return Some") else {
            panic!("expected AudioData variant");
        };
        assert_eq!(data, b"raw-unencrypted-opus");
    }
}
