//! Subscribe to a room and report what arrives at the packetiser boundary.
//!
//! Built for one question that nothing else in the repo can answer. Our encrypted H.264 is
//! forwarded correctly by the SFU, decoded by our own client, and rejected by Chrome's
//! depacketiser. A wire capture cannot settle it: SRTP hides the payload, so a `.pcap`
//! shows RTP headers and nothing about the bytes that matter. The SFU's own stats only say
//! the stream parsed.
//!
//! What is left is a comparison of what each *sender* hands to its packetiser. Running this
//! against a browser publisher recovers libwebrtc's side of that -- str0m depacketises the
//! stream back into frames, and `ELEMENTIUM_FRAME_DUMP` prints the head of each one. Run
//! `publish_test_tone --video-h264` with the same variable set and the two byte strings sit
//! side by side.
//!
//! ```bash
//! ELEMENTIUM_FRAME_DUMP=1 subscribe_frame_dump --room <name> --seconds 20 [--key-hex <hex>]
//! ```
//!
//! `--key-hex` only decides whether this client announces itself as encrypted; the dump
//! happens before any decryption, which is the point -- it is the depacketiser's view.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use elementium_types::CorrelationId;
use elementium_webrtc::EncryptionPolicy;
use elementium_webrtc::livekit::room::LiveKitRoom;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

const DEV_API_KEY: &str = "devkey";
const DEV_API_SECRET: &str = "secret";

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
                can_publish: false,
                can_subscribe: true,
                can_publish_data: false,
            },
        },
        &EncodingKey::from_secret(DEV_API_SECRET.as_bytes()),
    )
    .unwrap_or_default()
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
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let sfu = arg("--sfu").unwrap_or_else(|| "http://127.0.0.1:7880".to_owned());
    let room = arg("--room").expect("--room is required");
    let identity = arg("--identity").unwrap_or_else(|| "rust-frame-dump".to_owned());
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);

    let ctx = arg("--key-hex").map(|hex| {
        let material = decode_hex(&hex).expect("--key-hex must be valid hex");
        let ctx = elementium_e2ee::E2eeContext::new(elementium_e2ee::E2eeOptions::default());
        ctx.set_local_identity(&identity);
        ctx.set_key(&identity, 0, &material);
        ctx
    });
    let policy = ctx.map_or(
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

    println!("SUBSCRIBED");
    if std::env::var_os("ELEMENTIUM_FRAME_DUMP").is_none() {
        println!("(set ELEMENTIUM_FRAME_DUMP=1 to print the head of each arriving frame)");
    }

    tokio::time::sleep(Duration::from_secs(seconds)).await;
    room_conn.disconnect().await;
    println!("DONE");
}
