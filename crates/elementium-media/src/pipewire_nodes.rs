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

    let sources = found.lock().map(|g| g.clone()).unwrap_or_default();
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
fn video_source_from_global(global: &pipewire::registry::GlobalObject<&pipewire::spa::utils::dict::DictRef>) -> Option<PipewireVideoSource> {
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

#[cfg(test)]
mod tests {
    use super::is_video_source_class;

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
}
