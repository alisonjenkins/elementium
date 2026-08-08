pub mod share;
pub mod traits;

#[cfg(target_os = "linux")]
pub mod wayland;
#[cfg(target_os = "linux")]
pub mod x11;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

pub use share::{ShareBackend, ShareError, ShareSession, ShareSource, start_share, start_x11_share};
pub use traits::ScreenCapturer;
