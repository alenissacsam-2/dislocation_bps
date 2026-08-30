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
    /// The signing key, when the operator has unlocked it this session. Never written
    /// anywhere from here, and dropped when the process exits.
    pub custody: crate::wallet::Custody,
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
        // An installed copy starts with an empty data directory. Seeding it here rather
        // than on first read means the config editor and the bot both find a file, and
        // neither has to special-case its absence.
        if let Err(e) = paths.ensure_ready() {
            eprintln!("could not prepare {}: {e}", paths.root.display());
        }
        let runner =
            Arc::new(NativeRunner::new(paths.bot_exe(), paths.root.clone(), paths.log()));
        Self {
            paths,
            runner,
            custody: crate::wallet::Custody::default(),
            auto_restart: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// Probes the child and the port. Async because both touch the OS, and a command that
/// touches the OS on the main thread stalls the window — see [`read_history`].
///
/// # Errors
/// If the blocking task panics or is cancelled.
#[tauri::command]
pub async fn bot_status(app: tauri::State<'_, App>) -> Result<serde_json::Value, String> {
    let runner = Arc::clone(&app.runner);
    let paths = app.paths.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = runner.probe();
        let exe = paths.bot_exe();
        serde_json::json!({
            "state": state,
            "port": BOT_PORT,
            "botExePresent": exe.exists(),
            "botExe": exe,
            "root": paths.root,
            "ledger": paths.ledger(),
            "ledgerPresent": paths.ledger().exists(),
        })
    })
    .await
    .map_err(|e| e.to_string())
}

/// # Errors
/// If the binary is missing, the port is held by another instrument, or the spawn
/// fails. The message is shown to the operator verbatim.
#[tauri::command]
pub async fn bot_start(app: tauri::State<'_, App>) -> Result<(), String> {
    let runner = Arc::clone(&app.runner);
    tauri::async_runtime::spawn_blocking(move || runner.start().map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

/// # Errors
/// If the child cannot be reaped.
#[tauri::command]
pub async fn bot_stop(app: tauri::State<'_, App>) -> Result<(), String> {
    let runner = Arc::clone(&app.runner);
    tauri::async_runtime::spawn_blocking(move || runner.stop().map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
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
pub async fn save_config(
    app: tauri::State<'_, App>,
    params: config::Params,
    restart: bool,
) -> Result<serde_json::Value, String> {
    config::validate(&params)?;
    let runner = Arc::clone(&app.runner);
    let paths = app.paths.clone();
    // Off the main thread: archiving moves the database and its WAL, which are hundreds
    // of megabytes, and doing that on the event loop freezes the window for the duration.
    tauri::async_runtime::spawn_blocking(move || {
        let was_running = matches!(runner.probe(), RunState::Running | RunState::Starting);
        if was_running {
            runner.stop().map_err(|e| e.to_string())?;
        }
        config::write_params(&paths.config(), &params).map_err(|e| e.to_string())?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let archived = archive::archive_ledger(&paths.ledger(), &paths.archive_dir(), &stamp)
            .map_err(|e| e.to_string())?;
        let mut restarted = false;
        let mut restart_error = None;
        if restart && was_running {
            match runner.start() {
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
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read the live ledger. **Async, and the SQLite work happens off the main thread.**
///
/// # Why this must not be a synchronous command
///
/// Tauri runs synchronous commands on the main thread — the same thread that pumps the
/// window's event loop. Reading this ledger takes tens of seconds (it walks every sweep
/// to build episodes), which pinned a core and froze the interface for the whole of
/// startup. It presented as a spin in the event loop and was really one blocking call
/// on the wrong thread.
///
/// `crates/server/src/routes.rs` already did this correctly for the same reason. This
/// is that lesson, applied on the second occasion it was needed.
///
/// # Errors
/// If the blocking task panics or is cancelled. A ledger that cannot be read is not an
/// error here — it comes back as `{available: false, reason}`.
#[tauri::command]
pub async fn read_history(app: tauri::State<'_, App>) -> Result<serde_json::Value, String> {
    let db = app.paths.ledger();
    tauri::async_runtime::spawn_blocking(move || history::snapshot(&db))
        .await
        .map_err(|e| e.to_string())
}

/// # Errors
/// If the blocking task panics or is cancelled.
#[tauri::command]
pub async fn read_history_at(path: String) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || history::snapshot(std::path::Path::new(&path)))
        .await
        .map_err(|e| e.to_string())
}

/// # Errors
/// If the blocking task panics or is cancelled.
#[tauri::command]
pub async fn read_archives(
    app: tauri::State<'_, App>,
) -> Result<Vec<archive::ArchivedRun>, String> {
    let dir = app.paths.archive_dir();
    tauri::async_runtime::spawn_blocking(move || archive::list_archives(&dir))
        .await
        .map_err(|e| e.to_string())
}

/// # Errors
/// If the blocking task panics or is cancelled.
#[tauri::command]
pub async fn read_log(app: tauri::State<'_, App>, lines: usize) -> Result<Vec<String>, String> {
    let log = app.paths.log();
    tauri::async_runtime::spawn_blocking(move || logs::tail(&log, lines))
        .await
        .map_err(|e| e.to_string())
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

/// Key custody, all four of them, kept together because they share one invariant: the
/// secret exists in the clear only inside [`wallet_import`], and only for as long as
/// that call runs.
///
/// None of these are `spawn_blocking`. Argon2 at 64 MiB takes a noticeable fraction of a
/// second and that is a main-thread stall — but these are operator actions taken once,
/// not the polling path that froze the window before, and moving a secret across a
/// thread boundary to save 300ms is the wrong trade.
#[tauri::command]
pub fn wallet_status(app: tauri::State<'_, App>) -> crate::wallet::WalletStatus {
    crate::wallet::status(&app.paths.wallet(), &app.custody)
}

/// # Errors
/// If the key is not valid, the passphrase is empty, or the file cannot be written.
#[tauri::command]
pub fn wallet_import(
    app: tauri::State<'_, App>,
    secret: String,
    passphrase: String,
) -> Result<crate::wallet::WalletStatus, String> {
    crate::wallet::import(&app.paths.wallet(), secret, &passphrase)
}

/// # Errors
/// If there is no key, or the passphrase is wrong.
#[tauri::command]
pub fn wallet_unlock(
    app: tauri::State<'_, App>,
    passphrase: String,
) -> Result<crate::wallet::WalletStatus, String> {
    crate::wallet::unlock(&app.paths.wallet(), &passphrase, &app.custody)
}

/// # Errors
/// If the key file cannot be removed.
#[tauri::command]
pub fn wallet_forget(app: tauri::State<'_, App>) -> Result<crate::wallet::WalletStatus, String> {
    crate::wallet::remove(&app.paths.wallet(), &app.custody)
}

/// How many distinct token accounts a cycle needs, for the readiness calculation.
///
/// Three, because three hops is the ceiling a 1232-byte packet allows — see
/// `cb_executor::tx`. A two-hop cycle needs two, so this is the pessimistic answer and
/// it is the right one: an operator told they are ready should be ready for the widest
/// cycle the instrument can find, not the narrowest.
const MINTS_PER_CYCLE: usize = 3;

/// What the trading address holds, read live from the configured RPC endpoint.
///
/// Read-only, and it needs no passphrase: the address is stored in the clear beside the
/// ciphertext, so balances are visible whether or not the key is unlocked. Seeing what
/// an address holds should never require the ability to spend from it.
///
/// # Errors
/// If no key is configured, or the node cannot be reached.
#[tauri::command]
pub async fn wallet_balances(app: tauri::State<'_, App>) -> Result<crate::balances::Holdings, String> {
    let status = crate::wallet::status(&app.paths.wallet(), &app.custody);
    let Some(address) = status.pubkey else {
        return Err("no key is configured, so there is no address to look up".into());
    };
    let rpc = config::read_rpc_url(&app.paths.config()).map_err(|e| e.to_string())?;
    crate::balances::fetch(&rpc, &address, MINTS_PER_CYCLE).await.map_err(|e| e.to_string())
}

/// # Errors
/// If the config cannot be read or parsed.
#[tauri::command]
pub fn read_mode(app: tauri::State<'_, App>) -> Result<config::ModeStatus, String> {
    config::read_mode(&app.paths.config()).map_err(|e| e.to_string())
}

/// Switch between demo and live, archiving the run either way.
///
/// # Why the preconditions are checked here and not in the window
///
/// The window can be made to send anything. Every guard that matters is re-checked in
/// this function, so a malformed `invoke` cannot arm live trading by skipping a dialog.
/// `confirm` must be the literal word `LIVE`: not a checkbox, because a checkbox is one
/// stray click and this is not a decision that should be reachable by one stray click.
///
/// # Why the ledger is archived
///
/// `paper_fills` has no column saying which mode produced a row, so a run that changed
/// mode half way through would leave a table whose realised P&L is part simulation and
/// part real money, with nothing downstream able to separate them. Cutting the run is
/// the only thing that keeps the two readable.
///
/// # Errors
/// If a precondition fails, or the config cannot be written. On any failure the file
/// and the run are left untouched.
#[tauri::command]
pub async fn set_mode(
    app: tauri::State<'_, App>,
    mode: String,
    confirm: String,
) -> Result<serde_json::Value, String> {
    let going_live = mode == "live";

    if going_live {
        if confirm.trim() != "LIVE" {
            return Err("Type LIVE to confirm. Nothing has been changed.".into());
        }
        if !app.custody.is_unlocked() {
            return Err(
                "No key is unlocked in this session. Import and unlock one before arming live \
                 trading."
                    .into(),
            );
        }
        let limits = config::read_limits(&app.paths.config()).map_err(|e| e.to_string())?;
        limits.validate()?;
        if !config::EXECUTION_IMPLEMENTED {
            // Refused rather than allowed-with-a-warning. Writing mode = "live" here
            // would produce a config the bot declines to start with, and an operator
            // staring at a stopped instrument wondering which of the two things they
            // just did broke it.
            return Err(
                "Live execution is not implemented in this build: no swap instructions are \
                 encoded and cb-bot links no signing code. Arming live mode would only stop \
                 the bot from starting. Demo mode is the whole of what currently works."
                    .into(),
            );
        }
    } else if mode != "paper" {
        return Err(format!("unknown mode {mode:?}"));
    }

    let runner = Arc::clone(&app.runner);
    let paths = app.paths.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let was_running = matches!(runner.probe(), RunState::Running | RunState::Starting);
        if was_running {
            runner.stop().map_err(|e| e.to_string())?;
        }
        // Archive before the write, so a crash in between leaves the old rows filed
        // under the mode that produced them rather than orphaned beside a new config.
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let archived = archive::archive_ledger(&paths.ledger(), &paths.archive_dir(), &stamp)
            .map_err(|e| e.to_string())?;
        config::write_mode(&paths.config(), &mode).map_err(|e| e.to_string())?;

        let mut restarted = false;
        let mut restart_error = None;
        if was_running {
            match runner.start() {
                Ok(()) => restarted = true,
                Err(e) => restart_error = Some(e.to_string()),
            }
        }
        Ok(serde_json::json!({
            "mode": mode,
            "archived": archived.map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
            "wasRunning": was_running,
            "restarted": restarted,
            "restartError": restart_error,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// # Errors
/// If the config cannot be read or parsed.
#[tauri::command]
pub fn read_limits(app: tauri::State<'_, App>) -> Result<cb_executor::risk::Limits, String> {
    config::read_limits(&app.paths.config()).map_err(|e| e.to_string())
}

/// Saving limits deliberately does **not** archive the run — see `config::write_limits`.
///
/// # Errors
/// If the limits are unusable or the file cannot be written.
#[tauri::command]
pub fn save_limits(
    app: tauri::State<'_, App>,
    limits: cb_executor::risk::Limits,
) -> Result<(), String> {
    config::write_limits(&app.paths.config(), &limits).map_err(|e| e.to_string())
}

/// The directory this run reads and writes.
///
/// Worth showing rather than assuming: run from a checkout it is the repository, run
/// from the installer it is a data directory the operator never chose and would
/// otherwise have no way to find. A ledger whose location is a guess is a ledger nobody
/// can go and check.
#[tauri::command]
pub fn get_root(app: tauri::State<'_, App>) -> String {
    app.paths.root.display().to_string()
}
