//! Publish a real screen share to a `LiveKit` room, so a *browser* can be measured receiving it.
//!
//! The publisher half of SC-001: "a screen share started in a call produces a steadily
//! increasing decoded frame count at the receiving participant, sustained for at least 30
//! seconds". The browser half lives in `frontend/tests/browser/receive-path.spec.ts`, which
//! spawns this process and reads `RTCPeerConnection.getStats()` on the receiving side.
//!
//! Deliberately *not* a synthetic source. `publish_test_tone --video-vp8` already publishes
//! a generated checkerboard, and it passes whether or not screen capture works at all --
//! which is exactly the gap this feature was written to close. This binary goes through the
//! real path: the `XDG` `ScreenCast` portal, a `PipeWire` node, the DMA-BUF import, the shared
//! encode path, and the same `MediaTrackKey::screen_share()` routing the app uses. If any
//! link in that chain breaks, the receiver's frame count stops climbing and this test fails.
//!
//! Usage:
//!
//! ```bash
//! publish_screen_share --room <name> --identity <id> --seconds <n> [--key-hex <hex>] [--audio]
//! ```
//!
//! **This shows the portal picker and blocks until a person chooses.** That is not a defect
//! to be worked around here: the portal exists so that no application can start capturing a
//! screen without the user knowing, and a test binary that bypassed it would be testing a
//! path no user ever takes. Runs driven by this are attended by design.
//!
//! `--audio` additionally captures share audio -- the publisher half of SC-004: "with share
//! audio enabled, a tone played by the shared source is present in the receiver's audio
//! while the microphone remains independently audible and independently mutable." The XDG
//! portal carries no audio in any backend, so this reads it the only way it can be read: a
//! direct `PipeWire` connection to a desktop sink's monitor, exactly as
//! `src-tauri/src/commands/media_devices.rs`'s `screen_share_audio_capture_loop` does for
//! the real app. It picks the first `Audio/Sink` node `list_audio_sources()` reports rather
//! than a named one, because this binary is a harness proving the path works at all, not a
//! device picker -- that UI is what feeds the real `list_audio_sources()` call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use elementium_types::{CorrelationId, MediaTrackKey};
use elementium_webrtc::EncryptionPolicy;
use elementium_webrtc::livekit::room::LiveKitRoom;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

const DEV_API_KEY: &str = "devkey";
const DEV_API_SECRET: &str = "secret";

/// Share audio is always encoded mono, matching the app's own capture pipeline (see
/// `OUTBOUND_CHANNELS` in `src-tauri/src/commands/media_devices.rs`).
const AUDIO_CHANNELS: u16 = 1;

/// How long to wait for the first captured frame before giving up.
///
/// Generous because the clock starts at the portal dialog, and a person reading it takes as
/// long as they take. The failure this bounds is a capture that negotiates and then never
/// pumps -- which looks identical to a slow human until the deadline passes.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_mins(2);

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

/// Mint a dev-mode join token.
///
/// Duplicated from `publish_test_tone` rather than shared: that example is a working
/// measurement harness several browser tests already depend on, and refactoring it to
/// extract sixty lines of token minting would put those tests at risk for no gain to this
/// one. The duplication is confined to test binaries and to the dev SFU's fixed credentials.
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

/// Wait for the first frame, which is also how the published geometry is learned.
///
/// The share's size is the compositor's to decide -- a window is whatever size it is -- so
/// there is nothing to publish until a frame has arrived and said. Publishing a guess and
/// correcting later would mean renegotiating mid-share on every run.
fn await_first_frame(
    source: &elementium_media::video_source::VideoSource,
) -> Option<elementium_media::captured_frame::CapturedFrame> {
    let deadline = std::time::Instant::now().checked_add(FIRST_FRAME_TIMEOUT)?;
    while std::time::Instant::now() < deadline {
        if let Some(frame) = source.try_recv() {
            return Some(frame);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Encode one captured frame and write every packet it produced to the share's track.
///
/// Returns how many packets reached the wire, so the caller's total is a count of what was
/// actually sent rather than what was offered.
async fn send_frame(
    encoder: &mut elementium_codec::NegotiatedEncoder,
    room: &LiveKitRoom,
    frame: &elementium_media::captured_frame::CapturedFrame,
    codec: elementium_codec::VideoCodec,
) -> u64 {
    let elementium_media::captured_frame::CapturedFrame::Planar(planar) = frame else {
        // A screencast negotiates a raw layout, never MJPEG. Reached only if the capture
        // path changes underneath this, and worth a count of zero rather than a panic.
        return 0;
    };
    let Ok(packets) = elementium_codec::VideoEncoder::encode(encoder, planar) else {
        return 0;
    };
    let mut sent: u64 = 0;
    for packet in packets {
        if room
            .write_video(MediaTrackKey::screen_share(), packet.data, codec)
            .await
            .is_ok()
        {
            sent = sent.saturating_add(1);
        }
    }
    sent
}

/// The first `PipeWire` desktop sink `list_audio_sources()` reports, whose monitor carries
/// the desktop mix -- share audio, on this machine.
///
/// Not "the default sink": there is no such concept in what `PipeWire` reports, and picking
/// by name would tie this harness to one machine's device names. A real caller has a picker
/// UI built on the same enumeration; this binary proves the capture-and-publish path works
/// at all, which needs *a* sink, not a specific one.
fn find_share_audio_sink() -> Option<u32> {
    elementium_media::pipewire_nodes::list_audio_sources()
        .ok()?
        .into_iter()
        .find(|s| s.class == elementium_media::pipewire_nodes::AudioSourceClass::Sink)
        .map(|s| s.node_id)
}

/// Encode every complete 20ms frame currently sitting in `accumulator` and write each to the
/// share's audio track, draining what it consumes.
///
/// Returns how many packets reached the wire, matching [`send_frame`]'s video counterpart:
/// the driving test computes a delivery ratio against what was actually sent, not against
/// how much PCM was captured.
async fn send_audio_chunk(
    encoder: &mut elementium_codec::OpusEncoder,
    room: &LiveKitRoom,
    accumulator: &mut Vec<f32>,
    frame_samples: usize,
    sample_rate: u32,
) -> u64 {
    let mut sent: u64 = 0;
    while accumulator.len() >= frame_samples {
        let frame_data: Vec<f32> = accumulator.drain(..frame_samples).collect();
        let audio_frame = elementium_types::AudioFrame {
            sample_rate,
            channels: AUDIO_CHANNELS,
            data: frame_data,
            timestamp_us: 0,
        };
        if let Ok(packet) = encoder.encode(&audio_frame)
            && room
                .write_audio(MediaTrackKey::screen_share_audio(), packet)
                .await
                .is_ok()
        {
            sent = sent.saturating_add(1);
        }
    }
    sent
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::expect_used, clippy::print_stdout, clippy::too_many_lines)]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let sfu = arg("--sfu").unwrap_or_else(|| "http://127.0.0.1:7880".to_owned());
    let room = arg("--room").expect("--room is required");
    let identity = arg("--identity").unwrap_or_else(|| "rust-screen-publisher".to_owned());
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(35);
    let codec = elementium_codec::VideoCodec::Vp8;
    let audio = std::env::args().any(|a| a == "--audio");

    // The portal first, before the SFU connection: it blocks on a person, and holding a
    // room connection open across that wait would have the publisher time out in the SFU
    // before it ever published anything.
    println!("WAITING_FOR_PICKER");
    let session = elementium_screen::start_share()
        .await
        .expect("the portal must grant a screencast session");
    // Wayland only, on purpose. The X11 backend is push-based and has no node to pull from,
    // and this machine cannot exercise it anyway -- a run that quietly published nothing on
    // X11 would be worse than one that says it does not cover that case.
    let elementium_screen::ShareSource::Wayland { node_id } = *session.source() else {
        // Reported rather than asserted: a run that quietly published nothing would look
        // exactly like a healthy run whose screen had nothing on it.
        println!("UNSUPPORTED_BACKEND this publisher covers the Wayland portal path only");
        return;
    };
    println!("PORTAL_GRANTED {node_id}");

    let source = elementium_media::video_source::VideoSource::start_screencast(
        node_id,
        elementium_codec::EncodeTarget::software(),
    )
    .expect("open the granted screencast node");

    let first = await_first_frame(&source).expect("a frame must arrive from the granted node");
    let (width, height) = (first.width(), first.height());
    println!("CAPTURING {width}x{height}");

    // Opened here, ahead of the SFU connection, for the same reason the video source is:
    // nothing about `PipeWire` capture depends on the room, and a delay in the connection
    // steps below should not push out when the audio graph first hears this stream.
    let audio_capture = if audio {
        let node = find_share_audio_sink().expect(
            "--audio requires at least one PipeWire Audio/Sink node; list_audio_sources() reported none",
        );
        println!("AUDIO_SINK {node}");
        Some(
            elementium_media::pipewire_audio::PipewireAudioCapture::start(
                node,
                elementium_media::pipewire_audio::AudioSourceKind::SinkMonitor,
            )
            .expect("open the share-audio sink monitor"),
        )
    } else {
        None
    };

    let base_key = arg("--key-hex").map(|hex| decode_hex(&hex).expect("--key-hex must be valid hex"));
    let ctx = base_key.as_ref().map(|material| {
        let ctx = elementium_e2ee::E2eeContext::new(elementium_e2ee::E2eeOptions::default());
        ctx.set_local_identity(&identity);
        ctx.set_key(&identity, 0, material);
        ctx
    });
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
    room_conn
        .publish_video_track(MediaTrackKey::screen_share(), codec, width, height)
        .expect("publish the screen share track");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // A second, spaced-out publish, mirroring `publish_tracks` in `publish_test_tone.rs`:
    // two AddTrack requests issued back to back raced an in-flight offer there, and the
    // second answer described m-lines the first offer never had.
    if audio_capture.is_some() {
        room_conn
            .publish_track(MediaTrackKey::screen_share_audio())
            .expect("publish the share audio track");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let opus_rate = elementium_media::pipewire_audio::TARGET_SAMPLE_RATE;
    let mut audio_encoder = audio_capture.is_some().then(|| {
        elementium_codec::OpusEncoder::with_config(
            opus_rate,
            AUDIO_CHANNELS,
            elementium_codec::OpusEncoderConfig::default(),
        )
        .expect("Opus encoder for share audio")
    });
    // 20ms at the negotiated rate, matching every other Opus frame this codebase sends.
    let frame_samples = usize::try_from(opus_rate).unwrap_or(48_000).saturating_mul(20) / 1000;
    let mut audio_accum: Vec<f32> = Vec::with_capacity(frame_samples.saturating_mul(2));
    let mut audio_sent: u64 = 0;

    let mut encoder = elementium_codec::NegotiatedEncoder::new(
        codec,
        elementium_codec::EncoderConfig {
            width,
            height,
            bitrate_kbps: 4_000,
            max_framerate: 30,
        },
    )
    .expect("a video encoder at the captured geometry");

    // Tell the driving test we are live, so it can stop guessing at timing.
    println!("PUBLISHING");

    let mut sent = send_frame(&mut encoder, &room_conn, &first, codec).await;
    let mut frames: u64 = 1;

    // A keyframe every couple of seconds, because a subscriber that joins after the first
    // one has nothing to start decoding from.
    //
    // The app does this properly: an RTCP PLI arrives as `PcEvent::KeyframeRequested` and
    // `src-tauri/src/commands/webrtc.rs` sets the pipeline's keyframe flag. `LiveKitRoom`
    // does not surface that event to a direct caller like this one, so this stands in for
    // it on a timer. Without it the SFU asks for a keyframe, never gets one, and forwards
    // nothing at all -- measured here as 27 PLIs against a publisher happily sending 900
    // packets the receiver could not use.
    let mut last_keyframe = std::time::Instant::now();
    let keyframe_interval = Duration::from_secs(2);

    // Polled rather than paced. A compositor emits on damage, not on a clock, so there is
    // no cadence to hold to here -- the rate is whatever the shared content does, and a
    // fixed ticker would either idle or fall behind depending on what is on screen.
    let deadline = std::time::Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .expect("deadline fits");
    while std::time::Instant::now() < deadline {
        let got_video = if let Some(frame) = source.try_recv() {
            let now = std::time::Instant::now();
            if now.duration_since(last_keyframe) >= keyframe_interval {
                elementium_codec::VideoEncoder::request_keyframe(&mut encoder);
                last_keyframe = now;
            }
            sent = sent.saturating_add(send_frame(&mut encoder, &room_conn, &frame, codec).await);
            frames = frames.saturating_add(1);
            true
        } else {
            false
        };

        // Polled the same way the video source is, and for the same reason: `PipeWire`
        // delivers on its own quantum, not on a cadence this loop should impose.
        let got_audio = if let Some(capture) = audio_capture.as_ref()
            && let Some(aframe) = capture.try_recv()
        {
            let mono = elementium_media::audio_capture::downmix_to_mono(&aframe.data, aframe.channels);
            audio_accum.extend_from_slice(&mono);
            if let Some(enc) = audio_encoder.as_mut() {
                audio_sent = audio_sent.saturating_add(
                    send_audio_chunk(enc, &room_conn, &mut audio_accum, frame_samples, opus_rate).await,
                );
            }
            true
        } else {
            false
        };

        if !got_video && !got_audio {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // The receiving side reads these to compute a delivery ratio against what was actually
    // put on the wire, rather than against what it hoped would be.
    println!("CAPTURED {frames}");
    println!("VIDEO_SENT {sent}");
    if audio_capture.is_some() {
        println!("AUDIO_SENT {audio_sent}");
    }
    room_conn.disconnect().await;
    session.close().await;
}
