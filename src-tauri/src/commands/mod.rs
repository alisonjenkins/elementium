pub mod console;
pub mod e2ee;
pub mod livekit;
pub mod media_devices;
pub mod screen_capture;
pub mod secrets;
pub mod webrtc;

/// Lock a `std::sync::Mutex`, mapping poisoning to a `String` error the way every
/// `#[tauri::command]` in this module needs (IPC results must be `Result<T, String>`).
///
/// Deliberately does not recover poisoned locks (unlike `elementium_webrtc`'s
/// `peer_connection::lock_pc`): a poisoned command-layer mutex means a previous command
/// panicked mid-mutation of shared state, which is worth surfacing as an error to the
/// frontend rather than silently continuing with possibly-inconsistent state.
pub trait LockExt<T> {
    fn lock_str(&self) -> Result<std::sync::MutexGuard<'_, T>, String>;
}

impl<T> LockExt<T> for std::sync::Mutex<T> {
    fn lock_str(&self) -> Result<std::sync::MutexGuard<'_, T>, String> {
        self.lock().map_err(|e| e.to_string())
    }
}
