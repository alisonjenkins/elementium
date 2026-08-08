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
//!
//! `--bad-frames N` encrypts the first N frames with a key the far end does not hold, then
//! switches to the correct one. This models the real join sequence, where media starts
//! flowing before every peer has our key -- from the receiver's side the two are
//! indistinguishable. It exists to test whether a receiver ever recovers.
//!
//! `--rotate-frames N` switches to the next key every N frames, cycling through keys derived
//! from `--key-hex` by appending the index. Element Call rotates keys as participants come
//! and go, and a rotation the two sides disagree about drops only the frames encrypted
//! during the disagreement -- which sounds like part of a sentence going missing, not like
//! silence. A static-key test cannot see it.

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

/// The published video size. Small enough to encode comfortably in software or hardware,
/// large enough that a wrong stride or a half-decrypted frame is unmistakable.
/// Overridable so a run can publish frames small enough to fit one RTP packet. Whether a
/// frame is fragmented is the difference between two code paths in the packetiser, and
/// sizing it below the MTU is the cheapest way to ask which one is at fault.
fn video_size() -> (u32, u32) {
    match arg("--video-size").as_deref() {
        Some("tiny") => (160, 120),
        _ => (320, 240),
    }
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

/// Key material for rotation `index`, derived from the base key so both sides can compute
/// the same sequence without exchanging anything beyond the base key and the index.
fn rotated_key(base: &[u8], index: u8) -> Vec<u8> {
    let mut key = base.to_vec();
    if let Some(first) = key.first_mut() {
        *first ^= index;
    }
    key
}

/// A key the far end will not hold, at the same index as the real one.
fn wrong_key(base: &[u8]) -> Vec<u8> {
    let mut key = base.to_vec();
    if let Some(b) = key.get_mut(1) {
        *b ^= 0xFF;
    }
    key
}

/// One frame of a moving checkerboard, as I420.
///
/// Moving, not static: a receiver that decodes only the keyframe and then repeats it looks
/// identical to a working one on a still picture. The pattern is high-contrast so a frame
/// that arrives half-decrypted is visibly wrong rather than merely noisy.
#[allow(clippy::expect_used)]
fn checkerboard(width: u32, height: u32, step: u32) -> elementium_types::I420Frame {
    let cols = usize::try_from(width).unwrap_or(0);
    let rows = usize::try_from(height).unwrap_or(0);
    let shift = usize::try_from(step).unwrap_or(0).saturating_mul(8);
    let mut luma = vec![0_u8; cols.saturating_mul(rows)];
    for (i, px) in luma.iter_mut().enumerate() {
        let col = i.checked_rem(cols.max(1)).unwrap_or(0);
        let row = i.checked_div(cols.max(1)).unwrap_or(0);
        let block = col
            .saturating_add(shift)
            .checked_div(16)
            .unwrap_or(0)
            .saturating_add(row.checked_div(16).unwrap_or(0));
        *px = if block.is_multiple_of(2) { 210 } else { 30 };
    }
    let chroma = cols.div_ceil(2).saturating_mul(rows.div_ceil(2));
    elementium_types::I420Frame::from_planes(
        width,
        height,
        &luma,
        &vec![100_u8; chroma],
        &vec![150_u8; chroma],
        0,
    )
    .expect("the planes are sized from the geometry, so this cannot fail")
}

/// Announce the tracks this publisher will send, and let each negotiation settle.
///
/// Spaced rather than issued back to back: each publish triggers its own offer/answer, and
/// two in flight at once left the second answer describing mids the first offer never had.
#[allow(clippy::expect_used)]
async fn publish_tracks(room: &mut LiveKitRoom, codec: Option<elementium_codec::VideoCodec>) {
    room.publish_track("audio", "microphone")
        .expect("publish the audio track");
    if let Some(codec) = codec {
        tokio::time::sleep(Duration::from_secs(2)).await;
        room.publish_video_track("camera", codec, video_size().0, video_size().1)
            .expect("publish the video track");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
}

/// Which video codec to publish, if any.
///
/// A flag so the same harness can publish VP8, which is the control: if VP8 assembles at
/// the receiver and H.264 does not, the fault is H.264-specific. If neither does, video
/// publishing through this path has never worked -- and until this example existed nothing
/// had ever tried it.
fn requested_video_codec() -> Option<elementium_codec::VideoCodec> {
    if std::env::args().any(|a| a == "--video-h264") {
        Some(elementium_codec::VideoCodec::H264)
    } else if std::env::args().any(|a| a == "--video-vp8") {
        Some(elementium_codec::VideoCodec::Vp8)
    } else {
        None
    }
}

/// The E2EE context this publisher encrypts with, seeded with its starting key.
fn build_e2ee_context(
    identity: &str,
    material: &[u8],
    bad_frames: u64,
    key_index: u8,
) -> elementium_e2ee::E2eeContext {
    let ctx = elementium_e2ee::E2eeContext::new(elementium_e2ee::E2eeOptions::default());
    ctx.set_local_identity(identity);
    // Start on a key the far end does not have, if asked: `--bad-frames` needs the first
    // frames to be undecryptable, and encrypting with the wrong key is exactly what the
    // receiver sees when it has not yet been given the right one.
    let start = if bad_frames > 0 {
        wrong_key(material)
    } else {
        rotated_key(material, 0)
    };
    // The starting index is a parameter because the frame trailer's last byte *is* the key
    // index, and a zero there is a trailing zero on the NAL. Anything that trims those --
    // an SFU, a depacketiser -- corrupts the trailer for index 0 and leaves every other
    // index working, which is a difference only a run at a non-zero index can show.
    ctx.set_key(identity, key_index, &start);
    ctx
}

/// The H.264 encoder for the published video track.
///
/// Panics rather than degrading if there is none: a run asked for video, and publishing
/// silence in its place would make the test report a framing failure it never tested.
#[allow(clippy::expect_used)]
fn build_video_encoder(codec: elementium_codec::VideoCodec) -> elementium_codec::NegotiatedEncoder {
    elementium_codec::NegotiatedEncoder::new(
        codec,
        elementium_codec::EncoderConfig {
            width: video_size().0,
            height: video_size().1,
            bitrate_kbps: 1_000,
            max_framerate: 25,
        },
    )
    .expect("a video encoder for the requested codec")
}

/// Encode and send one video frame, on every second audio tick.
///
/// 25fps against the tone's 50: enough to show motion without making the encoder the
/// pacing bottleneck for the audio, which is what the rest of this harness measures.
///
/// Returns how many packets reached the wire, so the caller's counter stays a count of what
/// was actually sent rather than what was offered.
async fn send_video_frame(
    encoder: &mut elementium_codec::NegotiatedEncoder,
    room: &LiveKitRoom,
    tick: u64,
    codec: elementium_codec::VideoCodec,
) -> u64 {
    if !tick.is_multiple_of(2) {
        return 0;
    }
    let step = u32::try_from(tick.checked_div(2).unwrap_or(0)).unwrap_or(0);
    let Ok(packets) = elementium_codec::VideoEncoder::encode(
        encoder,
        &checkerboard(video_size().0, video_size().1, step),
    ) else {
        return 0;
    };
    let mut sent: u64 = 0;
    for packet in packets {
        if room
            .write_video(packet.data, codec)
            .await
            .is_ok()
        {
            sent = sent.saturating_add(1);
        }
    }
    sent
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
        .map(|i| {
            s.get(i..i.saturating_add(2))
                .and_then(|b| u8::from_str_radix(b, 16).ok())
        })
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
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);

    let rotate_frames: u64 = arg("--rotate-frames")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bad_frames: u64 = arg("--bad-frames")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Publishing video as well as the tone. H.264 specifically: its E2EE framing differs
    // from VP8's -- a different clear-header rule and RBSP-escaped ciphertext -- and that
    // contract was until now only checked against livekit's *source*. This is what checks
    // it against livekit's running worker.
    let video_codec = requested_video_codec();

    let base_key =
        arg("--key-hex").map(|hex| decode_hex(&hex).expect("--key-hex must be valid hex"));
    let key_index: u8 = arg("--key-index").and_then(|s| s.parse().ok()).unwrap_or(0);
    let ctx = base_key
        .as_ref()
        .map(|material| build_e2ee_context(&identity, material, bad_frames, key_index));
    let policy = ctx.clone().map_or(
        EncryptionPolicy::ExplicitlyUnencrypted,
        EncryptionPolicy::Encrypted,
    );

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
    publish_tracks(&mut room_conn, video_codec).await;

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

    let mut video_encoder = video_codec.map(build_video_encoder);
    let mut video_sent: u64 = 0;

    let mut current_key_index: u8 = key_index;
    for i in 0..frames {
        ticker.tick().await;

        // Switch to the key the far end actually holds, at the same index.
        if bad_frames > 0
            && i == bad_frames
            && let (Some(ctx), Some(base)) = (ctx.as_ref(), base_key.as_ref())
        {
            ctx.set_key(&identity, 0, &rotated_key(base, 0));
            println!("KEY_CORRECTED");
        }

        // Rotate on schedule, exactly as a client would when the room membership changes.
        if rotate_frames > 0
            && i > 0
            && i.is_multiple_of(rotate_frames)
            && let (Some(ctx), Some(base)) = (ctx.as_ref(), base_key.as_ref())
        {
            current_key_index = current_key_index.wrapping_add(1);
            ctx.set_key(
                &identity,
                current_key_index,
                &rotated_key(base, current_key_index),
            );
            println!("ROTATED {current_key_index}");
        }

        if let Some(encoder) = video_encoder.as_mut() {
            let codec = video_codec.unwrap_or(elementium_codec::VideoCodec::Vp8);
            video_sent = video_sent
                .saturating_add(send_video_frame(encoder, &room_conn, i, codec).await);
        }

        let frame = tone_frame(sample_index);
        sample_index = sample_index.wrapping_add(u32::try_from(FRAME_SAMPLES).unwrap_or(960));
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
    if video_codec.is_some() {
        println!("VIDEO_SENT {video_sent}");
    }
    room_conn.disconnect().await;
}
