//! Publish a known test tone to a `LiveKit` room, so a *browser* can be measured receiving it.
//!
//! The layered bisection suite proves our own Rust client can send audio through a real SFU
//! to another Rust client with zero loss. That leaves the receiver untested: in production
//! the far end is Chromium, running `libwebrtc`'s `NetEq` jitter buffer and `livekit`'s
//! insertable-streams E2EE worker -- neither of which our Rust decoder stands in for. A
//! fault that only manifests there is invisible to every test we have.
//!
//! This binary is the publisher half of that measurement. The browser half lives in
//! `frontend/tests/browser/`, driven by Playwright, which spawns this process and then
//! reads `RTCPeerConnection.getStats()` on the receiving side.
//!
//! Usage:
//!
//! ```bash
//! publish_test_tone --room <name> --identity <id> --seconds <n> [--key-hex <hex>]
//! ```
//!
//! `--key-hex` enables E2EE with that raw key material at index 0; without it the stream is
//! published unencrypted. Both are needed: comparing them is what separates "the browser
//! cannot decrypt our frames" from "the browser cannot play our audio".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use elementium_types::{AudioFrame, CorrelationId};
use elementium_webrtc::EncryptionPolicy;
use elementium_webrtc::livekit::room::LiveKitRoom;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

const DEV_API_KEY: &str = "devkey";
const DEV_API_SECRET: &str = "secret";
const SAMPLE_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 960;
/// Outbound audio is mono, matching the capture pipeline.
const CHANNELS: u16 = 1;

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
        .map_or(0, |d| d.as_secs());
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
    .unwrap_or_default()
}

/// One 20ms mono frame of a 440Hz sine at half scale.
///
/// A pure tone rather than noise or speech: the receiver's measurement is a concealment
/// *rate*, and a signal the browser can also be checked against spectrally later is worth
/// more than a realistic one.
fn tone_frame(start_sample: u32) -> Vec<f32> {
    let mut frame = Vec::with_capacity(FRAME_SAMPLES);
    let mut n = start_sample;
    for _ in 0..FRAME_SAMPLES {
        let t = f32::from(u16::try_from(n % SAMPLE_RATE).unwrap_or(0)) / 48_000.0_f32;
        frame.push((t * 440.0 * std::f32::consts::TAU).sin() * 0.5);
        n = n.wrapping_add(1);
    }
    frame
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i.saturating_add(1)))
        .cloned()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i.saturating_add(2)).and_then(|b| u8::from_str_radix(b, 16).ok()))
        .collect()
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::expect_used, clippy::print_stdout)]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let sfu = arg("--sfu").unwrap_or_else(|| "http://127.0.0.1:7880".to_owned());
    let room = arg("--room").expect("--room is required");
    let identity = arg("--identity").unwrap_or_else(|| "rust-publisher".to_owned());
    let seconds: u64 = arg("--seconds")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let policy = arg("--key-hex").map_or(EncryptionPolicy::ExplicitlyUnencrypted, |hex| {
        let material = decode_hex(&hex).expect("--key-hex must be valid hex");
        let ctx = elementium_e2ee::E2eeContext::new(elementium_e2ee::E2eeOptions::default());
        ctx.set_local_identity(&identity);
        ctx.set_key(&identity, 0, &material);
        EncryptionPolicy::Encrypted(ctx)
    });

    let video_frames: elementium_webrtc::engine::VideoFrameBuffer =
        Arc::new(Mutex::new(HashMap::new()));

    let (mut room_conn, _events) = LiveKitRoom::connect(
        &sfu,
        &mint_token(&identity, &room),
        video_frames,
        CorrelationId::new(),
        policy,
    )
    .await
    .expect("connect to the SFU");

    // Settle signaling before publishing: SignalSender::send is fire-and-forget, so an
    // AddTrack sent too early can be lost with no error surfaced.
    tokio::time::sleep(Duration::from_secs(2)).await;
    room_conn
        .publish_track("audio", "microphone")
        .expect("publish the audio track");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Tell the driving test we are live, so it can stop guessing at timing.
    println!("PUBLISHING");

    let mut encoder = elementium_codec::OpusEncoder::with_config(
        SAMPLE_RATE,
        CHANNELS,
        elementium_codec::OpusEncoderConfig::default(),
    )
    .expect("Opus encoder");

    let frames = seconds.saturating_mul(50);
    let mut sample_index: u32 = 0;
    let mut sent: u64 = 0;

    // Absolute-deadline pacing, not `sleep(20ms)` in a loop. Sleeping between sends makes
    // the real period 20ms *plus* the encode time, so the stream runs a few percent slow
    // and drifts without bound -- and a receiver's jitter buffer answers a slow sender by
    // continuously time-stretching, which is precisely the artefact this harness exists to
    // measure. A test publisher that produces it by itself cannot measure it.
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

    for _ in 0..frames {
        ticker.tick().await;
        let frame = tone_frame(sample_index);
        sample_index = sample_index.wrapping_add(
            u32::try_from(FRAME_SAMPLES).unwrap_or(960),
        );
        if let Ok(packet) = encoder.encode(&AudioFrame {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            data: frame,
            timestamp_us: 0,
        }) && room_conn.write_audio(packet).await.is_ok()
        {
            sent = sent.saturating_add(1);
        }
    }

    // The receiving side reads this to compute a delivery ratio against what we actually
    // put on the wire, rather than against what it hoped we would.
    println!("SENT {sent}");
    room_conn.disconnect().await;
}
