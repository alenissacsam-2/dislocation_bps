//! The application's shared state, and the commands the window can call.

use crate::{
    archive, config, history, logs,
    paths::Paths,
    runner::{BotRunner, NativeRunner, RunState, BOT_PORT},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub struct App {
    pub paths: Paths,
    pub runner: Arc<dyn BotRunner>,
    /// Whether to restart the bot when it is found dead.
    ///
    /// Off by default and deliberately so: an instrument that silently resurrects
    /// itself hides exactly the failures this application exists to make visible, and
    /// what it starts is a new run rather than a continuation of the old one.
    pub auto_restart: Arc<AtomicBool>,
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        let paths = Paths::discover();
        let runner =
            Arc::new(NativeRunner::new(paths.bot_exe(), paths.root.clone(), paths.log()));
        Self { paths, runner, auto_restart: Arc::new(AtomicBool::new(false)) }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub fn bot_status(app: tauri::State<'_, App>) -> serde_json::Value {
    let state = app.runner.probe();
    serde_json::json!({
        "state": state,
        "port": BOT_PORT,
        "botExe": app.paths.bot_exe(),
        "botExePresent": app.paths.bot_exe().exists(),
        "root": app.paths.root,
        "ledger": app.paths.ledger(),
        "ledgerPresent": app.paths.ledger().exists(),
    })
}

/// # Errors
/// If the binary is missing, the port is held by another instrument, or the spawn
/// fails. The message is shown to the operator verbatim.
#[tauri::command]
pub fn bot_start(app: tauri::State<'_, App>) -> Result<(), String> {
    app.runner.start().map_err(|e| e.to_string())
}

/// # Errors
/// If the child cannot be reaped.
#[tauri::command]
pub fn bot_stop(app: tauri::State<'_, App>) -> Result<(), String> {
    app.runner.stop().map_err(|e| e.to_string())
}

/// # Errors
/// If `config.toml` cannot be read or parsed.
#[tauri::command]
pub fn read_config(app: tauri::State<'_, App>) -> Result<config::Params, String> {
    config::read_params(&app.paths.config()).map_err(|e| e.to_string())
}

/// Write the parameters, archive the run they no longer describe, and optionally
/// restart into a clean one.
///
/// HANDOVER §7: rows recorded under different parameters aggregate by different rules
/// with nothing downstream showing the mixture, so a parameter change ends the run.
///
/// The outcomes are reported separately rather than as one boolean. "Saved but failed
/// to restart" is a different situation from "never saved", and collapsing them leaves
/// the operator guessing which happened.
///
/// # Errors
/// If validation fails or the config cannot be written. Validation failure leaves both
/// the file and the run untouched.
#[tauri::command]
pub fn save_config(
    app: tauri::State<'_, App>,
    params: config::Params,
    restart: bool,
) -> Result<serde_json::Value, String> {
    config::validate(&params)?;
    let was_running = matches!(app.runner.probe(), RunState::Running | RunState::Starting);
    if was_running {
        app.runner.stop().map_err(|e| e.to_string())?;
    }
    config::write_params(&app.paths.config(), &params).map_err(|e| e.to_string())?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let archived = archive::archive_ledger(&app.paths.ledger(), &app.paths.archive_dir(), &stamp)
        .map_err(|e| e.to_string())?;
    let mut restarted = false;
    let mut restart_error = None;
    if restart && was_running {
        match app.runner.start() {
            Ok(()) => restarted = true,
            Err(e) => restart_error = Some(e.to_string()),
        }
    }
    Ok(serde_json::json!({
        "saved": true,
        "archived": archived.map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
        "wasRunning": was_running,
        "restarted": restarted,
        "restartError": restart_error,
    }))
}

#[tauri::command]
pub fn read_history(app: tauri::State<'_, App>) -> serde_json::Value {
    history::snapshot(&app.paths.ledger())
}

#[tauri::command]
pub fn read_history_at(path: String) -> serde_json::Value {
    history::snapshot(std::path::Path::new(&path))
}

#[tauri::command]
pub fn read_archives(app: tauri::State<'_, App>) -> Vec<archive::ArchivedRun> {
    archive::list_archives(&app.paths.archive_dir())
}

#[tauri::command]
pub fn read_log(app: tauri::State<'_, App>, lines: usize) -> Vec<String> {
    logs::tail(&app.paths.log(), lines)
}

#[tauri::command]
pub fn get_auto_restart(app: tauri::State<'_, App>) -> bool {
    app.auto_restart.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn set_auto_restart(app: tauri::State<'_, App>, on: bool) {
    app.auto_restart.store(on, Ordering::Relaxed);
}

/// # Errors
/// If the platform's autostart registration cannot be read.
#[tauri::command]
pub fn get_autostart(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// # Errors
/// If the platform's autostart registration cannot be written.
#[tauri::command]
pub fn set_autostart(app: tauri::AppHandle, on: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let al = app.autolaunch();
    if on { al.enable() } else { al.disable() }.map_err(|e| e.to_string())
}
