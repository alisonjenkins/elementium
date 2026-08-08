// `get_display_media` below takes a `State<'_, T>` parameter, which causes the
// `#[command]` macro to generate a sibling IPC-dispatch wrapper item in this module
// containing an internal match with an arm clippy flags as unreachable. That wrapper is
// framework codegen (not nested inside the fn item itself, so a function- or
// statement-scoped `#[allow]` cannot reach it — verified empirically), hence the
// module-level allow here rather than the usual per-item scoping.
#![allow(clippy::unreachable)]
use serde::Serialize;
use tauri::{State, command};

use elementium_types::CaptureSource;

use super::media_devices::{
    MediaState, ShareHandle, start_screen_share_audio_pipeline, start_screen_share_pipeline,
};
use super::webrtc::WebRtcState;

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
/// `source_id` is honoured where the platform lets an application choose; where selection
/// belongs to the compositor it is ignored and the portal's picker decides, which is why it
/// is optional rather than defaulted.
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
    tracing::info!(
        source_id = source_id.as_deref().unwrap_or("<portal picker>"),
        audio_requested = audio,
        "starting a screen share"
    );

    // Awaited on the caller's runtime rather than run on a nested one. The portal exchange
    // shows a dialog and takes as long as a person takes to decide.
    let session = elementium_screen::start_share()
        .await
        .map_err(|e| e.to_string())?;

    let video_frames = {
        let engine = webrtc_state.0.lock().map_err(|_| "engine lock poisoned")?;
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

    let node_id = session.node_id();
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
        node_id,
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
