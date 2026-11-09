use std::sync::Arc;

use tauri::Manager;

pub mod appdb;
pub mod commands;
pub mod driver;
pub mod edit;
pub mod error;
pub mod export;
#[cfg(target_os = "macos")]
mod macos_quit;
pub mod filter;
pub mod legacy;
pub mod menu;
pub mod model;
pub mod mongoquery;
pub mod query;
pub mod secrets;
pub mod session;
pub mod sqltext;
pub mod windows;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .menu(menu::build)
        .on_menu_event(|app, event| {
            if event.id() == menu::QUIT_ID {
                // Each window asks about its open transactions and unsaved edits in turn.
                windows::start_quit(app);
            } else if event.id() == menu::NEW_WINDOW_ID {
                windows::open_in_background(app);
            }
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                let app = window.app_handle().clone();
                let label = window.label().to_string();
                // A quit waiting on this window moves on without it.
                windows::act(&app, app.state::<windows::QuitSequence>().window_gone(&label));
                // Taken now, before ⇧⌘N can reuse the label and connect: a cleanup that
                // ran later could otherwise remove the new window's session.
                let driver = app.state::<session::Sessions>().take(&label);
                app.state::<export::ExportJobs>().cancel_window(&label);
                app.state::<windows::QuitPrompts>().forget(&label);
                tauri::async_runtime::spawn(async move {
                    app.state::<query::sessions::QuerySessions>().close_window(&label).await;
                    if let Some(driver) = driver {
                        driver.close().await;
                    }
                });
            }
        })
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            if let Some(parent) = dir.parent() {
                let legacy_dir = parent.join(legacy::LEGACY_IDENTIFIER);
                // Stops the launch: opening the database now would create an empty one,
                // and the copy would never run again. The next launch retries.
                legacy::migrate_legacy_data(&dir, &legacy_dir).map_err(|e| {
                    format!("could not copy data from the previous version: {e}; your old data is untouched")
                })?;
            }
            let db = tauri::async_runtime::block_on(appdb::AppDb::open(&dir.join("app.db")))?;
            let secrets: Arc<dyn secrets::SecretStore> = Arc::new(secrets::KeychainStore);
            app.manage(db);
            app.manage(secrets);
            app.manage(session::Sessions::default());
            app.manage(query::sessions::QuerySessions::default());
            app.manage(export::ExportJobs::default());
            app.manage(windows::QuitSequence::default());
            app.manage(windows::QuitPrompts::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_connections,
            commands::save_connection,
            commands::delete_connection,
            commands::test_connection,
            commands::pick_database_file,
            commands::connect,
            commands::disconnect,
            commands::schema_tree,
            commands::table_structure,
            commands::fetch_page,
            commands::cancel_page,
            commands::preview_changes,
            commands::apply_changes,
            commands::analyze_sql,
            commands::format_mongo,
            commands::list_saved_queries,
            commands::save_query,
            commands::delete_saved_query,
            commands::list_history,
            commands::load_tab_state,
            commands::save_tab_state,
            commands::release_tab_state,
            commands::run_query,
            commands::cancel_query,
            commands::end_transaction,
            commands::close_query_session,
            commands::list_databases,
            commands::target_schemas,
            commands::target_columns,
            commands::collection_fields,
            commands::export_table,
            commands::export_rows,
            commands::export_query,
            commands::cancel_export,
            commands::reveal_export,
            windows::open_window,
            windows::quit_continue,
            windows::quit_cancel,
            windows::report_quit_prompt,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            // Dock ▸ Quit, logout and restart ask before quitting, as ⌘Q does.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Ready => {
                let app = app.clone();
                macos_quit::install(move || {
                    if windows::quit_needs_no_prompt(&app) {
                        return macos_quit::Reply::Now;
                    }
                    macos_quit::bring_app_to_front();
                    windows::start_quit(&app);
                    macos_quit::Reply::Cancel
                });
            }
            // Closing the last window leaves the app running on macOS; ⌘Q still quits.
            tauri::RunEvent::ExitRequested { code, api, .. } if windows::keeps_running(code) => api.prevent_exit(),
            // A Dock click with no window open opens one.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { has_visible_windows: false, .. } if app.webview_windows().is_empty() => {
                windows::open_in_background(app)
            }
            _ => {}
        });
}
