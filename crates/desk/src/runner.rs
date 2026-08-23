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
    fn start(&self) -> anyhow::Result<()>;
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

impl BotRunner for NativeRunner {
    fn start(&self) -> anyhow::Result<()> {
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
        let child = Command::new(&self.exe)
            .current_dir(&self.cwd)
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .stdin(Stdio::null())
            .spawn()?;
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

    #[test]
    fn a_released_port_is_not_mistaken_for_a_running_instrument() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!port_is_bound(port), "a released port must read as free");
    }

    #[test]
    fn starting_a_missing_binary_fails_loudly_rather_than_reporting_running() {
        let r = NativeRunner::new(
            PathBuf::from("definitely-not-here.exe"),
            PathBuf::from("."),
            std::env::temp_dir().join("cbdesk-test.log"),
        );
        assert!(r.start().is_err(), "a missing binary must be an error, not a silent no-op");
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
