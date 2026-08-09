use tauri::{
    App, AppHandle, Manager,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

/// Build the tray icon and its menu.
///
/// # Errors
///
/// Returns whatever `tauri::Error` the menu-item, menu, or tray-icon builder produced --
/// every fallible step here is already typed by `tauri` itself, so there is nothing to erase
/// behind `Box<dyn Error>` (which this used to do, in violation of Principle I): the concrete
/// type already names its cause and is propagated unchanged.
pub fn create_tray(app: &App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Elementium", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::new()
        .tooltip("Elementium")
        .menu(&menu)
        .on_menu_event(|app: &AppHandle, event| match event.id().as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}
