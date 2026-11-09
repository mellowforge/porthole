use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};

/// Id of the custom quit item; `lib.rs` starts the quit sequence (`windows::start_quit`).
pub const QUIT_ID: &str = "quit";
/// Id of File ▸ New Window; `lib.rs` opens a window (`windows::open`).
pub const NEW_WINDOW_ID: &str = "new-window";
/// The macOS app menu. Quit is a custom item so the UI can ask about open
/// transactions first, and there is no Close Window item, so ⌘W stays with the page.
pub fn build<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<Menu<R>> {
    let quit = MenuItemBuilder::with_id(QUIT_ID, "Quit Porthole")
        .accelerator("CmdOrCtrl+Q")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, "Porthole")
        .about(None)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .item(&quit)
        .build()?;
    let new_window = MenuItemBuilder::with_id(NEW_WINDOW_ID, "New Window")
        .accelerator("CmdOrCtrl+Shift+N")
        .build(app)?;
    let file = SubmenuBuilder::new(app, "File").item(&new_window).build()?;
    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    // Tauri makes the submenu with this id the macOS Window menu, which then lists the open
    // windows by title (`{connection} · {database}`).
    let window = SubmenuBuilder::with_id(app, tauri::menu::WINDOW_SUBMENU_ID, "Window")
        .item(&PredefinedMenuItem::minimize(app, None)?)
        .item(&PredefinedMenuItem::maximize(app, Some("Zoom"))?)
        .build()?;
    MenuBuilder::new(app).items(&[&app_menu, &file, &edit, &window]).build()
}
