use tauri::command;

/// Receive JS console messages and log them on the Rust side.
/// This lets us see all browser console output (including from iframes)
/// in the cargo run terminal.
#[command]
pub async fn console_log(level: String, args: Vec<String>) {
    let msg = args.join(" ");
    match level.as_str() {
        "error" => tracing::error!(target: "js_console", "{msg}"),
        "warn" => tracing::warn!(target: "js_console", "{msg}"),
        "debug" => tracing::debug!(target: "js_console", "{msg}"),
        _ => tracing::info!(target: "js_console", "{msg}"),
    }
}
