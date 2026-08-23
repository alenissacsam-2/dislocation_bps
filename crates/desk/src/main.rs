//! cryptobot-desk — the application that owns the instrument.
//!
//! `windows_subsystem = "windows"` in release so launching it does not flash a console
//! window; debug keeps the console, because that is where a panic is readable.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use cb_desk::{app, runner::RunState};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager, WindowEvent,
};

/// Two icons rather than one, because the point of a tray presence is that the run
/// state is legible without opening anything.
const ICON_LIVE: &[u8] = include_bytes!("../icons/tray-live.png");
const ICON_IDLE: &[u8] = include_bytes!("../icons/tray-idle.png");

/// How often the watcher re-probes. Slow enough to cost nothing, fast enough that a
/// death shows in the tray before you would have noticed it any other way.
const WATCH_SECS: u64 = 4;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(app::App::new())
        .invoke_handler(tauri::generate_handler![
            app::bot_status,
            app::bot_start,
            app::bot_stop,
            app::read_config,
            app::save_config,
            app::read_history,
            app::read_history_at,
            app::read_archives,
            app::read_log,
            app::get_auto_restart,
            app::set_auto_restart,
            app::get_autostart,
            app::set_autostart,
        ])
        .setup(|tauri_app| {
            build_tray(tauri_app)?;
            spawn_watcher(tauri_app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing hides to tray. The whole reason this app exists is to keep
            // watching after you stop looking at it, so the close button must not be
            // the thing that stops the watching.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("cryptobot-desk failed to start");
}

fn build_tray(tauri_app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(tauri_app, "show", "Show window", true, None::<&str>)?;
    let start = MenuItem::with_id(tauri_app, "start", "Start instrument", true, None::<&str>)?;
    let stop = MenuItem::with_id(tauri_app, "stop", "Stop instrument", true, None::<&str>)?;
    let quit = MenuItem::with_id(tauri_app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        tauri_app,
        &[
            &show,
            &PredefinedMenuItem::separator(tauri_app)?,
            &start,
            &stop,
            &PredefinedMenuItem::separator(tauri_app)?,
            &quit,
        ],
    )?;

    TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(ICON_IDLE)?)
        .tooltip("cryptobot — checking")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|handle, event| {
            let state = handle.state::<app::App>();
            match event.id.as_ref() {
                "show" => {
                    if let Some(w) = handle.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
                "start" => {
                    let _ = state.runner.start();
                }
                "stop" => {
                    let _ = state.runner.stop();
                }
                "quit" => {
                    // Stop the child before going, or quitting the window leaves an
                    // orphan holding port 8787 that nothing can then reclaim.
                    let _ = state.runner.stop();
                    handle.exit(0);
                }
                _ => {}
            }
        })
        .build(tauri_app)?;
    Ok(())
}

/// Watches the child and reflects its state into the tray.
///
/// Also performs the opt-in restart. `Failed` specifically, never `Stopped`: a run the
/// operator stopped deliberately must stay stopped, and only a process that exited on
/// its own is a candidate for resurrection.
fn spawn_watcher(handle: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            std::thread::sleep(Duration::from_secs(WATCH_SECS));
            let state = handle.state::<app::App>();
            let now = state.runner.probe();

            if last != Some(now) {
                if let Some(tray) = handle.tray_by_id("main") {
                    let live = matches!(now, RunState::Running | RunState::Starting);
                    if let Ok(icon) = Image::from_bytes(if live { ICON_LIVE } else { ICON_IDLE }) {
                        let _ = tray.set_icon(Some(icon));
                    }
                    let _ = tray.set_tooltip(Some(match now {
                        RunState::Running => "cryptobot — running",
                        RunState::Starting => "cryptobot — starting",
                        RunState::Stopped => "cryptobot — stopped",
                        RunState::Foreign => "cryptobot — port held by another process",
                        RunState::Failed => "cryptobot — died unexpectedly",
                    }));
                }
                last = Some(now);
            }

            if now == RunState::Failed && state.auto_restart.load(Ordering::Relaxed) {
                let _ = state.runner.start();
            }
        }
    });
}
