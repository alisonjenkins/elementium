// `get_display_media` below takes a `State<'_, T>` parameter, which causes the
// `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use serde::Serialize;
use tauri::{State, command};
use tracing::Instrument;

use elementium_types::CaptureSource;

use super::media_devices::{
    MediaState, ShareHandle, start_screen_share_audio_pipeline, start_screen_share_pipeline,
};
use super::webrtc::WebRtcState;
use super::{IpcErr, LockExt};

/// What starting a share produced.
///
/// An object rather than the bare track id it used to be, because the audio outcome is not
/// derivable by the caller: whether a share ended up carrying one application's audio or
/// the whole desktop mix is decided here, and the difference is one the user has to be told
/// about rather than one they can infer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayMediaResult {
    video_track_id: String,
    audio_track_id: Option<String>,
    audio_scope: Option<&'static str>,
    /// True when application audio was asked for and the desktop mix was captured instead.
    audio_scope_fallback: bool,
}

#[command]
pub async fn get_capture_sources() -> Result<Vec<CaptureSource>, String> {
    tracing::info!("Getting available capture sources");

    #[cfg(target_os = "linux")]
    {
        use elementium_screen::ScreenCapturer;

        // X11 first, because it can enumerate: it knows what monitors and windows exist and
        // can name them. Where it cannot, the portal takes over, and an *empty* list is how
        // that is signalled -- see the note on the empty case below.
        let capturer = elementium_screen::x11::X11Capturer::new();
        match capturer.sources() {
            Ok(sources) if !sources.is_empty() => Ok(sources),
            Ok(_) | Err(_) => {
                // Empty is not "no sources exist". On Wayland the compositor will not tell
                // an application what windows are open -- by design -- so any list we built
                // would be empty or a lie. The empty list means "ask the portal", which is
                // what `get_display_media` does when given no source id.
                tracing::info!(
                    "no enumerable capture sources; selection is delegated to the system picker"
                );
                Ok(Vec::new())
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(Vec::new())
    }
}

/// Start sharing a screen, window or application.
///
/// `source_id` is what makes an X11 share possible at all: X11 has no compositor picker, so
/// the id chosen from [`get_capture_sources`] is the only way to say what to capture, and its
/// absence there is a refusal (see [`elementium_screen::start_x11_share`]), not a fallback.
/// On Wayland it is ignored -- the portal's own picker decides -- which is why the parameter
/// is optional rather than defaulted: `None` means different things on the two backends, and
/// this command is what decides which backend a given call reaches (see below).
///
/// # Errors
///
/// Returns the `picker_cancelled` sentinel when the user dismissed the picker, and a
/// specific description otherwise. The two must stay distinguishable: one is a person
/// declining, the other is something to investigate.
#[command]
pub async fn get_display_media(
    webrtc_state: State<'_, WebRtcState>,
    media_state: State<'_, MediaState>,
    source_id: Option<String>,
    audio: bool,
) -> Result<DisplayMediaResult, String> {
    // The call this share belongs to, so the portal exchange can be read alongside the rest
    // of the call rather than as an unattached island. The pipeline this eventually starts
    // already labels itself this way; without it here, the steps *before* a pipeline exists
    // -- which is where a share that never gets a picker fails -- belonged to no call at all.
    //
    // Carried as a span applied to the future with `Instrument`, never as an entered guard:
    // a guard held across an `.await` is attached to whichever thread resumes it, and this
    // function awaits a dialog for as long as a person takes to read one.
    let call_id = media_state
        .session_correlation
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default();
    let span = tracing::info_span!("share", correlation_id = %call_id);

    get_display_media_inner(webrtc_state, media_state, source_id, audio)
        .instrument(span)
        .await
}

/// Flatten a portal share failure for the IPC, logging it once -- at `warn` for the picker
/// being dismissed (an expected, recoverable outcome: a person declined to share, not a
/// fault) and at `error` for everything else (a genuine failure to reach or use the portal).
///
/// `start_x11_share`'s errors go through the plain [`IpcErr::ipc_err`] instead: the X11 path
/// has no picker of its own (see the module doc on `source_id`), so [`elementium_screen::ShareError::Cancelled`]
/// can never reach it and every variant it can produce is a real fault.
fn log_share_err(error: &elementium_screen::ShareError) -> String {
    match error {
        elementium_screen::ShareError::Cancelled => {
            tracing::warn!(
                command = "get_display_media",
                "screen-share picker dismissed without a selection"
            );
        }
        other => {
            let chain = super::error_chain(other);
            tracing::error!(command = "get_display_media", chain = %chain, "command failed");
        }
    }
    error.to_string()
}

async fn get_display_media_inner(
    webrtc_state: State<'_, WebRtcState>,
    media_state: State<'_, MediaState>,
    source_id: Option<String>,
    audio: bool,
) -> Result<DisplayMediaResult, String> {
    tracing::info!(
        source_id = source_id.as_deref().unwrap_or("<portal picker>"),
        audio_requested = audio,
        "starting a screen share"
    );

    // Which backend to use is decided here, explicitly, by whether a source id was given --
    // not left for `elementium-screen` to infer from the platform. X11 has no picker of its
    // own, so a source id (chosen from `get_capture_sources`) *is* what asks for the X11
    // path; without one there is nothing for X11 to open, and the request falls through to
    // the portal's own picker, exactly as it did before X11 existed here.
    //
    // The portal branch is awaited on the caller's runtime rather than run on a nested one:
    // it shows a dialog and takes as long as a person takes to decide. The X11 branch has no
    // dialog and nothing to await.
    let session = if let Some(id) = source_id.as_deref() {
        elementium_screen::start_x11_share(Some(id)).ipc_err("get_display_media", id)?
    } else {
        elementium_screen::start_share()
            .await
            .map_err(|e| log_share_err(&e))?
    };

    let video_frames = {
        let engine = webrtc_state.0.lock_str("get_display_media")?;
        engine.video_frames.clone()
    };

    let track_id = start_screen_share_pipeline(&media_state, &video_frames, &session)?;

    // Audio is a separate PipeWire stream from the video the portal just granted -- the
    // screencast portal carries no audio in any backend, so this connects directly to the
    // desktop audio graph instead. Gated here rather than merely left uncalled: FR-008/SC-005
    // require that a user who did not opt in gets no new input stream at all, and the only way
    // to guarantee that is to never reach the code that would open one.
    let (audio_track_id, audio_scope, audio_scope_fallback) = if audio {
        start_share_audio(&media_state)
    } else {
        (None, None, false)
    };

    // Extracted before `session` moves into the replaced `ShareHandle` below: whichever
    // variant this share is, name it precisely enough for the log line to say what was
    // actually opened, not just that something was.
    let share_source_desc = match session.source() {
        elementium_screen::ShareSource::Wayland { node_id } => format!("pipewire node {node_id}"),
        elementium_screen::ShareSource::X11 { source_id } => format!("x11 source {source_id}"),
    };
    let source_kind = session.source_kind();

    // One share at a time. Replacing rather than layering: two portal sessions racing for
    // one video track is not a state any of this is built to be in, and the user's most
    // recent choice is the one they meant.
    //
    // The displaced share is taken out from under the lock and closed afterwards, because
    // closing it is an await and holding a std::sync guard across one makes the whole
    // command's future non-Send.
    let displaced = media_state.share.lock().ok().and_then(|mut slot| {
        slot.replace(ShareHandle {
            track_id: track_id.clone(),
            session,
        })
    });
    if let Some(previous) = displaced {
        tracing::info!(
            replaced_track = %previous.track_id,
            "a second share replaced the one already running"
        );
        previous.session.close().await;
    }

    tracing::info!(
        source = %share_source_desc,
        source_kind = ?source_kind,
        track_id = %track_id,
        "screen share started"
    );

    Ok(DisplayMediaResult {
        video_track_id: track_id,
        audio_track_id,
        audio_scope,
        audio_scope_fallback,
    })
}

/// Pick a share-audio source and start capturing it, returning the track id, the scope
/// that was actually captured, and whether that scope is a fallback from what was asked
/// for.
///
/// Only ever returns the desktop-mix fallback today. Real per-application scoping would
/// need to correlate the window the portal granted to the `PipeWire` node carrying that
/// application's audio, and `ShareSession` has nothing to correlate with -- the portal
/// reports whether a monitor or a window was chosen (see `source_kind`) but never a PID,
/// and `list_audio_sources` only has one to offer on native `PipeWire` clients, never on
/// ALSA-compatibility streams (research R8 / tasks.md T038). Writing an application-scoping
/// branch that can never be taken would be dead code, not a feature; the honest
/// implementation is the fallback, taken unconditionally and disclosed via the returned
/// `audio_scope_fallback`.
///
/// A source-enumeration or pipeline-start failure degrades to "no audio" rather than
/// failing the whole share: the user asked to see the screen shared, and a `PipeWire`
/// hiccup on the audio side should not take that down with it.
fn start_share_audio(media_state: &MediaState) -> (Option<String>, Option<&'static str>, bool) {
    use elementium_media::pipewire_audio::AudioSourceKind;
    use elementium_media::pipewire_nodes::{AudioSourceClass, list_audio_sources};

    let sink = match list_audio_sources() {
        Ok(sources) => sources.into_iter().find(|s| s.class == AudioSourceClass::Sink),
        Err(e) => {
            tracing::warn!(reason = %e, "could not enumerate PipeWire audio sources; sharing video only");
            return (None, None, false);
        }
    };
    let Some(sink) = sink else {
        tracing::warn!("no desktop audio sink found to capture; sharing video only");
        return (None, None, false);
    };

    match start_screen_share_audio_pipeline(media_state, sink.node_id, AudioSourceKind::SinkMonitor)
    {
        Ok(audio_track_id) => {
            tracing::info!(
                node_id = sink.node_id,
                track_id = %audio_track_id,
                "share audio started, scoped to the desktop mix"
            );
            (Some(audio_track_id), Some("desktop_mix"), true)
        }
        Err(e) => {
            tracing::warn!(reason = %e, node_id = sink.node_id, "could not start share audio pipeline; sharing video only");
            (None, None, false)
        }
    }
}
