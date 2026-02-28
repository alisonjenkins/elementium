// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod protocols;
mod tray;

use std::sync::{Arc, Mutex};

use tauri::Manager;
use tracing_subscriber::EnvFilter;

use commands::livekit::LiveKitState;
use commands::media_devices::MediaState;
use commands::webrtc::WebRtcState;
use elementium_webrtc::WebRtcEngine;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

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
    ]);

    builder = builder.register_asynchronous_uri_scheme_protocol(
        "elementium",
        protocols::handle_video_frame_protocol,
    );

    builder = builder.setup(|app| {
        tray::create_tray(app)?;

        if let Some(webview) = app.get_webview_window("main") {
            let _ = webview
                .eval("console.log('[Elementium] Native WebRTC backend active');");
        }

        Ok(())
    });

    builder
        .run(tauri::generate_context!())
        .expect("error while running Elementium");
}
