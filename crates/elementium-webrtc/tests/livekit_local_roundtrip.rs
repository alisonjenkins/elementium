//! Full two-client round-trip test against a real, locally-run `LiveKit` server.
//!
//! This exercises the actual publish -> SFU relay -> subscribe -> decrypt -> decode path
//! (the code implicated in a real "digital screeching" audio bug this test was written to
//! catch), rather than a synthetic in-process signal round trip -- it's driven through a
//! genuine `LiveKitRoom` connection, real SDP/ICE/DTLS-SRTP negotiation, and a real SFU
//! relay, exactly like the production app.
//!
//! # Running
//!
//! Requires a local `livekit-server` in dev mode:
//!
//! ```bash
//! docker run -d --name elementium-test-livekit --network host \
//!     livekit/livekit-server --dev --bind 0.0.0.0
//! ```
//!
//! Then:
//!
//! ```bash
//! cargo test -p elementium-webrtc --test livekit_local_roundtrip -- --ignored --nocapture
//! ```
//!
//! Ignored by default (like any test needing external infrastructure) so `cargo test
//! --workspace` and CI stay hermetic; this is a manual/local diagnostic tool, not a
//! regression gate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use elementium_observability_test::LogCapture;
use elementium_types::CorrelationId;
use elementium_webrtc::livekit::room::LiveKitRoom;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

const DEV_API_KEY: &str = "devkey";
const DEV_API_SECRET: &str = "secret";
const SFU_URL: &str = "http://127.0.0.1:7880";

// The 4 bools mirror LiveKit's own JWT video-grant schema field-for-field (an external API
// contract, not a design choice made here); splitting into an enum would just add an
// indirection layer with no clearer meaning.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
struct VideoGrant {
    room: String,
    room_join: bool,
    can_publish: bool,
    can_subscribe: bool,
    can_publish_data: bool,
}

#[derive(Serialize)]
struct Claims {
    iss: String,
    sub: String,
    iat: u64,
    exp: u64,
    nbf: u64,
    jti: String,
    name: String,
    video: VideoGrant,
}

/// Mint a `LiveKit` access token (HS256 JWT) against the local dev server's placeholder
/// `devkey`/`secret` credentials -- matches what `docker run ... livekit-server --dev` logs
/// on startup ("no keys provided, using placeholder keys").
#[allow(clippy::expect_used)]
fn mint_token(identity: &str, room: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the epoch")
        .as_secs();

    let claims = Claims {
        iss: DEV_API_KEY.to_string(),
        sub: identity.to_string(),
        iat: now,
        exp: now.checked_add(3600).unwrap_or(now),
        nbf: now,
        jti: identity.to_string(),
        name: identity.to_string(),
        video: VideoGrant {
            room: room.to_string(),
            room_join: true,
            can_publish: true,
            can_subscribe: true,
            can_publish_data: true,
        },
    };

    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(DEV_API_SECRET.as_bytes()),
    )
    .expect("JWT encoding with a valid HS256 secret cannot fail")
}

/// Generate `duration_ms` of a 440Hz sine wave as 20ms interleaved-stereo f32 PCM chunks at
/// 48kHz -- Opus's native rate, matching what the real encoder always uses.
fn sine_wave_chunks(duration_ms: u32, freq_hz: f32) -> Vec<Vec<f32>> {
    const SAMPLE_RATE: u32 = 48_000;
    const CHUNK_MS: u32 = 20;
    // 48_000 * 20 / 1000, spelled as a literal rather than a const-eval'd `as usize` cast.
    const SAMPLES_PER_CHUNK: usize = 960;
    // 48_000 is a fixed compile-time constant well within f32's exact-integer range
    // (up to 2^24), so this conversion is lossless despite the general-purpose lint.
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    const SAMPLE_RATE_F32: f32 = SAMPLE_RATE as f32;

    let num_chunks = duration_ms / CHUNK_MS;
    let mut chunks = Vec::with_capacity(usize::try_from(num_chunks).unwrap_or(0));
    let mut sample_index: u32 = 0;

    for _ in 0..num_chunks {
        let mut chunk = Vec::with_capacity(SAMPLES_PER_CHUNK * 2);
        for _ in 0..SAMPLES_PER_CHUNK {
            let t = f32::from(u16::try_from(sample_index % SAMPLE_RATE).unwrap_or(0))
                / SAMPLE_RATE_F32;
            let sample = (t * freq_hz * std::f32::consts::TAU).sin() * 0.5;
            chunk.push(sample); // left
            chunk.push(sample); // right
            sample_index = sample_index.wrapping_add(1);
        }
        chunks.push(chunk);
    }
    chunks
}

// Multi-threaded runtime, not the default single-threaded one: alice's signal-processing
// loop, bob's signal-processing loop, and this test function's own audio-sending loop all
// need to run with genuine parallelism. On a single cooperative thread, a long-running task
// (or one slow to yield) can starve the others -- e.g. the SFU renegotiating a subscriber's
// SDP mid-call needs that side's signal loop to be scheduled promptly to answer in time, or
// the SFU gives up ("negotiation timed out") and tears down the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a local livekit-server (see module docs for the docker run command)"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn audio_survives_real_publish_subscribe_round_trip() {
    let capture = LogCapture::new();
    capture.install_global();

    let room_name = format!(
        "elementium-test-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis()
    );

    let video_frames_alice: elementium_webrtc::engine::VideoFrameBuffer =
        Arc::new(Mutex::new(HashMap::new()));
    let video_frames_bob: elementium_webrtc::engine::VideoFrameBuffer =
        Arc::new(Mutex::new(HashMap::new()));

    let alice_token = mint_token("alice", &room_name);
    let bob_token = mint_token("bob", &room_name);

    let (mut alice, _alice_events) = LiveKitRoom::connect(
        SFU_URL,
        &alice_token,
        video_frames_alice,
        CorrelationId::new(),
        elementium_webrtc::EncryptionPolicy::ExplicitlyUnencrypted,
    )
    .await
    .expect("alice should connect to the local dev server");

    let (_bob, _bob_events) = LiveKitRoom::connect(
        SFU_URL,
        &bob_token,
        video_frames_bob,
        CorrelationId::new(),
        elementium_webrtc::EncryptionPolicy::ExplicitlyUnencrypted,
    )
    .await
    .expect("bob should connect to the local dev server");

    // Give both sides a moment to finish ICE/DTLS after the initial join before publishing.
    // NOTE: generous fixed delay, not event-driven -- SignalSender::send() is fire-and-forget
    // (enqueues to an unbounded channel, no delivery confirmation), so publishing too early
    // relative to the signaling connection's readiness could silently lose the AddTrack
    // message. If this test is still flaky at this delay, that's itself worth fixing for
    // real (either exposing a real "signaling ready" signal, or making publish_track's
    // caller-visible contract stronger), not just a test tuning problem.
    tokio::time::sleep(Duration::from_secs(3)).await;

    alice
        .publish_track("audio", "microphone")
        .expect("publish_track should succeed");

    // Give the SFU time to process the AddTrack + renegotiate before we start sending media.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut encoder =
        elementium_codec::OpusEncoder::new(48_000, 2).expect("Opus encoder should initialize");

    // 30 seconds, not a shorter window: on a machine with several network interfaces (LAN,
    // Docker bridges, a VPN, IPv6), the subscriber's ICE connectivity checks against every
    // candidate pair can genuinely take 10+ seconds to settle before any RTP can flow at
    // all -- confirmed empirically (a 6s window reliably produced zero decoded frames on such
    // a machine even though publish/SFU/subscribe signaling all completed correctly; 30s
    // reliably passes). This is a real property of ICE negotiation, not a arbitrary retry.
    for chunk in sine_wave_chunks(30_000, 440.0) {
        let frame = elementium_types::AudioFrame {
            sample_rate: 48_000,
            channels: 2,
            data: chunk,
            timestamp_us: 0,
        };
        if let Ok(packet) = encoder.encode(&frame) {
            let _ = alice.write_audio(packet).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Let the last few packets finish decoding on bob's side.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let events = capture.events();
    let decode_failures: Vec<_> = events
        .iter()
        .filter(|e| e.message().is_some_and(|m| m.contains("Failed to decode")))
        .collect();
    assert!(
        decode_failures.is_empty(),
        "expected zero Opus decode failures on the receiving side, got: {decode_failures:?}"
    );

    let decode_successes: Vec<_> = events
        .iter()
        .filter(|e| {
            e.message()
                .is_some_and(|m| m.contains("Decoded inbound Opus audio frame"))
        })
        .collect();
    assert!(
        !decode_successes.is_empty(),
        "expected at least one successfully decoded inbound Opus audio frame on bob's side \
         (100 packets were sent; decode events log every 100th one) -- got none, meaning audio \
         never made it through the real publish/SFU/subscribe/decode path"
    );

    for event in &decode_successes {
        assert_eq!(event.field("decoded_sample_rate"), Some("48000"));
        assert_eq!(event.field("decoded_channels"), Some("2"));
    }
}
