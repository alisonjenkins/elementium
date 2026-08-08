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

use super::media_devices::{MediaState, ShareHandle, start_screen_share_pipeline};
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

    // Audio is deliberately not started yet: capturing the sound of a shared application is
    // a separate PipeWire stream with its own lifetime (the screencast portal offers no
    // audio at all, in any backend), and it is sequenced after the video path is provably
    // working. Requesting it today is honoured by capturing nothing rather than by
    // capturing the desktop mix, because sending audio a user did not get a chance to
    // review is worse than sending none.
    if audio {
        tracing::warn!("share audio was requested but is not implemented yet; sharing video only");
    }

    let track_id = start_screen_share_pipeline(&media_state, &video_frames, &session)?;

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
        audio_track_id: None,
        audio_scope: None,
        audio_scope_fallback: false,
    })
}
