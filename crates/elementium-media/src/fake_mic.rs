//! A microphone that plays a file, so a test can transmit a signal it already knows.
//!
//! Every audio measurement this project has is of the form "did something arrive". None of
//! them can say whether what arrived is what was sent, and the fault they were written for --
//! "the mic audio is bad" -- reads healthy on all of them. Answering the real question needs a
//! *known* signal going in, and Elementium captures a real device, so until now there was no
//! way to supply one.
//!
//! Chromium has had this switch for years (`--use-file-for-fake-audio-capture`) and the
//! browser participants in our own call tests already use it. This is the same thing for the
//! native capture path: set `ELEMENTIUM_FAKE_MIC` to a 16-bit PCM WAV and no input device is
//! opened at all -- the file is played, on a loop, in real time, into the same channel a
//! device would have fed.
//!
//! # Why not a virtual device instead
//!
//! It was tried first, and measured. A `PulseAudio` null sink playing the signal, with the
//! session's default source pointed at its monitor, did retarget ALSA's `default` -- verified
//! by capturing through it -- and did *not* retarget Elementium, which opens the device it was
//! asked for by id rather than the default. The run that established this recorded a
//! `capture-raw` peak of 0.0006 on one channel and exactly 0.0 on the other: a hardware
//! microphone in a quiet room, not the monitor of a sink with a tone playing into it.
//!
//! The approach was abandoned for a better reason than that, though. It reconfigures the
//! developer's sound server -- their microphone and their speakers -- for the length of a
//! test, and restores it afterwards only if the run exits cleanly. That is not a thing a test
//! should do to the machine it runs on, and it makes the test unrunnable anywhere without a
//! sound server. A file source needs neither.
//!
//! # What this does and does not prove
//!
//! It bypasses `cpal` and the device driver: it cannot catch a fault in opening a device, in
//! device selection, or in a driver's own resampling. Everything after the device is real --
//! the same channel, the same resampling, the same automatic gain control, the same framing,
//! the same encoder -- which is where the audio path can damage a signal, and where a test can
//! usefully hold it to account.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use elementium_types::AudioFrame;
use thiserror::Error;

/// The environment variable that switches this on, holding a path to a WAV file.
const ENV_VAR: &str = "ELEMENTIUM_FAKE_MIC";

/// How much audio is delivered per callback. One Opus frame, as a real device is asked for.
const FRAME_MS: u64 = 20;

#[derive(Error, Debug)]
pub enum FakeMicError {
    #[error("could not read the fake microphone file: {0}")]
    Read(#[source] std::io::Error),
    /// Deliberately not a fallback to the real microphone. A test that asked for a known
    /// signal and silently got a room instead would report on the room.
    #[error("the fake microphone file is not a 16-bit PCM WAV ({0})")]
    Format(&'static str),
}

/// A decoded WAV: interleaved f32, and the rate and channel count to play it at.
pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved f32 samples, as a capture callback would deliver them.
    pub samples: Vec<f32>,
}

/// Little-endian `u32` at `at`, or `None` if the buffer is too short.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    <[u8; 4]>::try_from(bytes.get(at..end)?).ok().map(u32::from_le_bytes)
}

/// Little-endian `u16` at `at`, or `None` if the buffer is too short.
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    <[u8; 2]>::try_from(bytes.get(at..end)?).ok().map(u16::from_le_bytes)
}

/// Decode a 16-bit PCM WAV.
///
/// Chunks are walked rather than assumed to be at fixed offsets: a WAV written by ffmpeg
/// carries a `LIST` chunk before `data`, and reading the samples from byte 44 of one of those
/// produces metadata interpreted as audio -- which is a burst of noise at the start of every
/// measurement, in a test whose whole subject is whether the audio is intact.
///
/// # Errors
///
/// Returns [`FakeMicError::Format`] if this is not a WAV, or not 16-bit PCM.
pub fn parse_wav(bytes: &[u8]) -> Result<Wav, FakeMicError> {
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err(FakeMicError::Format("no RIFF/WAVE header"));
    }

    let mut at: usize = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;

    while let Some(id_end) = at.checked_add(4) {
        let Some(id) = bytes.get(at..id_end) else { break };
        let Some(size) = u32_at(bytes, id_end) else { break };
        let Some(body) = id_end.checked_add(4) else { break };
        let size = usize::try_from(size).unwrap_or(0);
        let Some(end) = body.checked_add(size) else { break };
        let Some(chunk) = bytes.get(body..end) else { break };

        if id == b"fmt " {
            format = Some((
                u16_at(chunk, 0).unwrap_or(0),
                u16_at(chunk, 2).unwrap_or(0),
                u32_at(chunk, 4).unwrap_or(0),
                u16_at(chunk, 14).unwrap_or(0),
            ));
        } else if id == b"data" {
            data = Some(chunk);
        }

        // Chunks are padded to an even length; the pad byte is not part of the chunk.
        at = end.checked_add(usize::from(size & 1 == 1)).unwrap_or(end);
    }

    let Some((tag, channels, sample_rate, bits)) = format else {
        return Err(FakeMicError::Format("no fmt chunk"));
    };
    if tag != 1 || bits != 16 {
        return Err(FakeMicError::Format("not uncompressed 16-bit PCM"));
    }
    if channels == 0 || sample_rate == 0 {
        return Err(FakeMicError::Format("zero channels or sample rate"));
    }
    let Some(data) = data else {
        return Err(FakeMicError::Format("no data chunk"));
    };

    let samples: Vec<f32> = data
        .chunks_exact(2)
        .filter_map(|pair| <[u8; 2]>::try_from(pair).ok())
        .map(|pair| f32::from(i16::from_le_bytes(pair)) / 32768.0)
        .collect();
    if samples.is_empty() {
        return Err(FakeMicError::Format("the data chunk is empty"));
    }

    Ok(Wav { sample_rate, channels, samples })
}

/// The path this run should play as its microphone, if any.
#[must_use]
pub fn requested() -> Option<std::path::PathBuf> {
    std::env::var_os(ENV_VAR).map(std::path::PathBuf::from)
}

/// A running fake microphone. Dropping it stops the thread.
pub struct FakeMic {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for FakeMic {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            // The thread wakes at least every frame, so this is bounded by one frame.
            drop(handle.join());
        }
    }
}

/// What [`start`] produces: the same three things `cpal` would have given the caller.
pub struct Started {
    pub mic: FakeMic,
    pub receiver: mpsc::Receiver<AudioFrame>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Start playing `path` as a microphone, looping it for as long as the capturer lives.
///
/// Frames are emitted against a fixed schedule from a single start instant rather than by
/// sleeping 20ms per iteration: the second accumulates drift, and drift in a source whose
/// purpose is to let a test measure gaps in milliseconds would be indistinguishable from the
/// fault being looked for.
///
/// The loop point is a seam in the signal, so it is placed on a frame boundary and the file is
/// expected to hold a whole number of cycles of whatever it carries.
///
/// # Errors
///
/// Returns [`FakeMicError`] if the file cannot be read or is not a 16-bit PCM WAV.
pub fn start(path: &std::path::Path) -> Result<Started, FakeMicError> {
    let bytes = std::fs::read(path).map_err(FakeMicError::Read)?;
    let wav = parse_wav(&bytes)?;
    let sample_rate = wav.sample_rate;
    let channels = wav.channels;

    // One 20ms frame of interleaved samples.
    let per_frame = usize::try_from(u64::from(sample_rate).saturating_mul(FRAME_MS) / 1000)
        .unwrap_or(960)
        .saturating_mul(usize::from(channels))
        .max(1);

    tracing::warn!(
        path = %path.display(),
        sample_rate,
        channels,
        total_samples = wav.samples.len(),
        frame_samples = per_frame,
        "ELEMENTIUM_FAKE_MIC: no microphone will be opened; this file is being played as \
         capture instead"
    );

    let (tx, rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let stop = Arc::clone(&running);

    let thread = thread::spawn(move || {
        let started = Instant::now();
        let mut frames: u64 = 0;
        let mut at: usize = 0;
        while stop.load(Ordering::Relaxed) {
            // Wrap at the end of the file rather than at the end of a frame, so the loop
            // never emits a short frame -- a short frame is a gap the encoder would have to
            // paper over, and gaps are what this exists to measure.
            let mut data: Vec<f32> = Vec::with_capacity(per_frame);
            while data.len() < per_frame {
                if at >= wav.samples.len() {
                    at = 0;
                }
                let want = per_frame.saturating_sub(data.len());
                let end = at.saturating_add(want).min(wav.samples.len());
                match wav.samples.get(at..end) {
                    Some(slice) if !slice.is_empty() => {
                        data.extend_from_slice(slice);
                        at = end;
                    }
                    _ => break,
                }
            }

            let timestamp_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            if tx
                .send(AudioFrame { sample_rate, channels, data, timestamp_us })
                .is_err()
            {
                break;
            }

            frames = frames.saturating_add(1);
            let due = Duration::from_millis(frames.saturating_mul(FRAME_MS));
            let elapsed = started.elapsed();
            if let Some(remaining) = due.checked_sub(elapsed) {
                thread::sleep(remaining);
            }
        }
    });

    Ok(Started {
        mic: FakeMic { running, thread: Some(thread) },
        receiver: rx,
        sample_rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 16-bit PCM WAV, optionally with a junk chunk before `data`.
    fn wav(samples: &[i16], channels: u16, rate: u32, with_list_chunk: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let list: Vec<u8> = if with_list_chunk {
            let body = b"INFOhello!!!".to_vec();
            let mut c = b"LIST".to_vec();
            c.extend_from_slice(&u32::try_from(body.len()).unwrap_or(0).to_le_bytes());
            c.extend_from_slice(&body);
            c
        } else {
            Vec::new()
        };
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(&list);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn a_plain_wav_decodes_to_the_samples_it_holds() {
        let parsed = parse_wav(&wav(&[0, 16384, -16384], 1, 48_000, false)).ok();
        assert!(parsed.is_some(), "a well-formed WAV must parse");
        if let Some(parsed) = parsed {
            assert_eq!(parsed.sample_rate, 48_000);
            assert_eq!(parsed.channels, 1);
            assert_eq!(parsed.samples, vec![0.0, 0.5, -0.5]);
        }
    }

    /// Reading the samples from a fixed offset instead of walking the chunks would take a
    /// `LIST` chunk's text as audio -- a burst of noise at the start of every measurement, in
    /// a test whose subject is whether the audio is intact.
    #[test]
    fn a_wav_with_a_metadata_chunk_before_the_data_still_decodes_to_the_samples() {
        let parsed = parse_wav(&wav(&[0, 16384, -16384], 2, 44_100, true)).ok();
        assert!(parsed.is_some(), "a WAV with a LIST chunk must parse");
        if let Some(parsed) = parsed {
            assert_eq!(parsed.channels, 2);
            assert_eq!(parsed.sample_rate, 44_100);
            assert_eq!(parsed.samples, vec![0.0, 0.5, -0.5]);
        }
    }

    /// Never fall back to the real microphone: a test that asked for a known signal and
    /// silently got a room instead reports on the room.
    #[test]
    fn anything_that_is_not_a_16_bit_pcm_wav_is_an_error_rather_than_a_fallback() {
        assert!(parse_wav(b"not a wav at all").is_err());
        let mut truncated = wav(&[1, 2, 3], 1, 48_000, false);
        truncated.truncate(20);
        assert!(parse_wav(&truncated).is_err());
    }
}
