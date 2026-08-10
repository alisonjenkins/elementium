// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod encoded_streams;
mod frontend_server;
mod protocols;
mod tray;

use std::sync::{Arc, Mutex};

use tauri::{WebviewUrl, WebviewWindowBuilder};
use tracing::warn;
use tracing_subscriber::EnvFilter;

use commands::e2ee::E2eeState;
use commands::livekit::LiveKitState;
use commands::media_devices::MediaState;
use commands::secrets::SecretStoreState;
use commands::webrtc::WebRtcState;
use elementium_keyring::{BackendType, SecretBackend, create_backend};
use elementium_types::CorrelationId;
use elementium_webrtc::WebRtcEngine;

/// The loopback port a release build serves the embedded frontend from.
///
/// A release build cannot load the frontend from Tauri's `tauri://localhost` origin,
/// because logging in is a redirect chain that has to come back to us: `WebKitGTK` refuses
/// to follow a redirect whose target is not HTTP(S), so the homeserver's hop back landed
/// on "Redirection to URL with a scheme that is not HTTP(S)" and the user could never
/// finish authenticating. The same origin also breaks OIDC before that point, because
/// dynamic client registration will not accept a `tauri:` redirect URI.
///
/// Fixed rather than ephemeral, and deliberately so: the origin keys localStorage,
/// `IndexedDB` and the crypto store, so a port that moved between launches would discard
/// the session and the device's own keys on every start.
const FRONTEND_PORT: u16 = 42871;

/// The port to serve the frontend on, overridable for the case where 42871 is taken.
///
/// Changing it logs the user out, for the reason given on [`FRONTEND_PORT`], so it is an
/// explicit opt-in rather than an automatic fallback to a free port.
fn frontend_port() -> u16 {
    std::env::var("ELEMENTIUM_HTTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(FRONTEND_PORT)
}

/// Build the JavaScript snippet that pre-populates localStorage with secrets
/// from the keyring before any page scripts run.
fn build_secrets_init_script(
    secrets: &std::collections::HashMap<String, String>,
    backend_type: BackendType,
) -> String {
    let needs_setup = backend_type == BackendType::NeedsSetup;

    if needs_setup {
        return "(function(){\
                window.__elementium_secrets_loaded=false;\
                window.__elementium_needs_secret_setup=true;\
                console.warn('[Elementium] No secret storage backend available — secrets stored in localStorage only');\
            })();"
            .to_string();
    }

    // Serialize secrets as JSON for injection
    let json = serde_json::to_string(secrets).unwrap_or_else(|_| "{}".to_string());

    format!(
        "(function(){{\
            var s={json};\
            for(var k in s)localStorage.setItem(k,s[k]);\
            window.__elementium_secrets_loaded=true;\
            window.__elementium_needs_secret_setup=false;\
        }})();"
    )
}

// There used to be a second console interceptor here, hand-duplicated in Rust as a Tauri
// `initialization_script` so it would run before any page script. It predated the frame tag
// that `console-bridge.ts` now stamps on every line (`[main]`/`[widget]`), and because a
// Tauri `initialization_script` only runs in the window's top document -- never in a nested
// iframe -- it patched `console.log` in the main frame alone, milliseconds before the real
// shim's own `setupConsoleBridge` ran there. `install()` in `install-report.ts` treats an
// already-patched `console.log` as "this shim is already in place" and does not re-wrap it,
// so the main frame was permanently stuck on this stale, untagged copy while the widget
// iframe -- never touched by this script -- ran the current one. That is the entire reason a
// live log showed 383 `[widget]` lines and zero `[main]` ones, and every widget-API line
// twice: once tagged, once not.
//
// Removed rather than kept in sync, because it served no purpose the JS shim does not: both
// use `__TAURI_INTERNALS__` directly and both are injected before Element Web's own scripts
// (the shim script tag is the first one `patch-element-web.sh` writes into `index.html`).
// One implementation, built by the one build that ships everything else, cannot itself drift
// out of sync with the frame it runs in -- which two hand-kept copies already had.

/// Load persisted secrets (if any backend is configured) for init-script injection.
fn load_initial_secrets(
    backend: Option<&SecretBackend>,
) -> std::collections::HashMap<String, String> {
    backend.map_or_else(std::collections::HashMap::new, |store| {
        store.get_all().unwrap_or_else(|e| {
            warn!("failed to load secrets from keyring: {e}");
            std::collections::HashMap::new()
        })
    })
}

/// Register all app-managed state (`WebRtcEngine`, media, `LiveKit`, secrets, E2EE).
fn register_state(
    builder: tauri::Builder<tauri::Wry>,
    backend: Option<SecretBackend>,
) -> tauri::Builder<tauri::Wry> {
    // The E2EE policy is shared between the WebRTC engine (for I/O loop
    // encryption/decryption) and Tauri's E2eeState (for JS commands).
    // This ensures that when e2ee_init/e2ee_set_key are called from JS,
    // the running I/O loops immediately pick up the context and keys.
    // Defaults to `ExplicitlyUnencrypted` until e2ee_init is called.
    let shared_e2ee: Arc<Mutex<elementium_webrtc::EncryptionPolicy>> =
        Arc::new(Mutex::new(elementium_webrtc::EncryptionPolicy::default()));

    let mut engine = WebRtcEngine::new();
    engine.e2ee = shared_e2ee.clone();
    let video_frames = engine.video_frames.clone();

    builder
        .manage(WebRtcState(Arc::new(Mutex::new(engine))))
        .manage(MediaState {
            active_tracks: Mutex::new(Vec::new()),
            pipelines: Mutex::new(std::collections::HashMap::new()),
            share: Mutex::new(None),
            sfu_media_tx: Mutex::new(None),
            session_correlation: Mutex::new(None),
        })
        .manage(protocols::VideoFrameState(video_frames.clone()))
        .manage(encoded_streams::EncodedStreams::new())
        .manage(LiveKitState {
            rooms: Arc::new(Mutex::new(std::collections::HashMap::new())),
            video_frames,
        })
        .manage(SecretStoreState {
            backend: Arc::new(Mutex::new(backend)),
        })
        .manage(E2eeState { ctx: shared_e2ee })
}

/// Register the IPC command handlers for every `#[tauri::command]` in `commands::*`.
fn register_commands(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder.invoke_handler(tauri::generate_handler![
        commands::webrtc::create_peer_connection,
        commands::webrtc::create_offer,
        commands::webrtc::create_answer,
        commands::webrtc::set_local_description,
        commands::webrtc::set_remote_description,
        commands::webrtc::add_ice_candidate,
        commands::webrtc::send_data_channel_message,
        commands::webrtc::get_transport_stats,
        commands::webrtc::restart_ice,
        commands::webrtc::close_peer_connection,
        commands::media_devices::enumerate_devices,
        commands::media_devices::get_user_media,
        commands::media_devices::get_video_frame,
        commands::media_devices::stop_track,
        commands::screen_capture::get_display_media,
        commands::screen_capture::get_capture_sources,
        commands::livekit::livekit_connect,
        commands::livekit::livekit_publish_track,
        commands::livekit::livekit_set_track_muted,
        commands::media_devices::set_capture_muted,
        commands::media_devices::set_video_bitrate,
        commands::media_devices::get_max_encode_fps,
        commands::media_devices::set_max_encode_fps,
        commands::media_devices::set_video_scale,
        commands::media_devices::get_capture_resolution,
        commands::media_devices::set_capture_resolution,
        commands::livekit::livekit_disconnect,
        commands::livekit::livekit_set_subscriber_volume,
        commands::secrets::secret_get,
        commands::secrets::secret_set,
        commands::secrets::secret_delete,
        commands::secrets::secret_get_all,
        commands::secrets::secret_get_backend_status,
        commands::secrets::secret_setup_file_backend,
        commands::e2ee::e2ee_init,
        commands::e2ee::e2ee_set_key,
        commands::e2ee::e2ee_set_local_identity,
        commands::e2ee::e2ee_set_sif_trailer,
        commands::console::console_log,
    ])
}

/// Why [`setup_app`] could not finish bringing up the window.
///
/// `tauri::Builder::setup`'s closure is declared by the framework to return
/// `Result<(), Box<dyn std::error::Error>>` -- not something this crate can change -- so
/// `main()` still has to hand it exactly that. What this type fixes is what `setup_app`
/// itself returns: three different fallible steps used to be flattened straight into that
/// `Box<dyn Error>` with a bare `?`, each one erasing which step failed and (via the `Box`)
/// its own source too. This names the step and keeps every source, and `main()` logs the
/// full chain immediately before the one, unavoidable conversion into `Box<dyn Error>` that
/// the framework's own signature requires.
#[derive(Debug)]
enum SetupError {
    /// Building the tray icon or its menu failed.
    Tray(tauri::Error),
    /// The embedded-frontend HTTP server (release builds only) could not start.
    FrontendServer(frontend_server::FrontendServerError),
    /// The main webview window could not be created.
    Window(tauri::Error),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tray(_) => write!(f, "could not create the tray icon"),
            Self::FrontendServer(_) => write!(f, "could not start the embedded frontend server"),
            Self::Window(_) => write!(f, "could not create the main window"),
        }
    }
}

impl std::error::Error for SetupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tray(e) | Self::Window(e) => Some(e),
            Self::FrontendServer(e) => Some(e),
        }
    }
}

/// Create the tray and main webview window during Tauri's `setup` hook.
fn setup_app(app: &tauri::App, init_script: &str) -> Result<(), SetupError> {
    // Before anything else touches it: a call can auto-join, or a device preview can
    // start, as soon as the window loads, and both read the frame-rate setting on the
    // capture thread via a plain atomic that this is the only thing that ever loads from
    // disk. See `load_persisted_max_encode_fps`.
    commands::media_devices::load_persisted_max_encode_fps(app);
    commands::media_devices::load_persisted_capture_resolution(app);

    tray::create_tray(app).map_err(SetupError::Tray)?;

    // Programmatic window creation with initialization_script for secret injection.
    //
    // Both URLs are loopback HTTP: the dev server in debug, and in release the localhost
    // plugin serving the embedded assets. Release must not use `WebviewUrl::App`, which
    // resolves to `tauri://localhost` and makes login impossible -- see `FRONTEND_PORT`.
    let url = if cfg!(debug_assertions) {
        "http://localhost:5173".to_owned()
    } else {
        let port = frontend_port();
        // Reported rather than ignored: without the server the window has nothing to load,
        // and a blank window with no explanation is the worst way to learn the port is
        // taken.
        frontend_server::spawn(&app.handle().clone(), port).map_err(SetupError::FrontendServer)?;
        format!("http://localhost:{port}")
    };
    // Both forms are `http://localhost:<u16>`, so the parse cannot fail in practice.
    #[allow(clippy::unwrap_used)]
    let url = WebviewUrl::External(url.parse().unwrap());

    let win = WebviewWindowBuilder::new(app, "main", url)
        .title("Elementium")
        .inner_size(1280.0, 800.0)
        .min_inner_size(800.0, 600.0)
        .resizable(true)
        .fullscreen(false)
        .initialization_script(init_script)
        .build()
        .map_err(SetupError::Window)?;

    let _ = win.eval("console.log('[Elementium] Native WebRTC backend active');");

    Ok(())
}

/// Send structured logs to a file as well as stderr, returning the guard that must be
/// held for the process's lifetime.
///
/// Until now the only sink was stderr, which means a fault reported after the fact — the
/// normal case, since the user is in a call when it happens — left nothing to read. Every
/// diagnosis had to start by asking them to reproduce it under a terminal. The file is
/// JSON, one event per line, so it can be filtered with `jq` rather than by eye.
///
/// Returns `None` if the log directory cannot be created, in which case stderr logging
/// still works: no log file is a degraded state, not a reason to refuse to start.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    // `RUST_LOG` (standard `tracing-subscriber::EnvFilter` syntax, e.g.
    // `RUST_LOG=elementium_webrtc=debug`) overrides the default level, per-crate/module, with
    // no rebuild -- see `docs/observability.md`. Default (no `RUST_LOG` set) is `info` for
    // everything, which is deliberately below the per-packet `debug!` instrumentation in
    // `elementium-webrtc`'s I/O loop (`UDP received`, `str0m event`): one 53-second call
    // produced 6,156 lines of those two alone at `info`, most of a 30MB log file that told a
    // reader nothing a healthy call did not already tell them. Set `RUST_LOG` to get them back.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let dir = dirs::data_dir().map(|d| d.join("io.github.elementium").join("logs"));
    let writer = dir.as_ref().and_then(|dir| {
        std::fs::create_dir_all(dir)
            .map_err(|e| eprintln!("could not create log directory {}: {e}", dir.display()))
            .ok()?;
        Some(tracing_appender::non_blocking(
            tracing_appender::rolling::daily(dir, "elementium.log"),
        ))
    });

    let stderr = tracing_subscriber::fmt::layer().json();

    let (file_layer, guard) = match writer {
        Some((file_writer, guard)) => (
            Some(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(file_writer),
            ),
            Some(guard),
        ),
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file_layer)
        .init();

    if guard.is_some()
        && let Some(dir) = dir
    {
        tracing::info!(log_dir = %dir.display(), "logging to file");
    }

    // Said unconditionally, not folded into the line above, because that one only fires when
    // the log directory could be created -- the override is worth knowing about either way.
    tracing::info!(
        default_level = "info",
        override_env = "RUST_LOG",
        "RUST_LOG (e.g. RUST_LOG=elementium_webrtc=debug) raises verbosity above the default; \
         see docs/observability.md"
    );

    guard
}

/// Log which Element Web build the webview is about to load.
///
/// A bug report that does not say what was running costs a round trip to find out, and
/// "the latest" is not an answer once a version can be pinned, patched, or built from a
/// fork. Written by `scripts/patch-element-web.sh`; see `specs/007-element-web-upgrade`.
///
/// Absence is logged rather than ignored: a missing record means the patch script did not
/// finish, which is worth knowing before the first call fails instead of after.
fn log_element_web_build() {
    // Dev runs from the repository root; a bundled app carries the frontend beside the
    // executable. Neither is guaranteed, so both are tried and the miss is reported.
    let candidates = [
        std::path::PathBuf::from("element-web-dist/.elementium-build.json"),
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_default()
            .join("element-web-dist/.elementium-build.json"),
    ];

    for path in &candidates {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(record) => {
                let field = |key: &str| record.get(key).cloned().unwrap_or(serde_json::Value::Null);
                tracing::info!(
                    element_web_version = %field("elementWebVersion"),
                    source = %field("source"),
                    built_at = %field("builtAt"),
                    element_call_fingerprint = %field("elementCallFingerprint"),
                    autojoin_injected = %field("autojoinInjected"),
                    patches = %field("patches"),
                    // What each frame's own console-bridge line (`shim installed,
                    // build=...`) should say. Printed once here so a mismatch between this
                    // and a frame's own line -- or between the two frames' lines -- is a
                    // one-glance diff instead of a mystery.
                    shim_fingerprint = %field("shimFingerprint"),
                    "Element Web build record"
                );
            }
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "Element Web build record is present but unreadable"
            ),
        }
        return;
    }

    tracing::warn!(
        "no Element Web build record found; scripts/patch-element-web.sh may not have run"
    );
}

fn main() -> tauri::Result<()> {
    // Held for the whole of main: dropping it flushes and stops the writer thread.
    let _log_guard = init_logging();

    // Root correlation ID for the whole process lifetime. Every event emitted before a
    // call/session-scoped span is entered (e.g. device enumeration, startup) inherits this
    // instead of being logged uncorrelated; call/session spans layer their own more specific
    // correlation_id field on top once a call starts.
    let app_instance_id = CorrelationId::new();
    let _app_span =
        tracing::info_span!("app_instance", correlation_id = %app_instance_id).entered();

    log_element_web_build();

    // Initialize secret storage backend
    let backend = create_backend();
    let initial_secrets = load_initial_secrets(backend.as_ref());
    let backend_type = backend
        .as_ref()
        .map_or(BackendType::NeedsSetup, SecretBackend::kind);
    let secrets_script = build_secrets_init_script(&initial_secrets, backend_type);
    let init_script = secrets_script;

    let mut builder = tauri::Builder::default();
    builder = register_state(builder, backend);
    builder = builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init());
    builder = register_commands(builder);
    builder = builder.register_asynchronous_uri_scheme_protocol(
        "elementium",
        protocols::handle_video_frame_protocol,
    );
    builder = builder.setup(move |app| {
        setup_app(app, &init_script).map_err(|e| {
            let chain = commands::error_chain(&e);
            tracing::error!(chain = %chain, "app setup failed; the window will not appear");
            let boxed: Box<dyn std::error::Error> = Box::new(e);
            boxed
        })
    });

    // Both `large_stack_frames` and `usage of process::exit` fire on this line because
    // they originate inside the expansion of `tauri::generate_context!()` (framework
    // codegen), not in our code.
    #[allow(clippy::large_stack_frames, clippy::exit)]
    let mut context = tauri::generate_context!();

    // Tell Tauri where the app is actually served from.
    //
    // `frontendDist` in `tauri.conf.json` has to stay a directory, because that is what
    // makes the assets get embedded in the binary. But it is also what Tauri compares a
    // page's URL against to decide whether that page is the app or something it navigated
    // to, and a directory means "the `tauri://` origin". Left alone, our own frontend --
    // the same embedded assets, handed out over loopback -- counts as *remote*, and Tauri
    // withholds IPC from remote pages unless every command is named in a capability. The
    // symptom is total and silent: the shims install, `invoke` throws, and nothing is
    // logged, because the console bridge reports over the IPC that is being refused.
    //
    // Pointing `frontend_dist` at the URL we serve on states the truth of the arrangement
    // and restores the local-origin behaviour, without widening what a genuinely remote
    // page -- an SSO provider we redirect to -- is allowed to do. The assets are a
    // separate part of the context and stay embedded.
    if !cfg!(debug_assertions) {
        match format!("http://localhost:{}", frontend_port()).parse() {
            Ok(url) => {
                context.config_mut().build.frontend_dist =
                    Some(tauri::utils::config::FrontendDist::Url(url));
            }
            // Cannot happen for `http://localhost:<u16>`; if it somehow does, the app runs
            // without IPC, which is worth a line in the log rather than a panic.
            Err(e) => warn!("could not parse the frontend URL, so IPC will be refused: {e}"),
        }
    }

    builder.run(context)
}
