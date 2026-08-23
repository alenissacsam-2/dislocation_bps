# cryptobot-desk — design

A native Windows application that owns the instrument: starts it, stops it, configures
it, and reads what it has measured. It replaces `dashboard/dist/index.html` and the
browser entirely.

Written 2026-08-23.

---

## 1. Why this exists

The web dashboard has one structural defect that no amount of styling fixes: **it is
served by the thing it observes.** When `cb-bot` is not running there is no server, so
there is no UI — not a degraded UI, none at all. The morning after a WSL restart killed
the process, the only way to learn that was to ask.

An application inverts the dependency. The app is the durable thing; the bot is a child
process it supervises. That single change buys:

- **History with the bot off.** The app links `cb-ledger` and opens the SQLite
  read-only in-process. Past runs are readable whether or not anything is running.
- **A control surface.** Start, stop, reconfigure, archive — actions the dashboard
  could never offer, because a page served by the bot cannot restart the bot.
- **A liveness signal you do not have to request.** A tray icon that is red when the
  instrument is dead is the fix for the failure mode that prompted this.

## 2. Architecture

A new crate in the existing Cargo workspace. Not a separate project, and this is the
decision the rest of the design hangs from: being in the workspace is what allows the
app to depend on `cb-ledger` and `cb-core` directly rather than re-implementing their
readers over HTTP.

```
crates/desk/
  Cargo.toml          tauri, cb-ledger, cb-core
  build.rs            tauri_build::build()
  tauri.conf.json
  src/
    main.rs           window, tray, plugin wiring
    runner.rs         BotRunner trait + Native and Wsl implementations
    config.rs         read/validate/write config.toml
    history.rs        cb_ledger read-only queries, archive listing
    logs.rs           tail a log file into a Tauri event stream
    status.rs         health poll, process probe, state machine
  ui/                 static frontend (no bundler, no framework)
    index.html
    app.css
    app.js
```

### 2.1 The runner boundary

The single most important interface in the app:

```rust
trait BotRunner {
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn probe(&self) -> Result<RunState>;
    fn log_path(&self) -> PathBuf;
    fn kind(&self) -> RunnerKind;
}
```

**Resolved 2026-08-23: there is one implementation, `NativeRunner`.** The open question
in §8 was whether `cb-bot` compiles for `x86_64-pc-windows-msvc`; it does, whole
dependency tree including `rusqlite` with bundled SQLite. So the app spawns
`cb-bot.exe` directly as a child process and **WSL leaves this project entirely** —
taking with it the VM's CPU overhead and the failure class where the VM restarts and
silently kills the run.

The trait survives the simplification because it is what makes the runner testable: a
fake implementation lets the whole status state machine be tested without spawning
anything. A second real implementation would be speculative, so there is not one.

### 2.1.1 Only one instrument may run

The bot and the app both address `cryptobot.db` and port 8787. Two live writers would
corrupt the ledger, and a stale WSL bot from the previous arrangement is a real
possibility on this machine. So `start()` refuses when it finds port 8787 already
bound, reports **who** holds it, and offers to reclaim rather than starting a second
writer. Refusing is the correct default: the ledger is the measurement.

**Locating the project.** The app resolves the repository root once at startup and
stores it in its own settings file under `%APPDATA%`, defaulting to the directory the
executable ships beside. Everything else — `config.toml`, the ledger, the archive
directory, the WSL path translation to `/mnt/d/...` — derives from that one value, so
there is exactly one place that knows where the project lives.

### 2.2 Data flow

Three distinct paths, deliberately not unified:

1. **Live telemetry** — the frontend opens a WebSocket to `127.0.0.1:8787/api/stream`
   when the bot is up, exactly as the dashboard does now. Unchanged, and the bot needs
   no modification.
2. **History** — frontend invokes a Tauri command; Rust reads the ledger read-only via
   `cb_ledger::Ledger::open_read_only`; JSON returns. No HTTP, no running bot required.
3. **Control** — frontend invokes a Tauri command; Rust drives the `BotRunner`; the
   resulting state change is broadcast back as a Tauri event.

Keeping these separate means a dead bot degrades path 1 only. The window still opens,
history still renders, controls still work.

## 3. What it controls

Beyond start and stop:

**Trading parameters.** `capital_usd`, `min_trade_usd`, `max_hops`, `fee_buffer_usd`.
Writing any of them **archives the current ledger and starts a clean run**, because
HANDOVER §7 establishes that rows measured under different parameters aggregate by
different rules and nothing downstream reveals the mixture. Silently continuing into
the same file would produce exactly the class of quiet contamination this project keeps
finding in itself. The archive reuses `scripts/archive-ledger.sh` semantics, which move
the `-wal` and `-shm` alongside the `.db`.

**Mode is not editable.** There is no control anywhere in the UI that can set
`mode = "live"`. HANDOVER invariant #1, enforced by the absence of a mechanism rather
than by a confirmation dialog.

**Tray and autostart.** The app launches with Windows, sits in the tray, and colours
its icon by run state. Auto-restart when the bot is found dead is **off by default**
and opt-in: an instrument that silently resurrects itself hides the very failures this
app exists to make visible, and the run it starts is a new run, not a continuation.

**Log viewer.** Tails the bot's log into the UI. When a start fails, the reason appears
in the window rather than requiring a shell.

**Ledger management.** Lists archived databases with their date ranges, opens any of
them read-only for comparison, and archives the live one on demand.

## 4. Error handling

The governing rule: **a failure must name itself in the window.** The dashboard's habit
of rendering a missing number as a flat line or a zero is precisely the failure mode
HANDOVER §5.1 documents, and it is not repeated here.

| Condition | Behaviour |
|---|---|
| Bot fails to start | Surface the last lines of the log inline, not a red dot |
| Port 8787 already bound | Detect an orphaned process, offer to reclaim it |
| WSL distro not running | Report it distinctly from "bot stopped" |
| Ledger absent | Panels say there is no history, never draw an empty chart |
| Config fails to validate | Refuse the write; the file is never left unparseable |
| Config write succeeds, restart fails | Report both facts separately |

## 5. Testing

- `config.rs` — round-trip parse/serialise; validation rejects negative capital, zero
  hops, `mode = "live"`; a rejected edit leaves the file byte-identical.
- `runner.rs` — command-line construction asserted **without executing**, including the
  bracket-escape in the WSL stop command. The trait makes a fake runner trivial, so
  state-machine tests need no real process.
- `history.rs` — against a fixture ledger; a missing file returns "no history" rather
  than an error the UI must interpret.
- `status.rs` — state transitions across the matrix of (process present, health
  responding), since those two disagree during startup and shutdown.

Test names remain sentences, matching the existing convention.

## 6. Design language

The current dashboard fails because it was designed as a crypto trading dashboard. This
project is not one, and says so in its own first line: *a measurement instrument*. The
visual thesis follows the subject — **precision laboratory equipment**, not a trading
terminal.

- **Ground.** Deep graphite carrying a trace of violet (`#101014`), with warm off-white
  ink (`#E8E6E3`) that reads as phosphor rather than paper. Not the near-black plus
  acid-green that every crypto interface ships.
- **One accent: sodium amber** (`#FFC24B`). The category is uniformly green, purple or
  electric blue; amber reads as instrumentation and is rare here. Green and red appear
  only for profit and loss, desaturated, so that P&L never competes with the interface
  for attention.
- **Type: IBM Plex Sans and IBM Plex Mono.** Engineering provenance, real tabular
  figures, and deliberately neither Inter nor Space Grotesk. Every number is monospaced
  and column-aligned.
- **Layout.** A left rail carrying control and navigation, a dense panel grid, and a
  hardware-style status line pinned to the bottom edge reporting slot, feed lag, pools
  live and stale-excluded. An instrument states its own condition continuously.
- Both themes fully designed. Motion restricted to a single live-pulse on the run
  indicator.

## 7. Out of scope

- **Live trading controls.** Invariant #1. The app must not become the thing that makes
  going live one click away.
- **Rewriting the bot's own HTTP server.** It keeps serving `/api/stream` and
  `/api/equity` unchanged. Only the static dashboard becomes dead weight.
- **Cross-platform.** Windows only. WSL interop is Windows-specific and there is no
  second machine to serve.
- **A frontend framework or bundler.** The existing dashboard is one static file and is
  the better precedent; a build step would add failure modes for no gain at this size.

## 8. The open question, and its answer

**Does `cb-bot` compile for `x86_64-pc-windows-msvc`?** Tested rather than assumed, on
2026-08-23: **yes**, the whole tree, `rusqlite` with bundled SQLite included, clean in
6m54s.

The consequences are larger than the runner selection this was meant to decide:

- **WSL leaves the project.** HANDOVER §2's "build and run from WSL, not Windows" was
  never a property of the code — it was a property of this machine lacking a linker.
  With MSVC installed, that instruction is obsolete and should be rewritten rather than
  left to mislead the next reader.
- **The CPU cost of the VM goes with it.** The measured 35% of a core was work done
  through a virtualisation layer that no longer has to exist.
- **A whole failure class disappears.** The run that died this morning died because the
  WSL VM restarted. A Windows child process supervised by a Windows app cannot fail
  that way.

One migration hazard follows, and §2.1.1 exists because of it: for as long as both
arrangements are installed, two bots could write one ledger. The app refuses to start
into a bound port rather than becoming the second writer.
