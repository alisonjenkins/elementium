//! Audio playback pipeline: str0m → decode → speakers.
//!
//! The capture side (mic → Opus → peer connection) lives in
//! `src-tauri/src/commands/media_devices.rs`, which is the actual code path wired up to
//! `getUserMedia`; this module's own former `AudioPipeline::start_capture` was dead code
//! (never constructed anywhere) and has been removed rather than left to drift out of sync
//! with the real capture implementation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use elementium_codec::OpusDecoder;
use elementium_media::audio_playback::AudioSink;
use elementium_types::PlaintextMedia;

use crate::peer_connection::PcEvent;

/// Whether these bytes really are an Opus packet, checked before decoding them.
///
/// `OpusDecoder::decode` is deliberately tolerant -- libopus turns corrupt input into
/// *noise* rather than erroring -- so a clean decode proves nothing about the input.
/// Parsing the packet's own framing does: real Opus reports a legal sample count (960 =
/// 20ms @ 48kHz for WebRTC audio), while non-Opus bytes (e.g. still-encrypted ciphertext
/// passed through because no E2EE key was configured) fail to parse or report nonsense.
///
/// This is the runtime half of the guarantee whose compile-time half is
/// [`PlaintextMedia`]: the type proves the bytes crossed the decryption boundary, this
/// proves the peer's idea of "plaintext" matches ours. Failing it means playing the bytes
/// would emit noise at the user, so the caller drops the packet instead.
fn is_decodable_opus(
    decoder: &OpusDecoder,
    packet: &PlaintextMedia,
    pc_id: &str,
    mid: &str,
) -> bool {
    let toc_byte = packet.as_bytes().first().copied().unwrap_or(0);
    match decoder.packet_sample_count(packet) {
        Ok(_) => true,
        Err(e) => {
            // Throttled: if this fires it fires for every packet (~50/sec), and the
            // condition is a persistent misconfiguration, not a transient glitch.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::error!(
                    pc_id,
                    mid,
                    opus_len = packet.len(),
                    toc_byte,
                    error = %e,
                    "Inbound audio is NOT valid Opus -- dropping it rather than playing noise. \
                     Most likely it is still encrypted (no E2EE key configured for this peer)."
                );
            });
            false
        }
    }
}

/// Start the playback pipeline: peer connection → Opus decode → speakers.
///
/// `event_rx` provides audio data events from the I/O loop. `player` is a handle to the
/// audio output stream to play decoded frames to -- shared across every
/// `PeerConnection` in the same engine (see
/// [`crate::engine::WebRtcEngine::shared_audio_player`]), not one per connection: a call
/// can involve more than one native `PeerConnection`, and giving each its own output
/// stream meant two independent cpal streams wrote to the same physical device
/// concurrently, a real confirmed cause of audio glitching separate from decode
/// correctness.
///
/// Failures inside the spawned playback thread (decoder setup) are logged rather than
/// propagated, since the thread runs independently of the caller and outlives this call --
/// there is no `Err` this function can ever produce. A `Result` that is always `Ok` taught
/// every caller to write error handling that could never run; see
/// `specs/BACKLOG-2026-08-09-errors.md` X4.
pub fn start_playback(event_rx: Arc<Mutex<mpsc::Receiver<PcEvent>>>, pc_id: String, player: AudioSink) {
    std::thread::spawn(move || {
        // AudioSink::play() resamples/remixes to the device's actual negotiated
        // rate/channels internally, so the decoder can stay fixed at Opus's native
        // 48kHz/stereo regardless of what the output device negotiates -- no manual
        // resample/remix step needed here.
        let play_rate = player.sample_rate();
        let play_channels = player.channels();

        // One decoder per remote track (`mid`), not one shared decoder for the whole PC.
        //
        // A single `PeerConnection` can carry more than one remote audio track -- e.g. two
        // participants' audio bundled onto one connection in a mesh call, confirmed from a
        // real session log. Opus decoding is stateful (LPC history, packet-loss
        // concealment); feeding packets from two independent encoder streams through one
        // decoder in whatever order they happen to arrive corrupts both streams' decoded
        // output. This was the actual root cause of a real "digital screeching" bug report
        // that survived an earlier E2EE-only fix.
        let mut decoders: HashMap<String, OpusDecoder> = HashMap::new();

        tracing::info!(pc_id, play_rate, play_channels, "Audio playback started");
        let mut decoded_count: u64 = 0;
        // How many distinct remote tracks have shown up on this PC's audio playback --
        // observability for the "one PC carries more than one remote track" scenario
        // that caused the per-mid decoder bug, so it's visible in logs going forward
        // instead of having to be inferred from raw SDP dumps.
        let mut seen_mids: std::collections::HashSet<String> = std::collections::HashSet::new();

        loop {
            let event = {
                let Ok(mut rx) = event_rx.lock() else {
                    return;
                };
                rx.try_recv().ok()
            };

            match event {
                // `contiguous` intentionally unused -- see the comment below on why
                // gap-triggered concealment is disabled.
                Some(PcEvent::AudioData {
                    mid,
                    data: opus_packet,
                    contiguous: _,
                }) => {
                    let is_new_track = seen_mids.insert(mid.clone());
                    if is_new_track {
                        tracing::info!(
                            pc_id,
                            mid,
                            distinct_tracks_seen = seen_mids.len(),
                            "New audio track mid observed on this PC's playback pipeline"
                        );
                    }

                    let decoder = match decoders.entry(mid.clone()) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(v) => {
                            match OpusDecoder::new(48000, 2) {
                                Ok(d) => v.insert(d),
                                Err(e) => {
                                    tracing::error!(pc_id, mid, error = %e, "Failed to create Opus decoder for track");
                                    continue;
                                }
                            }
                        }
                    };

                    // Gap-triggered Opus PLC (calling `OpusDecoder::conceal` when str0m
                    // reports `contiguous == false`) was tried and reverted: a test
                    // (`interleaved_plc_calls_corrupt_subsequent_real_decodes`,
                    // elementium-codec/opus_codec.rs) proved `conceal()` corrupts the
                    // *next real packet's* decode, not just the concealment frame itself,
                    // and the corruption doesn't heal on its own. str0m's own docs on
                    // `MediaData::contiguous` warn "For audio this flag most likely
                    // doesn't matter", so there was no reliable way to know the flag
                    // wasn't firing on a healthy connection -- and the failure mode
                    // (silent corruption of real audio) was worse than the gap it was
                    // meant to smooth over.

                    // Fail closed: never hand non-Opus bytes to the decoder, because it
                    // renders them as noise instead of rejecting them. Silence is a far
                    // better failure mode than screeching at the user.
                    if !is_decodable_opus(decoder, &opus_packet, &pc_id, &mid) {
                        continue;
                    }

                    // Decode the Opus packet
                    // 20ms at 48kHz = 960 samples per channel
                    match decoder.decode(&opus_packet, 960) {
                        Ok(decoded_frame) => {
                            decoded_count = decoded_count.saturating_add(1);
                            // Logged on the very first frame per mid (so short test calls
                            // still produce evidence) and then every 20th -- includes peak
                            // amplitude so silent/garbage decodes are visible without
                            // having to listen to the audio to know something's wrong.
                            if decoded_count == 1 || decoded_count.is_multiple_of(20) {
                                let peak = decoded_frame
                                    .data
                                    .iter()
                                    .fold(0.0_f32, |acc, s| acc.max(s.abs()));
                                let clipped =
                                    decoded_frame.data.iter().filter(|s| s.abs() >= 1.0).count();
                                tracing::info!(
                                    pc_id,
                                    mid,
                                    count = decoded_count,
                                    opus_len = opus_packet.len(),
                                    decoded_samples = decoded_frame.data.len(),
                                    decoded_sample_rate = decoded_frame.sample_rate,
                                    decoded_channels = decoded_frame.channels,
                                    peak_amplitude = peak,
                                    clipped_samples = clipped,
                                    "Decoded inbound Opus audio frame"
                                );
                            }
                            // `{pc_id}-{mid}` uniquely identifies this remote track: one
                            // PeerConnection can carry several audio mids, and several
                            // PeerConnections share one output stream. The mixer needs
                            // this to play a track's own frames sequentially while
                            // summing distinct tracks -- see `AudioSink::play`.
                            let track_id = format!("{pc_id}-{mid}");
                            elementium_media::audio_debug_dump::maybe_dump(
                                &track_id,
                                decoded_frame.sample_rate,
                                decoded_frame.channels,
                                &decoded_frame.data,
                            );
                            player.play(&track_id, decoded_frame);
                        }
                        Err(e) => {
                            tracing::warn!(
                                pc_id,
                                mid,
                                opus_len = opus_packet.len(),
                                error = %e,
                                "Failed to decode inbound Opus frame, dropping"
                            );
                        }
                    }
                }
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    });
}
