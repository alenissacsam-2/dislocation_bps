//! Process control for the instrument, behind one trait.
//!
//! # Why a trait for a single implementation
//!
//! Not to allow a second backend — there is deliberately only one, now that `cb-bot`
//! builds natively for Windows and WSL has left the project. The seam exists so the
//! status state machine can be tested against a fake without spawning a real Solana
//! client, and so "what does the app do when the bot dies" is a unit test rather than
//! an experiment.

use serde::Serialize;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

/// The port `cb-server` binds. Also the interlock: bound means an instrument is
/// already running, whoever started it.
pub const BOT_PORT: u16 = 8787;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RunState {
    /// No child of ours, and nothing holding the port.
    Stopped,
    /// Our child is alive but has not yet bound the port.
    Starting,
    /// Our child is alive and serving.
    Running,
    /// Something we did not start holds the port — a leftover bot, or a second copy
    /// of this app. Never treated as ours to stop.
    Foreign,
    /// Our child exited without us asking it to.
    Failed,
}

/// True if something accepts a TCP connection on `port` at loopback.
///
/// A connect probe rather than a bind probe: binding to test would race with the very
/// process we are trying to detect, and on Windows the socket-reuse semantics make a
/// successful bind a weak signal about whether anyone else is listening.
#[must_use]
pub fn port_is_bound(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

pub trait BotRunner: Send + Sync {
    /// # Errors
    /// If a child is already tracked, the port is held by someone else, the binary is
    /// missing, or the spawn itself fails.
    /// Start the instrument.
    ///
    /// `passphrase` is `Some` only when the config asks for live mode and the key is
    /// unlocked in this session. It is written to the child's stdin and dropped; it is
    /// never an argument, never an environment variable, and never touches disk. An
    /// argument appears in every process listing on the machine, and an environment
    /// variable is inherited by everything the child spawns.
    fn start(&self, passphrase: Option<String>) -> anyhow::Result<()>;
    /// # Errors
    /// If the child cannot be reaped.
    fn stop(&self) -> anyhow::Result<()>;
    fn probe(&self) -> RunState;
}

pub struct NativeRunner {
    exe: PathBuf,
    cwd: PathBuf,
    log: PathBuf,
    child: Mutex<Option<Child>>,
}

impl NativeRunner {
    #[must_use]
    pub fn new(exe: PathBuf, cwd: PathBuf, log: PathBuf) -> Self {
        Self { exe, cwd, log, child: Mutex::new(None) }
    }
}

/// Windows flag that stops a spawned process from getting its own console window.
///
/// `cryptobot-desk` is built with `windows_subsystem = "windows"` and has no console of
/// its own. `cb-bot.exe` has no such declaration, so it is an ordinary console-subsystem
/// binary — and spawning one of those from a windowless parent makes Windows allocate a
/// brand new console for it, empty because stdout and stderr are redirected to the log
/// file, but visible. That console is not part of this application: closing it sends
/// the child a close signal Windows will act on if the process does not react in time,
/// which is a way to kill a live, money-moving bot with one misplaced click on a window
/// that was never meant to be there. The Log tab already shows the same output; this
/// flag is what stops the redundant, dangerous copy of it from ever appearing.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl BotRunner for NativeRunner {
    fn start(&self, passphrase: Option<String>) -> anyhow::Result<()> {
        let mut guard = self.child.lock().unwrap();
        if guard.is_some() {
            anyhow::bail!("the instrument is already running");
        }
        // The interlock. Two processes writing one SQLite ledger corrupts the
        // measurement rather than duplicating it, so a bound port is a refusal and
        // never a race to see who wins.
        if port_is_bound(BOT_PORT) {
            anyhow::bail!(
                "port {BOT_PORT} is already held by another instrument — refusing to \
                 start a second writer against the same ledger"
            );
        }
        if !self.exe.exists() {
            anyhow::bail!("no bot binary at {}", self.exe.display());
        }
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&self.log)?;
        let err = out.try_clone()?;
        let mut cmd = Command::new(&self.exe);
        cmd.current_dir(&self.cwd).stdout(Stdio::from(out)).stderr(Stdio::from(err));
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            // Piped even in paper mode. A bot that finds a live config blocks reading
            // this, and a null stdin would give it EOF and a confusing "no passphrase
            // arrived" instead of the wait it is supposed to do.
            .stdin(Stdio::piped())
            .spawn()?;

        // Write it and close the pipe. Closing matters: the child reads one line, and a
        // pipe left open would leave a paper-mode bot holding a handle it never reads.
        if let Some(mut sink) = child.stdin.take() {
            use std::io::Write;
            if let Some(secret) = passphrase {
                let line = format!("{secret}\n");
                let wrote = sink.write_all(line.as_bytes()).and_then(|()| sink.flush());
                if let Err(e) = wrote {
                    // The child is alive and waiting for something it will never get.
                    // Kill it rather than leaving a process that looks started.
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("could not hand the passphrase to the instrument: {e}");
                }
            }
            drop(sink);
        }

        *guard = Some(child);
        Ok(())
    }

    fn stop(&self) -> anyhow::Result<()> {
        let mut guard = self.child.lock().unwrap();
        if let Some(mut c) = guard.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        Ok(())
    }

    fn probe(&self) -> RunState {
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut() {
            Some(c) => match c.try_wait() {
                Ok(Some(_)) => {
                    *guard = None;
                    RunState::Failed
                }
                Ok(None) if port_is_bound(BOT_PORT) => RunState::Running,
                Ok(None) => RunState::Starting,
                Err(_) => RunState::Failed,
            },
            None if port_is_bound(BOT_PORT) => RunState::Foreign,
            None => RunState::Stopped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard that stops two bots writing one ledger. A bound port means an
    /// instrument is already running, and starting a second writer would corrupt the
    /// measurement rather than duplicate it.
    #[test]
    fn a_port_someone_is_holding_reads_as_bound() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_is_bound(port), "a port we are holding open must read as bound");
    }

    /// Retried, because the single-shot version is a race it loses on a busy machine.
    ///
    /// Binding port 0 asks the OS for an ephemeral port, and once it is dropped the OS
    /// is free to hand the same number to anything else — including the browser, the
    /// app, or another test in the same run. It failed exactly once here, during a
    /// workspace run with the instrument live, and passed five times alone afterwards.
    /// A flake that only fires under load is one that will fire in CI and be blamed on
    /// whatever landed that day.
    #[test]
    fn a_released_port_is_not_mistaken_for_a_running_instrument() {
        for attempt in 0..8 {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            if !port_is_bound(port) {
                return;
            }
            assert!(attempt < 7, "eight freshly released ports all read as bound");
        }
    }

    #[test]
    fn starting_a_missing_binary_fails_loudly_rather_than_reporting_running() {
        let r = NativeRunner::new(
            PathBuf::from("definitely-not-here.exe"),
            PathBuf::from("."),
            std::env::temp_dir().join("cbdesk-test.log"),
        );
        assert!(r.start(None).is_err(), "a missing binary must be an error, not a silent no-op");
    }

    /// Spawns the real `cb-bot.exe` and stops it again.
    ///
    /// Ignored by default: it needs a release build present, it binds a real port, and
    /// it appends a few seconds to whatever ledger sits at the workspace root. Run it
    /// explicitly with `cargo test -p cb-desk -- --ignored --nocapture`.
    ///
    /// Note what it asserts when something else already holds the port — a refusal,
    /// not a race. That is the interlock, and the assertion covers both worlds so the
    /// test is meaningful whether or not an instrument is already running.
    #[test]
    #[ignore = "spawns the real bot"]
    fn the_runner_starts_and_stops_the_real_bot() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("crates/desk sits two levels below the workspace root")
            .to_path_buf();
        let exe = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
            .join("cryptobot-win-target")
            .join("release")
            .join("cb-bot.exe");
        if !exe.exists() {
            eprintln!("no release cb-bot.exe at {}; build it first", exe.display());
            return;
        }
        let already = port_is_bound(BOT_PORT);
        let r = NativeRunner::new(exe, root.clone(), root.join("cb-bot.log"));

        if already {
            let err = r.start(None).expect_err("a bound port must be refused, not raced");
            eprintln!("correctly refused: {err}");
            assert!(err.to_string().contains("refusing"));
            assert_eq!(r.probe(), RunState::Foreign, "a port we did not claim is Foreign");
            return;
        }

        r.start(None).expect("start the bot");
        // Give it time to bind. It reads a pool registry and opens a websocket first,
        // so it is not instant.
        let mut state = r.probe();
        for _ in 0..40 {
            if state == RunState::Running {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
            state = r.probe();
        }
        eprintln!("state after start: {state:?}");
        assert_eq!(state, RunState::Running, "the bot should be serving within 20s");

        r.stop().expect("stop the bot");
        assert_eq!(r.probe(), RunState::Stopped, "after stop, nothing of ours and no port");
    }

    /// Stopping something we never started must not throw. The tray calls this on
    /// quit regardless of state, and an error there would block shutdown.
    #[test]
    fn stopping_when_nothing_is_running_is_not_an_error() {
        let r = NativeRunner::new(
            PathBuf::from("x.exe"),
            PathBuf::from("."),
            std::env::temp_dir().join("cbdesk-test2.log"),
        );
        assert!(r.stop().is_ok());
    }
}
