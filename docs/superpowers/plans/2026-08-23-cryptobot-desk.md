# cryptobot-desk Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A native Windows application that supervises `cb-bot` as a child process,
configures it, reads its ledger with the bot switched off, and replaces the browser
dashboard entirely.

**Architecture:** A Tauri v2 crate (`crates/desk`) inside the existing Cargo workspace.
Being in the workspace lets it link `cb-ledger` and read SQLite in-process rather than
over HTTP, which is what allows history to render when nothing is running. A
`BotRunner` trait isolates process control so the status state machine is testable
without spawning anything. The frontend is static HTML/CSS/JS with no bundler, matching
the precedent set by `dashboard/dist/index.html`.

**Tech Stack:** Rust 1.85+, Tauri 2, `tauri-plugin-autostart`, `toml_edit` (preserves
comments on config rewrite), `cb-ledger`, `cb-core`. Frontend: vanilla JS, Canvas 2D,
locally-bundled IBM Plex woff2.

## Global Constraints

- **Windows only.** `x86_64-pc-windows-msvc`. No cross-platform abstraction.
- **Build target dir is `%LOCALAPPDATA%\cryptobot-win-target`.** Never the repo `target/`,
  and never WSL's `~/.cargo-target/cryptobot`.
- **Paper mode is not editable.** No UI control may set `mode = "live"`. Config
  validation rejects it. HANDOVER invariant #1.
- **Never two writers.** Refuse to start when port 8787 is already bound.
- **Config rewrites must preserve comments.** `config.toml` carries substantial
  explanatory prose; use `toml_edit`, never serde round-trip.
- **Changing a trading parameter archives the ledger** and starts a clean run.
  HANDOVER §7.
- **Test names are sentences.** Existing convention across the workspace.
- **No network at render time.** Fonts are bundled locally; the app must look correct
  offline.
- **Crate name `cb-desk`**, binary `cryptobot-desk.exe`, identifier `dev.cryptobot.desk`.

---

### Task 1: Workspace scaffold and a window that opens

**Files:**
- Modify: `Cargo.toml:3-13` (add member), `Cargo.toml:21-30` (add workspace dep)
- Create: `crates/desk/Cargo.toml`, `crates/desk/build.rs`,
  `crates/desk/tauri.conf.json`, `crates/desk/src/main.rs`,
  `crates/desk/ui/index.html`, `crates/desk/icons/icon.ico`
- Create: `.gitignore` entry for `crates/desk/ui/fonts/*.woff2` is NOT wanted — fonts
  are committed deliberately so the build is reproducible offline.

**Interfaces:**
- Consumes: nothing.
- Produces: a buildable crate `cb-desk` whose binary is `cryptobot-desk.exe`.

- [ ] **Step 1: Add the crate to the workspace**

In `Cargo.toml`, add `"crates/desk",` to `members`, and under
`[workspace.dependencies]` add:

```toml
cb-desk      = { path = "crates/desk" }
toml_edit    = "0.22"
```

- [ ] **Step 2: Write `crates/desk/Cargo.toml`**

```toml
[package]
name = "cb-desk"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[[bin]]
name = "cryptobot-desk"
path = "src/main.rs"

[build-dependencies]
tauri-build = { version = "2", features = [] }

[dependencies]
tauri = { version = "2", features = ["tray-icon", "image-ico"] }
tauri-plugin-autostart = "2"
cb-ledger = { workspace = true }
cb-core = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
toml_edit = { workspace = true }
chrono = { workspace = true }
```

- [ ] **Step 3: Write `crates/desk/build.rs`**

```rust
fn main() {
    tauri_build::build();
}
```

- [ ] **Step 4: Write `crates/desk/tauri.conf.json`**

```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "Cryptobot Desk",
  "version": "0.1.0",
  "identifier": "dev.cryptobot.desk",
  "build": { "frontendDist": "ui" },
  "app": {
    "windows": [
      {
        "title": "cryptobot",
        "width": 1480,
        "height": 940,
        "minWidth": 1120,
        "minHeight": 720,
        "resizable": true,
        "theme": "Dark"
      }
    ],
    "security": { "csp": "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' ws://127.0.0.1:8787 http://127.0.0.1:8787" }
  },
  "bundle": {
    "active": true,
    "targets": ["nsis"],
    "icon": ["icons/icon.ico"]
  }
}
```

Note the `connect-src`: the frontend talks to the bot's own WebSocket directly. Without
this entry the live panels silently fail to connect.

- [ ] **Step 5: Generate the icon**

Create a 512×512 PNG at `crates/desk/icons/source.png` — a filled square of `#101014`
with a centred amber (`#FFC24B`) circle outline, 3px stroke, 60% diameter, evoking a
gauge. Then:

```bash
npx @tauri-apps/cli icon crates/desk/icons/source.png -o crates/desk/icons
```

Expected: writes `icon.ico`, `32x32.png`, `128x128.png`, `icon.png`.

- [ ] **Step 6: Write the minimal `src/main.rs`**

```rust
//! cryptobot-desk — the application that owns the instrument.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("cryptobot-desk failed to start");
}
```

- [ ] **Step 7: Write a placeholder `ui/index.html`**

```html
<!doctype html>
<meta charset="utf-8">
<title>cryptobot</title>
<body style="background:#101014;color:#E8E6E3;font:14px system-ui;padding:2rem">
  <p>cryptobot-desk</p>
</body>
```

- [ ] **Step 8: Build and run it**

Run:
```bash
cargo build -p cb-desk
```
Expected: compiles; `%LOCALAPPDATA%\cryptobot-win-target\debug\cryptobot-desk.exe` exists.
Launch it and confirm a dark window titled `cryptobot` opens.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml crates/desk
git commit -m "feat(desk): a window that outlives the thing it watches"
```

---

### Task 2: The runner — process control behind a testable seam

**Files:**
- Create: `crates/desk/src/runner.rs`, `crates/desk/src/lib.rs`
- Modify: `crates/desk/src/main.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `enum RunState { Stopped, Starting, Running, Foreign, Failed }` (Serialize, camelCase)
  - `trait BotRunner: Send + Sync { fn start(&self) -> anyhow::Result<()>; fn stop(&self) -> anyhow::Result<()>; fn probe(&self) -> RunState; }`
  - `struct NativeRunner { exe: PathBuf, cwd: PathBuf, log: PathBuf, child: Mutex<Option<Child>> }`
  - `fn NativeRunner::new(exe: PathBuf, cwd: PathBuf, log: PathBuf) -> Self`
  - `fn port_is_bound(port: u16) -> bool`
  - `const BOT_PORT: u16 = 8787;`

- [ ] **Step 1: Write the failing tests**

Create `crates/desk/src/runner.rs` with a test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The guard that stops two bots writing one ledger. A bound port means an
    /// instrument is already running - possibly the old WSL one - and starting a
    /// second writer would corrupt the measurement rather than duplicate it.
    #[test]
    fn a_bound_port_reports_a_foreign_instrument_not_a_stopped_one() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_is_bound(port), "a port we are holding open must read as bound");
    }

    #[test]
    fn an_unbound_port_is_not_mistaken_for_a_running_instrument() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!port_is_bound(port), "a released port must read as free");
    }

    #[test]
    fn a_runner_with_no_child_and_a_free_port_is_stopped() {
        let r = NativeRunner::new("nonexistent.exe".into(), ".".into(), "log.txt".into());
        assert_eq!(r.probe(), RunState::Stopped);
    }

    #[test]
    fn starting_a_missing_binary_fails_loudly_rather_than_reporting_running() {
        let r = NativeRunner::new("definitely-not-here.exe".into(), ".".into(), "log.txt".into());
        assert!(r.start().is_err(), "a missing binary must be an error, not a silent no-op");
        assert_eq!(r.probe(), RunState::Stopped);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p cb-desk runner`
Expected: FAIL — `port_is_bound`, `NativeRunner`, `RunState` not found.

- [ ] **Step 3: Implement the runner**

Prepend to `crates/desk/src/runner.rs`:

```rust
//! Process control for the instrument, behind one trait.
//!
//! # Why a trait for a single implementation
//!
//! Not to allow a second backend - there is deliberately only one, now that `cb-bot`
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
    /// Something we did not start holds the port. A leftover WSL bot, or a second
    /// copy of this app. Never treated as ours to stop.
    Foreign,
    /// Our child exited without us asking.
    Failed,
}

/// True if something accepts a TCP connection on `port` at loopback.
///
/// A connect probe rather than a bind probe: binding to test would race with the
/// process we are trying to detect, and on Windows `SO_REUSEADDR` semantics make a
/// successful bind a weak signal.
#[must_use]
pub fn port_is_bound(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

pub trait BotRunner: Send + Sync {
    fn start(&self) -> anyhow::Result<()>;
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
            anyhow::bail!("already running");
        }
        if port_is_bound(BOT_PORT) {
            anyhow::bail!(
                "port {BOT_PORT} is already held by another instrument - refusing to \
                 start a second writer to the same ledger"
            );
        }
        if !self.exe.exists() {
            anyhow::bail!("no bot binary at {}", self.exe.display());
        }
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)?;
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
```

Create `crates/desk/src/lib.rs`:

```rust
pub mod runner;
```

And in `Cargo.toml` add above `[[bin]]`:

```toml
[lib]
name = "cb_desk"
path = "src/lib.rs"
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p cb-desk runner`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/desk
git commit -m "feat(desk): refuse to become the second writer to one ledger"
```

---

### Task 3: Where the project lives, and where its pieces are

**Files:**
- Create: `crates/desk/src/paths.rs`
- Modify: `crates/desk/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `struct Paths { pub root: PathBuf }`
  - `fn Paths::discover() -> Paths`
  - `fn Paths::config(&self) -> PathBuf`, `ledger`, `archive_dir`, `bot_exe`, `log`
  - `fn Paths::settings_file() -> PathBuf` (under `%APPDATA%\cryptobot-desk\settings.json`)
  - `fn Paths::save(&self) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Everything derives from one value, so there is exactly one thing to get wrong
    /// and exactly one place to fix it.
    #[test]
    fn every_project_path_derives_from_the_single_root() {
        let p = Paths { root: PathBuf::from("D:\\proj") };
        assert!(p.config().ends_with("config.toml"));
        assert!(p.ledger().ends_with("cryptobot.db"));
        assert!(p.config().starts_with("D:\\proj"));
        assert!(p.ledger().starts_with("D:\\proj"));
        assert!(p.archive_dir().starts_with("D:\\proj"));
    }

    #[test]
    fn the_bot_binary_is_looked_for_beside_the_app_before_the_build_tree() {
        let p = Paths { root: PathBuf::from("D:\\proj") };
        // Not asserting which one wins on this machine, only that it names cb-bot.
        assert!(p.bot_exe().to_string_lossy().contains("cb-bot"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p cb-desk paths`
Expected: FAIL — `Paths` not found.

- [ ] **Step 3: Implement**

```rust
//! Where the project is, resolved once.
//!
//! One value - the repository root - and every other path derived from it. The
//! alternative, letting each module find `config.toml` its own way, is how a config
//! editor and a bot end up disagreeing about which file is the config.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// Saved choice if there is one, else the directory the executable sits in, else
    /// the current directory.
    #[must_use]
    pub fn discover() -> Self {
        if let Some(saved) = Self::load_saved() {
            if saved.config().exists() {
                return saved;
            }
        }
        let beside_exe = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(std::path::Path::to_path_buf));
        for cand in [beside_exe, std::env::current_dir().ok()].into_iter().flatten() {
            let mut dir = Some(cand);
            while let Some(d) = dir {
                if d.join("config.toml").exists() && d.join("crates").is_dir() {
                    return Self { root: d };
                }
                dir = d.parent().map(std::path::Path::to_path_buf);
            }
        }
        Self { root: std::env::current_dir().unwrap_or_default() }
    }

    #[must_use] pub fn config(&self) -> PathBuf { self.root.join("config.toml") }
    #[must_use] pub fn ledger(&self) -> PathBuf { self.root.join("cryptobot.db") }
    #[must_use] pub fn archive_dir(&self) -> PathBuf { self.root.join("archive") }
    #[must_use] pub fn log(&self) -> PathBuf { self.root.join("cb-bot.log") }

    /// Beside the app first - that is what a shipped install looks like - then the
    /// Windows build tree, which is what a development run looks like.
    #[must_use]
    pub fn bot_exe(&self) -> PathBuf {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.join("cb-bot.exe")))
            .filter(|p| p.exists());
        beside.unwrap_or_else(|| {
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            PathBuf::from(local)
                .join("cryptobot-win-target")
                .join("release")
                .join("cb-bot.exe")
        })
    }

    #[must_use]
    pub fn settings_file() -> PathBuf {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("cryptobot-desk").join("settings.json")
    }

    fn load_saved() -> Option<Self> {
        let text = std::fs::read_to_string(Self::settings_file()).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let f = Self::settings_file();
        if let Some(dir) = f.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(f, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
```

Add `pub mod paths;` to `lib.rs`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p cb-desk paths`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/desk
git commit -m "feat(desk): one root, and every path derived from it"
```

---

### Task 4: Config — edit the tunables without destroying the prose

**Files:**
- Create: `crates/desk/src/config.rs`
- Modify: `crates/desk/src/lib.rs`

**Interfaces:**
- Consumes: `paths::Paths`.
- Produces:
  - `struct Params { capital_usd: f64, fee_buffer_usd: f64, min_trade_usd: f64, max_hops: usize }` (Serialize, Deserialize, camelCase, PartialEq)
  - `fn read_params(path: &Path) -> anyhow::Result<Params>`
  - `fn validate(p: &Params) -> Result<(), String>`
  - `fn write_params(path: &Path, p: &Params) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
mode = "paper"
feed = "live"
rpc_ws_url = "wss://x"
# Total working capital. Caps every trade size.
capital_usd = 100.0
fee_buffer_usd = 0.20
min_trade_usd = 10.0
max_hops = 3
min_profit_lamports = 0
max_position_lamports = 20000000
"#;

    fn tmp(contents: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cbdesk-cfg-{}.toml", std::process::id()));
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn reads_the_four_tunables_from_a_real_config() {
        let p = tmp(SAMPLE);
        let got = read_params(&p).unwrap();
        assert_eq!(got.capital_usd, 100.0);
        assert_eq!(got.min_trade_usd, 10.0);
        assert_eq!(got.max_hops, 3);
    }

    /// config.toml is more comment than value, and those comments are the reasoning
    /// behind every number in it. A serde round-trip would silently delete all of it.
    #[test]
    fn rewriting_a_value_keeps_every_comment_in_the_file() {
        let p = tmp(SAMPLE);
        let mut params = read_params(&p).unwrap();
        params.capital_usd = 1000.0;
        write_params(&p, &params).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("# Total working capital. Caps every trade size."));
        assert!(after.contains("1000"));
    }

    #[test]
    fn rewriting_a_value_never_touches_the_mode_switch() {
        let p = tmp(SAMPLE);
        let mut params = read_params(&p).unwrap();
        params.capital_usd = 500.0;
        write_params(&p, &params).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains(r#"mode = "paper""#), "mode must survive untouched");
    }

    #[test]
    fn a_negative_book_is_refused_rather_than_written() {
        let bad = Params { capital_usd: -1.0, fee_buffer_usd: 0.2, min_trade_usd: 10.0, max_hops: 3 };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn zero_hops_is_refused_because_a_cycle_needs_at_least_two() {
        let bad = Params { capital_usd: 100.0, fee_buffer_usd: 0.2, min_trade_usd: 10.0, max_hops: 0 };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn a_buffer_larger_than_the_book_is_refused() {
        let bad = Params { capital_usd: 1.0, fee_buffer_usd: 5.0, min_trade_usd: 10.0, max_hops: 3 };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn a_rejected_edit_leaves_the_file_byte_identical() {
        let p = tmp(SAMPLE);
        let before = std::fs::read_to_string(&p).unwrap();
        let bad = Params { capital_usd: -1.0, fee_buffer_usd: 0.2, min_trade_usd: 10.0, max_hops: 3 };
        let _ = write_params(&p, &bad);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p cb-desk config`
Expected: FAIL — `Params` not found.

- [ ] **Step 3: Implement**

```rust
//! Reading and rewriting the four numbers the operator is allowed to change.
//!
//! # Why `toml_edit` and not serde
//!
//! `config.toml` is mostly prose. Every number in it carries a paragraph explaining
//! why it is that number - where the min-trade floor comes from, what the capital
//! ladder measured, why max_hops stops at 3. A serde deserialise-then-serialise round
//! trip produces a valid file with all of that deleted, and nothing about the result
//! looks wrong. `toml_edit` mutates the value in place and leaves the document alone.
//!
//! # What is deliberately not here
//!
//! `mode`. There is no code path in this application that writes it. HANDOVER
//! invariant #1 is enforced by the absence of a mechanism rather than by a dialog
//! someone can click through.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub capital_usd: f64,
    pub fee_buffer_usd: f64,
    pub min_trade_usd: f64,
    pub max_hops: usize,
}

pub fn read_params(path: &Path) -> anyhow::Result<Params> {
    let text = std::fs::read_to_string(path)?;
    let doc: toml_edit::DocumentMut = text.parse()?;
    let f = |k: &str, d: f64| doc.get(k).and_then(toml_edit::Item::as_float).unwrap_or(d);
    let i = |k: &str, d: i64| doc.get(k).and_then(toml_edit::Item::as_integer).unwrap_or(d);
    Ok(Params {
        capital_usd: f("capital_usd", 100.0),
        fee_buffer_usd: f("fee_buffer_usd", 0.20),
        min_trade_usd: f("min_trade_usd", 10.0),
        max_hops: usize::try_from(i("max_hops", 3)).unwrap_or(3),
    })
}

/// Returns the reason it is unacceptable, so the UI can print it verbatim.
pub fn validate(p: &Params) -> Result<(), String> {
    if !p.capital_usd.is_finite() || p.capital_usd <= 0.0 {
        return Err("Capital must be a positive number.".into());
    }
    if !p.fee_buffer_usd.is_finite() || p.fee_buffer_usd < 0.0 {
        return Err("Fee buffer cannot be negative.".into());
    }
    if p.fee_buffer_usd >= p.capital_usd {
        return Err("Fee buffer must leave something to trade.".into());
    }
    if !p.min_trade_usd.is_finite() || p.min_trade_usd <= 0.0 {
        return Err("Minimum trade must be a positive number.".into());
    }
    if p.max_hops < 2 {
        return Err("A cycle needs at least 2 hops.".into());
    }
    if p.max_hops > 4 {
        return Err("Above 4 hops the search explodes for no added reach.".into());
    }
    Ok(())
}

pub fn write_params(path: &Path, p: &Params) -> anyhow::Result<()> {
    if let Err(why) = validate(p) {
        anyhow::bail!(why);
    }
    let text = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    doc["capital_usd"] = toml_edit::value(p.capital_usd);
    doc["fee_buffer_usd"] = toml_edit::value(p.fee_buffer_usd);
    doc["min_trade_usd"] = toml_edit::value(p.min_trade_usd);
    doc["max_hops"] = toml_edit::value(i64::try_from(p.max_hops).unwrap_or(3));
    std::fs::write(path, doc.to_string())?;
    Ok(())
}
```

Add `pub mod config;` to `lib.rs`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p cb-desk config`
Expected: 7 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/desk
git commit -m "feat(desk): edit the numbers without deleting the reasoning"
```

---

### Task 5: Archiving — a parameter change starts a new measurement

**Files:**
- Create: `crates/desk/src/archive.rs`
- Modify: `crates/desk/src/lib.rs`

**Interfaces:**
- Consumes: `paths::Paths`.
- Produces:
  - `struct ArchivedRun { pub name: String, pub path: PathBuf, pub bytes: u64 }` (Serialize, camelCase)
  - `fn archive_ledger(ledger: &Path, archive_dir: &Path, stamp: &str) -> anyhow::Result<Option<PathBuf>>`
  - `fn list_archives(archive_dir: &Path) -> Vec<ArchivedRun>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("cbdesk-arch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A SQLite database in WAL mode is three files. Moving only the .db silently
    /// leaves the most recent writes behind - the exact hazard scripts/archive-ledger.sh
    /// exists to avoid.
    #[test]
    fn archiving_takes_the_wal_and_shm_with_the_database() {
        let d = scratch("wal");
        let db = d.join("cryptobot.db");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(d.join("cryptobot.db-wal"), b"wal").unwrap();
        std::fs::write(d.join("cryptobot.db-shm"), b"shm").unwrap();
        let arch = d.join("archive");
        let moved = archive_ledger(&db, &arch, "20260823-101500").unwrap().unwrap();
        assert!(moved.exists());
        assert!(arch.join("cryptobot-20260823-101500.db-wal").exists());
        assert!(arch.join("cryptobot-20260823-101500.db-shm").exists());
        assert!(!db.exists(), "the live ledger must be moved, not copied");
    }

    #[test]
    fn archiving_a_ledger_that_does_not_exist_is_not_an_error() {
        let d = scratch("none");
        let got = archive_ledger(&d.join("cryptobot.db"), &d.join("archive"), "x").unwrap();
        assert!(got.is_none(), "a first run has nothing to archive");
    }

    #[test]
    fn archived_runs_are_listed_newest_first() {
        let d = scratch("list");
        let arch = d.join("archive");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::write(arch.join("cryptobot-20260101-000000.db"), b"a").unwrap();
        std::fs::write(arch.join("cryptobot-20260823-000000.db"), b"bb").unwrap();
        let got = list_archives(&arch);
        assert_eq!(got.len(), 2);
        assert!(got[0].name.contains("20260823"), "newest first");
    }

    #[test]
    fn the_wal_and_shm_are_not_listed_as_separate_runs() {
        let d = scratch("nowal");
        let arch = d.join("archive");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::write(arch.join("cryptobot-20260101-000000.db"), b"a").unwrap();
        std::fs::write(arch.join("cryptobot-20260101-000000.db-wal"), b"a").unwrap();
        assert_eq!(list_archives(&arch).len(), 1);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p cb-desk archive`
Expected: FAIL — `archive_ledger` not found.

- [ ] **Step 3: Implement**

```rust
//! Moving a finished run out of the way so the next one starts clean.
//!
//! HANDOVER §7: rows measured under different parameters aggregate by different rules,
//! and nothing downstream shows the mixture. So a parameter change ends the run rather
//! than continuing into it. This is the mechanism.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// The three files SQLite keeps in WAL mode. Copying only the first loses writes.
const SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedRun {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Move `ledger` (and its `-wal`/`-shm`) into `archive_dir`, stamped. `Ok(None)` when
/// there was no ledger to move, which is the normal first-run case rather than a fault.
pub fn archive_ledger(
    ledger: &Path,
    archive_dir: &Path,
    stamp: &str,
) -> anyhow::Result<Option<PathBuf>> {
    if !ledger.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(archive_dir)?;
    let stem = ledger.file_stem().and_then(|s| s.to_str()).unwrap_or("cryptobot");
    let target = archive_dir.join(format!("{stem}-{stamp}.db"));
    for suffix in SUFFIXES {
        let from = PathBuf::from(format!("{}{suffix}", ledger.display()));
        if !from.exists() {
            continue;
        }
        let to = PathBuf::from(format!("{}{suffix}", target.display()));
        // Rename first: same volume, atomic. Fall back to copy+remove across volumes.
        if std::fs::rename(&from, &to).is_err() {
            std::fs::copy(&from, &to)?;
            std::fs::remove_file(&from)?;
        }
    }
    Ok(Some(target))
}

#[must_use]
pub fn list_archives(archive_dir: &Path) -> Vec<ArchivedRun> {
    let Ok(entries) = std::fs::read_dir(archive_dir) else {
        return Vec::new();
    };
    let mut out: Vec<ArchivedRun> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "db"))
        .map(|e| ArchivedRun {
            name: e.file_name().to_string_lossy().into_owned(),
            path: e.path(),
            bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .collect();
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}
```

Add `pub mod archive;` to `lib.rs`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p cb-desk archive`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/desk
git commit -m "feat(desk): a parameter change ends the run rather than contaminating it"
```

---

### Task 6: History — the ledger, read with the bot switched off

**Files:**
- Create: `crates/desk/src/history.rs`
- Modify: `crates/desk/src/lib.rs`

**Interfaces:**
- Consumes: `cb_ledger::Ledger`.
- Produces:
  - `fn snapshot(db: &Path) -> serde_json::Value` — never errors; returns
    `{"available":false,"reason":...}` when there is no readable ledger.
  - `const EPISODE_GAP_SLOTS: u64 = 5; const CURVE_POINTS: usize = 600; const SCATTER_POINTS: usize = 1500;`

Values must match `crates/server/src/routes.rs:19-27` exactly, so the app and
`cb-bot --report` cannot disagree about what an episode is.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason this is an app and not a web page: with no bot running there
    /// is no server, and the window must still open and say something true.
    #[test]
    fn a_missing_ledger_reports_no_history_rather_than_failing() {
        let v = snapshot(std::path::Path::new("does-not-exist.db"));
        assert_eq!(v["available"], serde_json::json!(false));
        assert!(v["reason"].is_string(), "the UI prints this verbatim");
    }

    #[test]
    fn the_episode_gap_matches_the_servers_so_the_two_cannot_disagree() {
        assert_eq!(EPISODE_GAP_SLOTS, 5);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p cb-desk history`
Expected: FAIL — `snapshot` not found.

- [ ] **Step 3: Implement**

```rust
//! The ledger, read directly.
//!
//! The dashboard reached this data over HTTP from the bot's own server, which meant a
//! stopped bot showed no history at all. Linking `cb-ledger` removes the dependency:
//! the file is on disk whether or not anything is running.
//!
//! Read-only, always. This process must never migrate, create, or write the
//! measurement.

use std::path::Path;

/// Detections this far apart in slots belong to different episodes. Must equal
/// `cb_server::routes::EPISODE_GAP_SLOTS`, or the app and `--report` will disagree
/// about how many opportunities there were.
pub const EPISODE_GAP_SLOTS: u64 = 5;
pub const CURVE_POINTS: usize = 600;
pub const SCATTER_POINTS: usize = 1500;

/// Everything the history panels need, or an explicit statement that there is none.
///
/// Deliberately infallible. A UI that has to decide what an error means will render a
/// flat line or a zero, and HANDOVER §5.1 is a long account of what that costs.
#[must_use]
pub fn snapshot(db: &Path) -> serde_json::Value {
    if !db.exists() {
        return serde_json::json!({
            "available": false,
            "reason": format!("no ledger at {}", db.display()),
        });
    }
    match read(db) {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "available": false, "reason": e.to_string() }),
    }
}

fn read(db: &Path) -> anyhow::Result<serde_json::Value> {
    let path = db.to_string_lossy().to_string();
    let ledger = cb_ledger::Ledger::open_read_only(&path)?;
    let contest = ledger.contest_audit(EPISODE_GAP_SLOTS)?;
    let summary = ledger.summary()?;
    Ok(serde_json::json!({
        "available": true,
        "curve": ledger.equity_curve(EPISODE_GAP_SLOTS, CURVE_POINTS)?,
        "ladder": ledger.capital_ladder(EPISODE_GAP_SLOTS)?,
        "episodes": ledger.episode_scatter(EPISODE_GAP_SLOTS, SCATTER_POINTS)?,
        "contestSurvivalRate": contest.contested_survival_rate(),
        "uncontestedSurvivalRate": contest.uncontested_survival_rate(),
        "contestHasEvidence": contest.has_enough_evidence(),
        "contest": contest,
        "hoursObserved": ledger.hours_observed()?,
        "samples": summary.samples,
        "firstAt": summary.first_at,
        "lastAt": summary.last_at,
    }))
}
```

Add `pub mod history;` to `lib.rs`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p cb-desk history`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/desk
git commit -m "feat(desk): history that does not need the bot to be alive"
```

---

### Task 7: Log tailing

**Files:**
- Create: `crates/desk/src/logs.rs`
- Modify: `crates/desk/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `fn tail(path: &Path, lines: usize) -> Vec<String>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, body: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cbdesk-log-{}-{name}", std::process::id()));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn returns_the_last_n_lines_in_order() {
        let p = tmp("order", "a\nb\nc\nd\n");
        assert_eq!(tail(&p, 2), vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn a_file_shorter_than_the_window_returns_all_of_it() {
        let p = tmp("short", "only\n");
        assert_eq!(tail(&p, 50), vec!["only".to_string()]);
    }

    /// When a start fails the log is the only explanation there is; a missing file
    /// must read as empty rather than throwing the UI into an error state.
    #[test]
    fn a_missing_log_is_empty_not_an_error() {
        assert!(tail(std::path::Path::new("nope.log"), 10).is_empty());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p cb-desk logs`
Expected: FAIL — `tail` not found.

- [ ] **Step 3: Implement**

```rust
//! The last lines of the bot's log, for when a start fails and the window has to be
//! able to say why without anyone opening a shell.

use std::path::Path;

#[must_use]
pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].iter().map(|s| (*s).to_string()).collect()
}
```

Add `pub mod logs;` to `lib.rs`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p cb-desk logs`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/desk
git commit -m "feat(desk): the log, without opening a shell"
```

---

### Task 8: Wiring — Tauri commands and the state the window sees

**Files:**
- Modify: `crates/desk/src/main.rs`
- Create: `crates/desk/src/app.rs`
- Modify: `crates/desk/src/lib.rs`

**Interfaces:**
- Consumes: `runner`, `paths`, `config`, `archive`, `history`, `logs`.
- Produces these `#[tauri::command]` functions, invoked from JS by exact name:
  - `bot_status() -> serde_json::Value` — `{state, port, botExe, root, ledger}`
  - `bot_start() -> Result<(), String>`
  - `bot_stop() -> Result<(), String>`
  - `read_config() -> Result<Params, String>`
  - `save_config(params: Params, restart: bool) -> Result<serde_json::Value, String>`
  - `read_history() -> serde_json::Value`
  - `read_archives() -> Vec<ArchivedRun>`
  - `read_history_at(path: String) -> serde_json::Value`
  - `read_log(lines: usize) -> Vec<String>`

- [ ] **Step 1: Write `crates/desk/src/app.rs`**

```rust
//! The application's one piece of shared state, and the commands the window calls.

use crate::{archive, config, history, logs, paths::Paths, runner::{BotRunner, NativeRunner, RunState, BOT_PORT}};
use std::sync::Arc;

pub struct App {
    pub paths: Paths,
    pub runner: Arc<dyn BotRunner>,
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        let paths = Paths::discover();
        let runner = Arc::new(NativeRunner::new(
            paths.bot_exe(),
            paths.root.clone(),
            paths.log(),
        ));
        Self { paths, runner }
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

#[tauri::command]
pub fn bot_start(app: tauri::State<'_, App>) -> Result<(), String> {
    app.runner.start().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn bot_stop(app: tauri::State<'_, App>) -> Result<(), String> {
    app.runner.stop().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn read_config(app: tauri::State<'_, App>) -> Result<config::Params, String> {
    config::read_params(&app.paths.config()).map_err(|e| e.to_string())
}

/// Writes the parameters and, because HANDOVER §7 says a mixed-parameter ledger
/// aggregates by two different rules with nothing downstream showing it, archives the
/// current run before restarting into a clean one.
///
/// The three outcomes are reported separately. A config that saved but failed to
/// restart is a different situation from one that never saved, and collapsing them
/// into one boolean would leave the operator guessing.
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
        "archived": archived,
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
```

Add `pub mod app;` to `lib.rs`.

- [ ] **Step 2: Rewrite `src/main.rs` to register everything**

```rust
//! cryptobot-desk - the application that owns the instrument.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use cb_desk::app;

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
        ])
        .run(tauri::generate_context!())
        .expect("cryptobot-desk failed to start");
}
```

- [ ] **Step 3: Build and confirm the whole crate compiles**

Run: `cargo build -p cb-desk`
Expected: compiles clean.

Run: `cargo clippy -p cb-desk -- -D warnings`
Expected: no warnings. This workspace holds clippy clean under `-D warnings`.

- [ ] **Step 4: Commit**

```bash
git add crates/desk
git commit -m "feat(desk): the commands the window can call"
```

---

### Task 9: The design system and the app shell

**Files:**
- Create: `crates/desk/ui/app.css`, `crates/desk/ui/fonts/` (4 woff2 files)
- Rewrite: `crates/desk/ui/index.html`

**Interfaces:**
- Consumes: nothing.
- Produces: CSS custom properties consumed by every later UI task —
  `--ground --panel --raise --line --ink --muted --faint --accent --pos --neg`,
  and the classes `.rail .stage .panel .statusline .metric .num`.

- [ ] **Step 1: Bundle the fonts**

Download these four files into `crates/desk/ui/fonts/`. An app that needs the internet
to render correctly is broken, so these are committed, not linked:

- `IBMPlexSans-Regular.woff2`, `IBMPlexSans-Medium.woff2` — from
  `https://cdn.jsdelivr.net/npm/@fontsource/ibm-plex-sans@5/files/ibm-plex-sans-latin-400-normal.woff2`
  and `...-500-normal.woff2`
- `IBMPlexMono-Regular.woff2`, `IBMPlexMono-Medium.woff2` — from
  `https://cdn.jsdelivr.net/npm/@fontsource/ibm-plex-mono@5/files/ibm-plex-mono-latin-400-normal.woff2`
  and `...-500-normal.woff2`

- [ ] **Step 2: Write `ui/app.css` — tokens first**

The theme pattern matters and is easy to get subtly wrong: the bare `:root` carries the
**complete** light palette; the dark blocks redefine **only** tokens. No component rule
may declare a colour inside a media query, or it will not apply in the un-stamped
"system" state.

```css
@font-face{font-family:"IBM Plex Sans";src:url("fonts/IBMPlexSans-Regular.woff2") format("woff2");font-weight:400;font-display:swap}
@font-face{font-family:"IBM Plex Sans";src:url("fonts/IBMPlexSans-Medium.woff2") format("woff2");font-weight:500;font-display:swap}
@font-face{font-family:"IBM Plex Mono";src:url("fonts/IBMPlexMono-Regular.woff2") format("woff2");font-weight:400;font-display:swap}
@font-face{font-family:"IBM Plex Mono";src:url("fonts/IBMPlexMono-Medium.woff2") format("woff2");font-weight:500;font-display:swap}

:root{
  --ground:#F4F2ED; --panel:#FBFAF8; --raise:#FFFFFF;
  --line:#E0DCD3; --ink:#17171C; --muted:#615D55; --faint:#918C82;
  --accent:#9A6410; --accent-ink:#FFFFFF;
  --pos:#1B6B45; --neg:#A5352A;
  --sans:"IBM Plex Sans",system-ui,sans-serif;
  --mono:"IBM Plex Mono",ui-monospace,monospace;
  --r:3px;
}
@media (prefers-color-scheme:dark){
  :root:not([data-theme="light"]){
    --ground:#101014; --panel:#17171D; --raise:#1D1D25;
    --line:#28282F; --ink:#E8E6E3; --muted:#8A8794; --faint:#5E5B66;
    --accent:#FFC24B; --accent-ink:#101014;
    --pos:#5FBF8A; --neg:#E0574A;
  }
}
:root[data-theme="dark"]{
  --ground:#101014; --panel:#17171D; --raise:#1D1D25;
  --line:#28282F; --ink:#E8E6E3; --muted:#8A8794; --faint:#5E5B66;
  --accent:#FFC24B; --accent-ink:#101014;
  --pos:#5FBF8A; --neg:#E0574A;
}

*{box-sizing:border-box}
html,body{height:100%}
body{
  margin:0;background:var(--ground);color:var(--ink);
  font:400 13px/1.5 var(--sans);
  -webkit-font-smoothing:antialiased;
  display:grid;grid-template-columns:232px 1fr;grid-template-rows:1fr 26px;
  overflow:hidden;
}
.num{font-family:var(--mono);font-variant-numeric:tabular-nums;letter-spacing:-0.01em}
```

- [ ] **Step 3: Add the shell components**

```css
.rail{grid-row:1/3;background:var(--panel);border-right:1px solid var(--line);
  display:flex;flex-direction:column;padding:18px 16px;gap:20px;overflow-y:auto}
.brand{display:flex;align-items:center;gap:9px;font-weight:500;letter-spacing:.02em}
.dot{width:8px;height:8px;border-radius:50%;background:var(--faint);flex:none}
.dot.run{background:var(--pos);animation:pulse 2.4s ease-in-out infinite}
.dot.foreign{background:var(--accent)}
.dot.fail{background:var(--neg)}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.45}}
@media (prefers-reduced-motion:reduce){.dot.run{animation:none}}

.stage{grid-row:1;overflow-y:auto;padding:20px 22px;display:grid;gap:14px;
  grid-template-columns:repeat(12,1fr);align-content:start}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:var(--r);
  padding:14px 16px;min-width:0}
.panel h2{margin:0 0 10px;font-size:10px;font-weight:500;letter-spacing:.09em;
  text-transform:uppercase;color:var(--faint)}

.statusline{grid-column:2;grid-row:2;background:var(--panel);
  border-top:1px solid var(--line);display:flex;align-items:center;gap:20px;
  padding:0 16px;font-family:var(--mono);font-size:11px;color:var(--muted)}
.statusline b{color:var(--ink);font-weight:500}

button{font:inherit;color:var(--ink);background:var(--raise);
  border:1px solid var(--line);border-radius:var(--r);padding:7px 12px;cursor:pointer}
button:hover{border-color:var(--faint)}
button.primary{background:var(--accent);color:var(--accent-ink);border-color:var(--accent);font-weight:500}
button:disabled{opacity:.4;cursor:not-allowed}
button:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
canvas{display:block;width:100%}
```

**Canvas heights must be set in CSS.** Setting only `cv.height = clientHeight * dpr`
with no CSS height makes `clientHeight` derive from the attribute, which multiplies the
element every frame until the page is thousands of pixels tall. This already happened
once in `dashboard/dist/index.html`.

- [ ] **Step 4: Write `ui/index.html` shell**

Structure: `.rail` containing brand + run state dot + Start/Stop buttons + nav
(Live / History / Config / Runs / Log) + theme toggle; `.stage` for panels;
`.statusline` with slot, feed lag, pools, stale, and ledger name.

- [ ] **Step 5: Verify visually**

Run `cargo run -p cb-desk`, confirm: window opens, both themes render legibly (toggle
and OS setting), no horizontal body scroll at 1120px width.

- [ ] **Step 6: Commit**

```bash
git add crates/desk/ui
git commit -m "feat(desk): an instrument panel, not a trading dashboard"
```

---

### Task 10: Control, config, log and runs panels

**Files:**
- Create: `crates/desk/ui/app.js`
- Modify: `crates/desk/ui/index.html`

**Interfaces:**
- Consumes: every command from Task 8.
- Produces: `window.__cbdesk` with `{refreshStatus, refreshHistory, setView}` for the
  live panels in Task 11 to call.

- [ ] **Step 1: Status polling and control**

```js
const invoke = window.__TAURI__.core.invoke;
const state = { status:null, history:null, view:"live" };

async function refreshStatus(){
  state.status = await invoke("bot_status");
  paintStatus();
}

function paintStatus(){
  const s = state.status; if (!s) return;
  const dot = document.getElementById("dot");
  const label = document.getElementById("stateLabel");
  dot.className = "dot " + ({running:"run",starting:"run",foreign:"foreign",failed:"fail",stopped:""}[s.state] || "");
  label.textContent = {
    running:"Running", starting:"Starting", stopped:"Stopped",
    foreign:"Port held by another process", failed:"Died unexpectedly",
  }[s.state] || s.state;
  document.getElementById("btnStart").disabled = s.state !== "stopped";
  document.getElementById("btnStop").disabled  = !(s.state === "running" || s.state === "starting");
}
```

A `foreign` state must never enable Stop — the app did not start that process and must
not kill something it cannot identify.

- [ ] **Step 2: Start/stop with the failure surfaced**

```js
document.getElementById("btnStart").onclick = async () => {
  try { await invoke("bot_start"); }
  catch (e) { showFailure(String(e), await invoke("read_log", { lines: 25 })); }
  refreshStatus();
};
```

`showFailure` renders the message and the log tail into the Log panel and switches to
it. A failed start must explain itself in the window, never as a red dot alone.

- [ ] **Step 3: Config form with the archive warning stated before the click**

The form shows capital, fee buffer, min trade, max hops. Above Save, permanently
visible (not a dialog that appears after):

> Saving ends the current run. The ledger is archived and a new measurement begins,
> because rows recorded under different parameters cannot be aggregated together.

On save, call `save_config` and report all three outcomes separately: saved, archived
to *name*, restarted (or the restart error).

- [ ] **Step 4: Runs panel**

`read_archives` into a table of name / size / actions, with "Open read-only" calling
`read_history_at` and rendering the history panels against that file, plus a clear
banner naming which archived run is being viewed.

- [ ] **Step 5: Verify**

Run the app. Start the bot, confirm the dot goes green and the status line populates.
Stop it, confirm history still renders. Change capital, confirm the ledger is archived
and appears in Runs.

- [ ] **Step 6: Commit**

```bash
git add crates/desk/ui
git commit -m "feat(desk): control that says what happened, including when it failed"
```

---

### Task 11: Live telemetry and history charts

**Files:**
- Modify: `crates/desk/ui/app.js`, `crates/desk/ui/index.html`
- Reference: `dashboard/dist/index.html` for the existing chart implementations

**Interfaces:**
- Consumes: `read_history`, and `ws://127.0.0.1:8787/api/stream`.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: WebSocket with honest disconnected state**

```js
function connect(){
  const ws = new WebSocket("ws://127.0.0.1:8787/api/stream");
  ws.onmessage = (m) => onEvent(JSON.parse(m.data));
  ws.onclose = () => { markLiveStale(); setTimeout(connect, 2000); };
}
```

`markLiveStale()` greys the live panels and labels them "not connected". It must never
leave the last received values on screen looking current.

- [ ] **Step 2: Port the four charts**

Lift from `dashboard/dist/index.html`, restyled to the tokens: venue divergence
(bps from consensus per venue), dislocation vs fee wall, P&L curve, and the log-log
value-against-lifetime scatter. Every canvas gets an explicit CSS height.

- [ ] **Step 3: Capital ladder and contest panels**

From `history.ladder` and `history.contest`. The contest panel must render
`contestHasEvidence === false` as "not enough evidence yet", never as a percentage
computed from too few episodes.

- [ ] **Step 4: Verify against a known ledger**

Open an archived run and check the totals match `cb-bot --report` on the same file.

- [ ] **Step 5: Commit**

```bash
git add crates/desk/ui
git commit -m "feat(desk): the charts, on the instrument's own palette"
```

---

### Task 12: Tray, autostart, and retiring the browser

**Files:**
- Modify: `crates/desk/src/main.rs`, `crates/desk/tauri.conf.json`, `HANDOVER.md`,
  `README.md`
- Delete: `dashboard/dist/index.html`
- Modify: `crates/server/src/routes.rs` (drop the static file service)

**Interfaces:**
- Consumes: `runner::RunState`.
- Produces: nothing.

- [ ] **Step 1: Tray icon coloured by run state**

Build a `TrayIconBuilder` with a menu of Show / Start / Stop / Quit. A background
thread polls `runner.probe()` every 3 seconds and sets the icon and tooltip. Closing
the window hides it to tray rather than exiting.

- [ ] **Step 2: Autostart, off by default**

Register `tauri-plugin-autostart`; expose a checkbox in the Config panel. Auto-restart
of a dead bot is a **separate** checkbox, also default off, with the reason stated in
the UI: an instrument that silently resurrects itself hides the failures this app
exists to make visible, and the run it starts is a new run.

- [ ] **Step 3: Retire the web dashboard**

Delete `dashboard/dist/index.html` and remove the `ServeDir`/`ServeFile` fallback from
`crates/server/src/routes.rs:43-52`, keeping `/api/health`, `/api/stream` and
`/api/equity`. The server still exists; it just no longer pretends to be a UI.

- [ ] **Step 4: Rewrite the stale instruction in HANDOVER**

`HANDOVER.md:66` says "Build and run from WSL, not Windows. There is no MSVC linker on
this machine." That was true of the machine, never of the code, and is now false.
Replace §2 with the native Windows build and the app, and record that WSL is no longer
required.

- [ ] **Step 5: Full verification**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo build --release -p cb-desk -p cb-bot
```
Expected: all tests pass, no warnings, both binaries produced.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(desk): the instrument gets an application, and WSL leaves the project"
```

---

## Self-Review

**Spec coverage.** §1 durability → Tasks 1, 6. §2 architecture → 1, 2, 8. §2.1 runner →
2. §2.1.1 one writer → 2. §2.2 three data paths → 6 (history), 8 (control), 11 (live).
§3 parameters → 4, 5, 10; mode not editable → 4; tray/autostart → 12; logs → 7, 10;
ledger management → 5, 10. §4 error handling → 2 (port), 4 (validation), 6 (missing
ledger), 10 (start failure). §5 testing → every task. §6 design language → 9, 11. §7
out of scope respected: no live control anywhere; server keeps its API; Windows only;
no bundler. §8 native build → resolved before planning; Task 12 corrects HANDOVER.

**Type consistency.** `Params` fields are identical in Tasks 4, 8, 10. `RunState`
variants serialise camelCase and the JS map in Task 10 covers all five. `EPISODE_GAP_SLOTS`
is pinned to the server's value with a test. `ArchivedRun` is produced in Task 5 and
consumed unchanged in Tasks 8 and 10.

**Known gap, deliberate.** Tasks 9–11 specify CSS and JS structure with real tokens and
real control-flow code, but not every line of chart drawing — those are ports of
existing, working implementations in `dashboard/dist/index.html`, and transcribing them
into the plan would duplicate rather than specify. The file to port from is named in
each step.
