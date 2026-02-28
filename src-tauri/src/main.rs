// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod protocols;
mod tray;

use std::sync::{Arc, Mutex};

use tauri::{WebviewUrl, WebviewWindowBuilder};
use tracing::warn;
use tracing_subscriber::EnvFilter;

use commands::livekit::LiveKitState;
use commands::media_devices::MediaState;
use commands::secrets::SecretStoreState;
use commands::webrtc::WebRtcState;
use elementium_keyring::{BackendType, create_backend};
use elementium_webrtc::WebRtcEngine;

/// Build the JavaScript snippet that pre-populates localStorage with secrets
/// from the keyring before any page scripts run.
fn build_secrets_init_script(
    secrets: &std::collections::HashMap<String, String>,
    backend_type: BackendType,
) -> String {
    let needs_setup = backend_type == BackendType::NeedsSetup;

    if needs_setup {
        return format!(
            "(function(){{\
                window.__elementium_secrets_loaded=false;\
                window.__elementium_needs_secret_setup=true;\
                console.warn('[Elementium] No secret storage backend available — secrets stored in localStorage only');\
            }})();"
        );
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

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Initialize secret storage backend
    let (backend, backend_type) = create_backend();

    // Load secrets for init script injection
    let initial_secrets = match &backend {
        Some(store) => store.get_all().unwrap_or_else(|e| {
            warn!("failed to load secrets from keyring: {e}");
            std::collections::HashMap::new()
        }),
        None => std::collections::HashMap::new(),
    };

    let init_script = build_secrets_init_script(&initial_secrets, backend_type);

    let mut builder = tauri::Builder::default();

    // Register shared state
    let engine = WebRtcEngine::new();
    let video_frames = engine.video_frames.clone();

    builder = builder
        .manage(WebRtcState(Arc::new(Mutex::new(engine))))
        .manage(MediaState {
            active_tracks: Mutex::new(Vec::new()),
        })
        .manage(LiveKitState {
            rooms: Arc::new(Mutex::new(std::collections::HashMap::new())),
            video_frames,
        })
        .manage(SecretStoreState {
            store: Arc::new(Mutex::new(backend)),
            backend_type: Arc::new(Mutex::new(backend_type)),
        });

    builder = builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init());

    builder = builder.invoke_handler(tauri::generate_handler![
        commands::webrtc::create_peer_connection,
        commands::webrtc::create_offer,
        commands::webrtc::create_answer,
        commands::webrtc::set_local_description,
        commands::webrtc::set_remote_description,
        commands::webrtc::add_ice_candidate,
        commands::webrtc::close_peer_connection,
        commands::media_devices::enumerate_devices,
        commands::media_devices::get_user_media,
        commands::media_devices::stop_track,
        commands::screen_capture::get_display_media,
        commands::screen_capture::get_capture_sources,
        commands::livekit::livekit_connect,
        commands::livekit::livekit_publish_track,
        commands::livekit::livekit_disconnect,
        commands::livekit::livekit_set_subscriber_volume,
        commands::secrets::secret_get,
        commands::secrets::secret_set,
        commands::secrets::secret_delete,
        commands::secrets::secret_get_all,
        commands::secrets::secret_get_backend_status,
        commands::secrets::secret_setup_file_backend,
    ]);

    builder = builder.register_asynchronous_uri_scheme_protocol(
        "elementium",
        protocols::handle_video_frame_protocol,
    );

    builder = builder.setup(move |app| {
        tray::create_tray(app)?;

        // Programmatic window creation with initialization_script for secret injection
        let url = if cfg!(debug_assertions) {
            WebviewUrl::External("http://localhost:5173".parse().unwrap())
        } else {
            WebviewUrl::App("index.html".into())
        };

        let win = WebviewWindowBuilder::new(app, "main", url)
            .title("Elementium")
            .inner_size(1280.0, 800.0)
            .min_inner_size(800.0, 600.0)
            .resizable(true)
            .fullscreen(false)
            .initialization_script(&init_script)
            .build()?;

        let _ = win.eval("console.log('[Elementium] Native WebRTC backend active');");

        Ok(())
    });

    builder
        .run(tauri::generate_context!())
        .expect("error while running Elementium");
}
