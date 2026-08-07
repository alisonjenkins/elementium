//! Systematic per-layer bisection of the outbound audio path.
//!
//! # Why this exists
//!
//! A long-running "the far end hears me as a robot" bug survived five rounds of targeted
//! fixes because every measurement taken inside the app came back clean: the microphone was
//! healthy, the encoder round-tripped, RTCP reported 0% loss, packet rate was exactly
//! 50/sec, and nothing was ever dropped. The existing round-trip test
//! (`livekit_local_roundtrip.rs`) agreed, because its assertion is:
//!
//! ```text
//! assert!(!decode_successes.is_empty(), "expected at least one ...")
//! ```
//!
//! "At least one frame" passes when 95% of the audio is missing. That is the exact
//! symptom being chased, so the test could never have caught it. It also runs
//! [`EncryptionPolicy::ExplicitlyUnencrypted`], so the E2EE layer -- where a real bug was
//! eventually found -- was never exercised end-to-end at all.
//!
//! These tests are built on two rules learned from that:
//!
//! 1. **Assert on proportions, never on existence.** Every end-to-end test here measures a
//!    delivery ratio against the number of frames actually sent.
//! 2. **Each layer is isolated, and each adds exactly one thing** to the layer below it, so
//!    a failure localises itself instead of requiring a bisection by hand.
//!
//! # Layers
//!
//! | Layer | Adds | Needs an SFU |
//! |-------|------|--------------|
//! | 1 | Opus encode/decode | no |
//! | 2 | E2EE encrypt/decrypt around the codec | no |
//! | 3 | Key rotation past the key-ring size | no |
//! | 4 | A real SFU relay, unencrypted | yes |
//! | 5 | A real SFU relay, encrypted | yes |
//!
//! Layers 1-3 are hermetic and run in `cargo test --workspace`. Layers 4-5 need a local
//! `livekit-server` and are `#[ignore]`d, like any test with external infrastructure:
//!
//! ```bash
//! docker run -d --name elementium-test-livekit --network host \
//!     livekit/livekit-server --dev --bind 0.0.0.0
//! cargo test -p elementium-webrtc --test audio_layer_bisection -- --ignored --nocapture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use elementium_codec::{OpusDecoder, OpusEncoder, OpusEncoderConfig};
use elementium_e2ee::{E2eeContext, E2eeOptions, MediaKind};
use elementium_observability_test::LogCapture;
use elementium_types::{AudioFrame, CorrelationId, WireMedia};
use elementium_webrtc::EncryptionPolicy;
use elementium_webrtc::livekit::room::LiveKitRoom;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Reference signal and fidelity metrics
// ---------------------------------------------------------------------------

/// Samples in one 20ms frame at 48kHz, per channel. Opus's native framing.
const FRAME_SAMPLES: usize = 960;
const SAMPLE_RATE: u32 = 48_000;

/// Outbound audio is mono; see `OUTBOUND_CHANNELS` in the capture pipeline.
const CHANNELS: u16 = 1;

/// A deterministic mono reference signal: a 440Hz sine at half scale, cut into 20ms frames.
///
/// Deterministic on purpose -- a fidelity assertion has to compare against something the
/// test itself knows exactly, and a fixed tone makes a correlation figure interpretable
/// rather than merely reproducible.
fn reference_frames(frame_count: usize) -> Vec<Vec<f32>> {
    // 48_000 is exactly representable in f32 (well under 2^24).
    let rate = 48_000.0_f32;
    let mut frames = Vec::with_capacity(frame_count);
    let mut n: u32 = 0;
    for _ in 0..frame_count {
        let mut frame = Vec::with_capacity(FRAME_SAMPLES);
        for _ in 0..FRAME_SAMPLES {
            #[allow(clippy::cast_precision_loss)]
            let t = f32::from(u16::try_from(n % SAMPLE_RATE).unwrap_or(0)) / rate;
            frame.push((t * 440.0 * std::f32::consts::TAU).sin() * 0.5);
            n = n.wrapping_add(1);
        }
        frames.push(frame);
    }
    frames
}

/// Pearson correlation of two equal-length signals.
///
/// Opus is perceptual, not waveform-preserving, so this is a coarse "is it still the same
/// sound" check rather than a quality metric. It is here to catch gross damage --
/// scrambled ordering, wrong channel interpretation, half-length frames -- which all drive
/// correlation to near zero while leaving amplitude statistics looking perfectly healthy.
fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    let len = n as f32;
    let mean_a = a[..n].iter().sum::<f32>() / len;
    let mean_b = b[..n].iter().sum::<f32>() / len;
    let mut num = 0.0_f32;
    let mut den_a = 0.0_f32;
    let mut den_b = 0.0_f32;
    for i in 0..n {
        let da = a[i] - mean_a;
        let db = b[i] - mean_b;
        num = da.mul_add(db, num);
        den_a = da.mul_add(da, den_a);
        den_b = db.mul_add(db, den_b);
    }
    let den = (den_a * den_b).sqrt();
    if den == 0.0 { 0.0 } else { num / den }
}

/// Best correlation over a search for the alignment offset.
///
/// Opus has ~6.4ms of algorithmic delay at 48kHz (309 samples), and an SFU path adds an
/// arbitrary further offset. Comparing at offset zero would report catastrophic
/// decorrelation for a perfectly intact signal -- a mistake made once already during this
/// investigation, which produced a false alarm.
fn best_correlation(reference: &[f32], received: &[f32], max_offset: usize) -> (f32, usize) {
    let mut best = (0.0_f32, 0_usize);
    for offset in 0..max_offset.min(received.len()) {
        let c = correlation(reference, &received[offset..]);
        if c > best.0 {
            best = (c, offset);
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Layer 1: the codec alone
// ---------------------------------------------------------------------------

fn mono_encoder() -> OpusEncoder {
    OpusEncoder::with_config(SAMPLE_RATE, CHANNELS, OpusEncoderConfig::default())
        .expect("mono Opus encoder")
}

/// Layer 1: encode then decode, in process. Establishes the fidelity ceiling every layer
/// above is measured against.
#[test]
fn layer1_codec_preserves_frame_count_duration_and_waveform() {
    let mut encoder = mono_encoder();
    let mut decoder = OpusDecoder::new(SAMPLE_RATE, CHANNELS).expect("mono Opus decoder");

    let frames = reference_frames(100);
    let mut reference = Vec::new();
    let mut received = Vec::new();

    for frame in &frames {
        reference.extend_from_slice(frame);
        let packet = encoder
            .encode(&AudioFrame {
                sample_rate: SAMPLE_RATE,
                channels: CHANNELS,
                data: frame.clone(),
                timestamp_us: 0,
            })
            .expect("encode");
        let out = decoder.decode(&packet, FRAME_SAMPLES).expect("decode");

        // Duration must survive exactly: a frame that decodes to a different number of
        // samples than it encoded shifts every subsequent frame's playout and is a
        // classic cause of continuous receiver-side time-stretching.
        assert_eq!(
            out.data.len(),
            FRAME_SAMPLES * usize::from(CHANNELS),
            "one 20ms mono frame must decode to exactly {FRAME_SAMPLES} samples"
        );
        assert_eq!(out.channels, CHANNELS);
        assert_eq!(out.sample_rate, SAMPLE_RATE);
        received.extend_from_slice(&out.data);
    }

    assert_eq!(received.len(), reference.len(), "total duration must match");

    // Search a generous window: Opus's algorithmic delay is ~309 samples at 48kHz.
    let (corr, offset) = best_correlation(&reference, &received, 1000);
    assert!(
        corr > 0.9,
        "codec round trip should preserve the waveform (correlation {corr:.3} at offset {offset})"
    );
}

// ---------------------------------------------------------------------------
// Layer 2: E2EE wrapped around the codec
// ---------------------------------------------------------------------------

const TEST_KEY: &[u8] = b"layer-bisection-key-material-01";
const LOCAL: &str = "alice";

fn e2ee_ctx(key_index: u8) -> E2eeContext {
    let ctx = E2eeContext::new(E2eeOptions::default());
    ctx.set_local_identity(LOCAL);
    ctx.set_key(LOCAL, key_index, TEST_KEY);
    ctx
}

/// Layer 2: every encrypted frame must decrypt back to a byte-identical Opus packet.
///
/// Stronger than "it still decodes": a frame whose payload is shifted by a byte often
/// still decodes to plausible-sounding audio, which is how a two-byte trailer bug survived
/// undetected here for a long time. Byte equality admits no such ambiguity.
#[test]
fn layer2_e2ee_round_trip_returns_the_opus_payload_byte_for_byte() {
    let ctx = e2ee_ctx(0);
    let mut encoder = mono_encoder();

    for frame in reference_frames(50) {
        let packet = encoder
            .encode(&AudioFrame {
                sample_rate: SAMPLE_RATE,
                channels: CHANNELS,
                data: frame,
                timestamp_us: 0,
            })
            .expect("encode");

        let wire = ctx
            .encrypt_frame(&packet, MediaKind::Audio)
            .expect("encryption is configured");
        assert_ne!(
            wire.as_bytes(),
            packet.as_bytes(),
            "frame must be encrypted"
        );

        let back = ctx
            .decrypt_frame(&wire, LOCAL, MediaKind::Audio)
            .expect("decrypt must not error")
            .expect("a key is held");
        assert_eq!(
            back.as_bytes(),
            packet.as_bytes(),
            "decrypted payload must be byte-identical to what the encoder produced"
        );
    }
}

/// Layer 2b: an encrypted frame must still be intelligible audio after decode.
#[test]
fn layer2_audio_survives_encrypt_decrypt_decode() {
    let ctx = e2ee_ctx(0);
    let mut encoder = mono_encoder();
    let mut decoder = OpusDecoder::new(SAMPLE_RATE, CHANNELS).expect("decoder");

    let frames = reference_frames(100);
    let mut reference = Vec::new();
    let mut received = Vec::new();

    for frame in &frames {
        reference.extend_from_slice(frame);
        let packet = encoder
            .encode(&AudioFrame {
                sample_rate: SAMPLE_RATE,
                channels: CHANNELS,
                data: frame.clone(),
                timestamp_us: 0,
            })
            .expect("encode");
        let wire = ctx
            .encrypt_frame(&packet, MediaKind::Audio)
            .expect("encrypt");
        let back = ctx
            .decrypt_frame(&wire, LOCAL, MediaKind::Audio)
            .expect("decrypt")
            .expect("key held");
        received.extend_from_slice(&decoder.decode(&back, FRAME_SAMPLES).expect("decode").data);
    }

    let (corr, _) = best_correlation(&reference, &received, 1000);
    assert!(
        corr > 0.9,
        "E2EE must not degrade the audio (correlation {corr:.3})"
    );
}

// ---------------------------------------------------------------------------
// Layer 3: key rotation
// ---------------------------------------------------------------------------

/// Layer 3: every rotation index a sender can reach must decrypt with the key held at it.
///
/// Regression for two real field bugs on the same line. livekit writes the sender's
/// rotation counter into the frame trailer unreduced -- Element Call rotates with
/// `(prev + 1) % 256` -- so the byte routinely exceeds 15. Rejecting it made a peer
/// completely inaudible while every packet arrived normally and no key was missing;
/// reducing it modulo 16 instead then aliased index 19 onto index 3, so one participant's
/// rotation destroyed another's live key.
///
/// Sweeping the whole byte range rather than spot-checking: both faults only appear past
/// 15, so a test that stopped there would have passed throughout.
#[test]
fn layer3_every_rotation_index_decrypts_including_past_the_ring_size() {
    let mut encoder = mono_encoder();
    let packet = encoder
        .encode(&AudioFrame {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            data: reference_frames(1).remove(0),
            timestamp_us: 0,
        })
        .expect("encode");

    // Every value the trailer byte can hold, each with its key at its own index.
    for rotation in [0u8, 1, 15, 16, 17, 19, 25, 63, 128, 200, 255] {
        let ctx = e2ee_ctx(rotation);
        let wire = ctx
            .encrypt_frame(&packet, MediaKind::Audio)
            .expect("encrypt");

        // The encoder writes the index it was given; a real peer writes the same counter.
        assert_eq!(
            wire.as_bytes().last().copied(),
            Some(rotation),
            "rotation {rotation} must reach the wire unreduced"
        );

        let back = ctx
            .decrypt_frame(
                &WireMedia::from_network(wire.as_bytes().to_vec()),
                LOCAL,
                MediaKind::Audio,
            )
            .unwrap_or_else(|e| panic!("rotation {rotation} must decrypt: {e}"))
            .unwrap_or_else(|| panic!("rotation {rotation} found no key"));
        assert_eq!(back.as_bytes(), packet.as_bytes(), "rotation {rotation}");
    }
}

// ---------------------------------------------------------------------------
// Layers 4 and 5: through a real SFU
// ---------------------------------------------------------------------------

const DEV_API_KEY: &str = "devkey";
const DEV_API_SECRET: &str = "secret";
const SFU_URL: &str = "http://127.0.0.1:7880";

/// Fraction of sent frames that must arrive decoded at the subscriber.
///
/// Not 1.0: a few frames are genuinely in flight when the measurement window closes, and
/// the first packets can be sent before the subscriber's ICE settles. But a threshold at
/// all is the entire point -- the previous test asserted only that the count was nonzero,
/// which is satisfied by a stream that loses 95% of its audio.
const MIN_DELIVERY_RATIO: f64 = 0.90;

/// Longest establishment phase tolerated before giving up, in 20ms frames (40 seconds).
const WARMUP_CAP_FRAMES: usize = 2000;

/// The one process-wide log capture.
///
/// `tracing::subscriber::set_global_default` succeeds exactly once per process, so a
/// per-test `LogCapture` would leave every test after the first silently receiving nothing
/// -- and a delivery ratio computed from an empty capture reads as total packet loss. The
/// tests that use it run with `--test-threads=1` and scope their window with `clear()`.
fn shared_capture() -> &'static LogCapture {
    static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    CAPTURE.get_or_init(|| {
        let c = LogCapture::new();
        c.install_global();
        c
    })
}

/// Encode one reference frame and publish it, pacing at the real 20ms frame interval.
///
/// Returns whether the write was accepted.
async fn send_one(room: &LiveKitRoom, encoder: &mut OpusEncoder, frame: Vec<f32>) -> bool {
    let packet = encoder
        .encode(&AudioFrame {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            data: frame,
            timestamp_us: 0,
        })
        .expect("encode");
    let ok = room.write_audio(packet).await.is_ok();
    tokio::time::sleep(Duration::from_millis(20)).await;
    ok
}

/// Frames the subscriber has decoded so far, per the per-frame TRACE event.
fn decoded_count(capture: &LogCapture) -> usize {
    capture
        .events()
        .iter()
        .filter(|e| {
            e.message()
                .is_some_and(|m| m.contains("Inbound audio frame decoded"))
        })
        .count()
}

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

fn mint_token(identity: &str, room: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    encode(
        &Header::new(Algorithm::HS256),
        &Claims {
            iss: DEV_API_KEY.to_owned(),
            sub: identity.to_owned(),
            iat: now,
            exp: now.saturating_add(3600),
            nbf: now,
            jti: identity.to_owned(),
            name: identity.to_owned(),
            video: VideoGrant {
                room: room.to_owned(),
                room_join: true,
                can_publish: true,
                can_subscribe: true,
                can_publish_data: true,
            },
        },
        &EncodingKey::from_secret(DEV_API_SECRET.as_bytes()),
    )
    .expect("JWT encoding with a valid HS256 secret cannot fail")
}

fn empty_video_buffer() -> elementium_webrtc::engine::VideoFrameBuffer {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Publish `frame_count` mono frames from alice, count what bob decodes, and assert the
/// delivery ratio.
///
/// Returns the ratio so callers can report it even when it passes -- a run at 0.91 and a
/// run at 1.00 are very different states of health, and a bare pass hides that.
async fn measure_sfu_delivery(
    policy_for: impl Fn() -> EncryptionPolicy,
    frame_count: usize,
) -> f64 {
    let capture = shared_capture();
    capture.clear();

    let room_name = format!(
        "elementium-bisect-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis()
    );

    let (mut alice, _alice_events) = LiveKitRoom::connect(
        SFU_URL,
        &mint_token("alice", &room_name),
        empty_video_buffer(),
        CorrelationId::new(),
        policy_for(),
    )
    .await
    .expect("alice connects to the local dev server");

    let (_bob, _bob_events) = LiveKitRoom::connect(
        SFU_URL,
        &mint_token("bob", &room_name),
        empty_video_buffer(),
        CorrelationId::new(),
        policy_for(),
    )
    .await
    .expect("bob connects to the local dev server");

    // Fixed settling delays, matching the existing round-trip test: `SignalSender::send`
    // is fire-and-forget, so there is no "signaling ready" signal to wait on.
    tokio::time::sleep(Duration::from_secs(3)).await;
    alice.publish_track("audio", "microphone").expect("publish");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Warm-up is event-driven, not a fixed sleep. On a machine with several network
    // interfaces (LAN, Docker bridges, a VPN, IPv6) the subscriber's ICE connectivity
    // checks can take 10+ seconds to settle before any RTP flows at all, and a fixed guess
    // either wastes time or silently measures the connection-establishment window as
    // packet loss. Send until the first frame is seen decoded, then start counting.
    let mut encoder = mono_encoder();
    let mut source = reference_frames(WARMUP_CAP_FRAMES + frame_count).into_iter();
    let mut established = false;

    for _ in 0..WARMUP_CAP_FRAMES {
        let Some(frame) = source.next() else { break };
        let _ = send_one(&alice, &mut encoder, frame).await;
        if decoded_count(capture) > 0 {
            established = true;
            break;
        }
    }

    assert!(
        established,
        "no audio ever reached the subscriber within {}s of publishing -- the path never \
         established at all, which is a different failure from losing frames once it did",
        WARMUP_CAP_FRAMES / 50
    );

    // Everything above was establishment; the measurement window starts here.
    capture.clear();
    let mut sent = 0_usize;

    for _ in 0..frame_count {
        let Some(frame) = source.next() else { break };
        if send_one(&alice, &mut encoder, frame).await {
            sent = sent.saturating_add(1);
        }
    }

    // Let the last packets in flight finish arriving before counting.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let events = capture.events();
    let decoded = decoded_count(capture);
    let decrypt_failures = events
        .iter()
        .filter(|e| e.message().is_some_and(|m| m.contains("decrypt failed")))
        .count();
    let decode_failures = events
        .iter()
        .filter(|e| {
            e.message()
                .is_some_and(|m| m.contains("Failed to decode inbound Opus frame"))
        })
        .count();

    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    let ratio = if sent == 0 {
        0.0
    } else {
        decoded as f64 / sent as f64
    };

    println!(
        "delivery: sent={sent} decoded={decoded} ratio={ratio:.3} \
         decrypt_failures={decrypt_failures} decode_failures={decode_failures}"
    );

    assert_eq!(
        decrypt_failures, 0,
        "no frame should fail to decrypt at the subscriber"
    );
    assert_eq!(decode_failures, 0, "no frame should fail to decode");
    ratio
}

/// Layer 4: a real SFU relay, no E2EE. Isolates transport from encryption.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a local livekit-server (see module docs)"]
async fn layer4_unencrypted_audio_survives_the_sfu() {
    let ratio = measure_sfu_delivery(|| EncryptionPolicy::ExplicitlyUnencrypted, 250).await;
    assert!(
        ratio >= MIN_DELIVERY_RATIO,
        "only {:.1}% of unencrypted frames arrived; the transport path is losing audio",
        ratio * 100.0
    );
}

/// Layer 5: the same relay with E2EE active on both ends.
///
/// The one test that would have caught the bug this suite was written for. If layer 4
/// passes and this fails, the fault is in encryption, not transport -- a distinction that
/// took several rounds of live-call guessing to make by ear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a local livekit-server (see module docs)"]
async fn layer5_encrypted_audio_survives_the_sfu() {
    // Both rooms need a context holding the same key, since each side encrypts as itself
    // and decrypts whatever arrives by trying every key it holds.
    let ratio = measure_sfu_delivery(
        || {
            let ctx = E2eeContext::new(E2eeOptions::default());
            ctx.set_local_identity(LOCAL);
            ctx.set_key(LOCAL, 0, TEST_KEY);
            EncryptionPolicy::Encrypted(ctx)
        },
        250,
    )
    .await;
    assert!(
        ratio >= MIN_DELIVERY_RATIO,
        "only {:.1}% of encrypted frames arrived; encryption is losing audio that layer 4 \
         proves the transport delivers",
        ratio * 100.0
    );
}
