//! Enumerate `PipeWire` video sources.
//!
//! On a modern Linux desktop `PipeWire` owns the camera: it opens the `V4L2` device and
//! hands frames out to clients. A process that tries to open `/dev/videoN` itself gets
//! `EBUSY`, which is exactly what happens here -- `/dev/video3` is held by the `pipewire`
//! daemon, so every direct capture attempt fails with "Device or resource busy" while the
//! camera is in perfect working order.
//!
//! `V4L2` enumeration is also misleading in a way that matters: a UVC camera exposes a
//! second device node for metadata, which enumerates as a camera but has no usable format.
//! `PipeWire` only advertises real sources, so the list it gives is the list a user would
//! recognise.
//!
//! This module also enumerates audio sources suitable for *share audio* -- the sound of a
//! shared screen or application, as distinct from the microphone. Two `media.class` values
//! matter: `Audio/Sink`, a device sink whose monitor carries the whole desktop mix, and
//! `Stream/Output/Audio`, one application's own playback. Confirmed against a real `pw-dump`
//! on this machine (see [`AudioSourceClass`]) rather than assumed from documentation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A video source `PipeWire` is offering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipewireVideoSource {
    /// Global id, used to connect a stream to this node.
    pub node_id: u32,
    /// Stable machine name, e.g. `v4l2_input.pci-0000_15_00.0-usb-0_1.1_1.0`.
    pub name: String,
    /// Human-readable name, e.g. `OBSBOT Tiny 2 Lite (V4L2)`.
    pub description: String,
    /// Backing device path when the source is a `V4L2` device, for correlating with the
    /// `V4L2` enumeration this replaces.
    pub device_path: Option<String>,
}

/// Errors from talking to `PipeWire`.
#[derive(Debug, thiserror::Error)]
pub enum PipewireError {
    #[error("PipeWire initialisation failed: {0}")]
    Init(String),
    #[error("PipeWire connection failed: {0}")]
    Connect(String),
    /// Enumeration ran but its result cannot be trusted.
    ///
    /// Distinct from an empty list, which is a legitimate answer meaning the machine has no
    /// such device. This says we do not know, and a caller that shows "no devices" for it
    /// would be reporting a fact it does not have.
    #[error("PipeWire enumeration failed: {0}")]
    Enumerate(String),
}

/// How long to let the registry settle before returning what it announced.
///
/// `PipeWire` announces existing globals asynchronously after a client connects, with no
/// "that's all of them" event, so enumeration is inherently "collect for a moment". Short
/// enough not to be felt in a device picker, long enough for a local daemon to reply.
const ENUMERATION_SETTLE: Duration = Duration::from_millis(300);

/// List every video source `PipeWire` currently offers.
///
/// # Errors
///
/// Returns [`PipewireError`] if the `PipeWire` library cannot be initialised or no
/// connection to the daemon can be made (e.g. it is not running).
pub fn list_video_sources() -> Result<Vec<PipewireVideoSource>, PipewireError> {
    pipewire::init();

    let mainloop = pipewire::main_loop::MainLoopRc::new(None)
        .map_err(|e| PipewireError::Init(e.to_string()))?;
    let context = pipewire::context::ContextRc::new(&mainloop, None)
        .map_err(|e| PipewireError::Init(e.to_string()))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| PipewireError::Connect(e.to_string()))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| PipewireError::Connect(e.to_string()))?;

    let found: Arc<Mutex<Vec<PipewireVideoSource>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&found);

    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(source) = video_source_from_global(global)
                && let Ok(mut list) = sink.lock()
            {
                list.push(source);
            }
        })
        .register();

    // Run the loop briefly rather than blocking forever: there is no completion event.
    let timer_loop = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        timer_loop.quit();
    });
    let _ = timer.update_timer(Some(ENUMERATION_SETTLE), None);
    mainloop.run();

    // A poisoned lock is reported, not folded into "no sources". The two are opposite
    // situations that reach the same caller: an empty list means the machine has no camera,
    // which a UI shows as "no devices"; a poisoned lock means the enumeration thread panicked
    // and we know nothing, which a UI would then show as "no devices" too. One of those is
    // a fact about the machine and the other is a bug in here.
    let sources = match found.lock() {
        Ok(list) => list.clone(),
        Err(_) => {
            return Err(PipewireError::Enumerate(
                "the enumeration thread panicked; the video source list is unknown".to_owned(),
            ));
        }
    };
    tracing::info!(count = sources.len(), "PipeWire video sources enumerated");
    for s in &sources {
        tracing::info!(
            node_id = s.node_id,
            name = %s.name,
            description = %s.description,
            device_path = s.device_path.as_deref().unwrap_or("-"),
            "PipeWire video source"
        );
    }
    Ok(sources)
}

/// Interpret a registry global as a video source, or `None` if it is something else.
///
/// Split out and given its own properties type so the filtering rule is testable without a
/// running `PipeWire` daemon.
fn video_source_from_global(
    global: &pipewire::registry::GlobalObject<&pipewire::spa::utils::dict::DictRef>,
) -> Option<PipewireVideoSource> {
    let props = global.props?;
    let media_class = props.get("media.class")?;
    if !is_video_source_class(media_class) {
        return None;
    }
    Some(PipewireVideoSource {
        node_id: global.id,
        name: props.get("node.name").unwrap_or_default().to_owned(),
        description: props
            .get("node.description")
            .or_else(|| props.get("node.nick"))
            .unwrap_or_default()
            .to_owned(),
        device_path: props
            .get("api.v4l2.path")
            .or_else(|| props.get("device.path"))
            .map(str::to_owned),
    })
}

/// Whether a `media.class` names something that produces video frames.
///
/// `Video/Source` is a camera. `Video/Source/Virtual` covers screen-capture and virtual
/// cameras, which are equally valid sources -- the OBS virtual camera on this machine is
/// one. Sinks and audio classes are not.
#[must_use]
pub fn is_video_source_class(media_class: &str) -> bool {
    media_class == "Video/Source" || media_class.starts_with("Video/Source/")
}

/// A `PipeWire` audio node offered as a possible source of *share audio*: the sound of a
/// shared screen or application, captured separately from the microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipewireAudioSource {
    /// Global id, used to connect a stream to this node.
    pub node_id: u32,
    /// Stable machine name, e.g. `alsa_output.pci-0000_00_1f.3.analog-stereo` or `Zen`.
    pub name: String,
    /// Human-readable name, when the node advertises one. Application streams commonly do
    /// not -- `node.description` was empty for every `Stream/Output/Audio` node observed on
    /// this machine, leaving `name` (the browser's own `node.name`, e.g. `Zen`) as the only
    /// human-legible label.
    pub description: String,
    /// Which relationship this node has to the audio graph, and so how it must be captured.
    pub class: AudioSourceClass,
    /// The PID of the process that owns this node, when `PipeWire` reports one.
    ///
    /// Only ever present on application streams, never on a device sink -- a sink is not
    /// owned by any one process. This is the candidate handle for correlating a shared
    /// window (which the XDG portal identifies, but never with audio) to the application
    /// whose audio should follow it; nothing consumes it yet, but capturing it here is what
    /// makes that matching possible later.
    pub application_pid: Option<u32>,
}

/// Which relationship a `PipeWire` audio node has to the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSourceClass {
    /// `Audio/Sink`: a device sink. Not itself readable -- capturing it means capturing its
    /// *monitor*, which mirrors the whole mix being played to it.
    Sink,
    /// `Stream/Output/Audio`: one running application's own playback.
    AppStream,
}

impl AudioSourceClass {
    /// Map a `media.class` string onto a share-audio source category, or `None` if it names
    /// something else -- a microphone (`Audio/Source`), a capture-side stream
    /// (`Stream/Input/Audio`, which is what something *recording* looks like, not what is
    /// playing), or an unrelated class entirely.
    #[must_use]
    pub fn from_media_class(media_class: &str) -> Option<Self> {
        match media_class {
            "Audio/Sink" => Some(Self::Sink),
            "Stream/Output/Audio" => Some(Self::AppStream),
            _ => None,
        }
    }
}

/// List every `PipeWire` audio node suitable as a share-audio source: device sinks (captured
/// via their monitor) and individual application playback streams.
///
/// # Errors
///
/// Returns [`PipewireError`] if the `PipeWire` library cannot be initialised or no
/// connection to the daemon can be made.
pub fn list_audio_sources() -> Result<Vec<PipewireAudioSource>, PipewireError> {
    pipewire::init();

    let mainloop = pipewire::main_loop::MainLoopRc::new(None)
        .map_err(|e| PipewireError::Init(e.to_string()))?;
    let context = pipewire::context::ContextRc::new(&mainloop, None)
        .map_err(|e| PipewireError::Init(e.to_string()))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| PipewireError::Connect(e.to_string()))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| PipewireError::Connect(e.to_string()))?;

    let found: Arc<Mutex<Vec<PipewireAudioSource>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&found);

    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(source) = audio_source_from_global(global)
                && let Ok(mut list) = sink.lock()
            {
                list.push(source);
            }
        })
        .register();

    // Same settle-and-collect approach as video enumeration: globals are announced
    // asynchronously with no completion event.
    let timer_loop = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        timer_loop.quit();
    });
    let _ = timer.update_timer(Some(ENUMERATION_SETTLE), None);
    mainloop.run();

    // Same reasoning as the video path above: "we could not find out" is not "there is
    // nothing".
    let sources = match found.lock() {
        Ok(list) => list.clone(),
        Err(_) => {
            return Err(PipewireError::Enumerate(
                "the enumeration thread panicked; the audio source list is unknown".to_owned(),
            ));
        }
    };
    tracing::info!(count = sources.len(), "PipeWire audio sources enumerated");
    for s in &sources {
        tracing::info!(
            node_id = s.node_id,
            name = %s.name,
            description = %s.description,
            class = ?s.class,
            application_pid = s.application_pid,
            "PipeWire audio source"
        );
    }
    Ok(sources)
}

/// Interpret a registry global as a share-audio source, or `None` if it is something else.
fn audio_source_from_global(
    global: &pipewire::registry::GlobalObject<&pipewire::spa::utils::dict::DictRef>,
) -> Option<PipewireAudioSource> {
    let props = global.props?;
    let class = AudioSourceClass::from_media_class(props.get("media.class")?)?;
    Some(PipewireAudioSource {
        node_id: global.id,
        name: props.get("node.name").unwrap_or_default().to_owned(),
        description: props
            .get("node.description")
            .or_else(|| props.get("node.nick"))
            .unwrap_or_default()
            .to_owned(),
        class,
        application_pid: props
            .get("application.process.id")
            .and_then(|pid| pid.parse().ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::{AudioSourceClass, is_video_source_class};

    #[test]
    fn recognises_camera_and_virtual_video_sources() {
        assert!(is_video_source_class("Video/Source"));
        assert!(is_video_source_class("Video/Source/Virtual"));
    }

    #[test]
    fn rejects_sinks_and_audio() {
        assert!(!is_video_source_class("Video/Sink"));
        assert!(!is_video_source_class("Audio/Source"));
        assert!(!is_video_source_class("Audio/Sink"));
        assert!(!is_video_source_class("Stream/Output/Audio"));
        assert!(!is_video_source_class(""));
    }

    /// `Video/Source` must not match by prefix alone, or `Video/SourceSomething` would be
    /// accepted as a source class it is not.
    #[test]
    fn requires_a_separator_before_a_subclass() {
        assert!(!is_video_source_class("Video/SourceLike"));
    }

    /// The two classes confirmed against a real `pw-dump` on this machine: `Audio/Sink`
    /// nodes (e.g. `alsa_output...analog-stereo`) and `Stream/Output/Audio` nodes for
    /// individual applications (e.g. a browser tab named `Zen`, or `OBS: Game Audio`).
    #[test]
    fn recognises_sinks_and_app_streams() {
        assert_eq!(
            AudioSourceClass::from_media_class("Audio/Sink"),
            Some(AudioSourceClass::Sink)
        );
        assert_eq!(
            AudioSourceClass::from_media_class("Stream/Output/Audio"),
            Some(AudioSourceClass::AppStream)
        );
    }

    /// The microphone and anything actively *recording* must not be offered as a share-audio
    /// source: `Audio/Source` is a microphone, and `Stream/Input/Audio` is something
    /// consuming audio (as `pw-dump` showed OBS's own capture streams doing), not producing
    /// it.
    #[test]
    fn rejects_microphones_and_recording_streams() {
        assert_eq!(AudioSourceClass::from_media_class("Audio/Source"), None);
        assert_eq!(
            AudioSourceClass::from_media_class("Stream/Input/Audio"),
            None
        );
        assert_eq!(AudioSourceClass::from_media_class("Video/Source"), None);
        assert_eq!(AudioSourceClass::from_media_class(""), None);
    }
}
