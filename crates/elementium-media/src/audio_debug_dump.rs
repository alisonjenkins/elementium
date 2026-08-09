//! Opt-in raw-PCM capture of decoded audio.
//!
//! For diagnosing bugs that only manifest in the actual waveform shape (e.g. aliasing,
//! discontinuities, wrong pitch) -- scalar telemetry like peak amplitude or clipped-sample
//! counts cannot distinguish "clean audio" from "digital screeching" from a log line; both
//! can have unremarkable amplitude stats. This writes the exact samples handed to playback
//! to disk so they can be inspected/played back directly instead of guessed at from
//! aggregate numbers.
//!
//! Disabled unless the `ELEMENTIUM_AUDIO_DUMP` environment variable is set (to any value),
//! so it costs nothing in normal operation.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::{Mutex, OnceLock};

/// Cap per dumped stream: 30s at 48kHz stereo f32, so a long-running call can't fill the
/// disk from a debug feature nobody remembered was on.
const MAX_SAMPLES_PER_STREAM: usize = 48_000 * 2 * 30;

struct DumpState {
    writer: BufWriter<File>,
    samples_written: usize,
    capped_logged: bool,
}

/// Whether audio capture-to-disk is switched on.
///
/// Exposed so callers can skip setting up work that exists only to feed a dump -- e.g. the
/// capture path builds a throwaway Opus decoder to record what its own encoder produces,
/// which must not happen on a normal run.
#[must_use]
pub fn is_enabled() -> bool {
    enabled()
}

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        // Marker file, checked in addition to the env var, so capture can be turned on
        // for a remotely-launched process without needing to touch its launch
        // environment -- `touch /tmp/ELEMENTIUM_AUDIO_DUMP` before the next repro.
        std::env::var_os("ELEMENTIUM_AUDIO_DUMP").is_some()
            || std::path::Path::new("/tmp/ELEMENTIUM_AUDIO_DUMP").exists()
    })
}

/// Create a dump file only this user can read, refusing to follow an existing path.
///
/// This file holds decoded call audio -- someone's voice -- at a name anyone can predict,
/// in a directory everyone can write to. `File::create` gave it mode 0644 and happily
/// followed a symlink someone else had planted, which is both a disclosure and a way to
/// have this process overwrite a file it should not. The camera preview dump next door
/// already writes 0600 for the same reason; this now matches it.
///
/// `create_new` means a stale dump from a previous run is an error rather than something to
/// clobber, and it is what makes the symlink case fail instead of succeed.
fn create_private(path: &str) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn registry() -> &'static Mutex<HashMap<String, DumpState>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, DumpState>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Append `samples` (interleaved f32, as decoded) to this stream's dump file.
///
/// No-op unless `ELEMENTIUM_AUDIO_DUMP` is set. `stream_key` should uniquely identify the
/// track (e.g. `"{pc_id}-{mid}"`) so concurrent tracks land in separate files. Writes raw
/// little-endian f32 samples with no container/header -- load with an explicit format
/// spec, e.g. `ffplay -f f32le -ar 48000 -ac 2 /tmp/elementium_audio_dump_<key>.f32le` or
/// `numpy.fromfile(path, dtype='<f4')`.
pub fn maybe_dump(stream_key: &str, sample_rate: u32, channels: u16, samples: &[f32]) {
    if !enabled() {
        return;
    }

    let Ok(mut reg) = registry().lock() else {
        return;
    };

    let state = match reg.entry(stream_key.to_string()) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            let path = format!("/tmp/elementium_audio_dump_{stream_key}.f32le");
            match create_private(&path) {
                Ok(file) => {
                    tracing::info!(
                        stream_key,
                        path,
                        sample_rate,
                        channels,
                        "ELEMENTIUM_AUDIO_DUMP: capturing decoded audio to raw f32le file"
                    );
                    v.insert(DumpState {
                        writer: BufWriter::new(file),
                        samples_written: 0,
                        capped_logged: false,
                    })
                }
                Err(e) => {
                    tracing::error!(
                        stream_key,
                        path,
                        error = %e,
                        "ELEMENTIUM_AUDIO_DUMP: failed to create dump file (it must not \
                         already exist -- remove the previous one to record again)"
                    );
                    return;
                }
            }
        }
    };

    if state.samples_written >= MAX_SAMPLES_PER_STREAM {
        if !state.capped_logged {
            state.capped_logged = true;
            tracing::info!(
                stream_key,
                "ELEMENTIUM_AUDIO_DUMP: capture cap reached, no longer writing this stream"
            );
        }
        return;
    }

    let remaining = MAX_SAMPLES_PER_STREAM.saturating_sub(state.samples_written);
    let to_write = samples.len().min(remaining);
    for sample in samples.iter().take(to_write) {
        let _ = state.writer.write_all(&sample.to_le_bytes());
    }
    state.samples_written = state.samples_written.saturating_add(to_write);
}
