# Phase 1: Live Pool Pipeline & Price Dashboard — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stream live Solana pool state for Raydium AMM v4 and PumpSwap, detect two-pool arbitrage opportunities off-chain, persist every detection to SQLite, and display them live in a browser dashboard. No transaction is ever signed or sent.

**Architecture:** A Rust workspace where a `feed` streams account updates over WebSocket into an in-memory `pool_store`; a `scanner` reacts to each update by testing candidate two-pool cycles using the verified closed-form math in `cb-core`; results go to a SQLite `ledger` and simultaneously to a lossy `tokio::broadcast` channel that an Axum WebSocket server relays to a Next.js dashboard. The hot path never blocks on the UI.

**Tech Stack:** Rust (stable, WSL2 Ubuntu), `tokio`, `axum` 0.8, `rusqlite` 0.40 (bundled SQLite), `solana-sdk` 4.1, `figment` for config, `tracing` for logs; Next.js 15 + React 19 + Tailwind v4 for the dashboard.

## Global Constraints

- **Build in WSL2 Ubuntu, never Windows.** MSVC `link.exe` is absent. Always: `export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot` before `cargo`, then `cd /mnt/d/Dev/Quant/cryptobot`.
- **No floats in the profit path.** All monetary math is `u64`/`u128` integer arithmetic. Floats are permitted only for display/charting.
- **No transaction signing in Phase 1.** No `Keypair`, no `send_transaction`, no private key handling of any kind reaches the codebase in this phase.
- **No network calls in unit tests.** Decoders are tested against committed fixture bytes.
- **Rust edition 2021, rust-version 1.85.** `u128::isqrt` requires ≥1.84.
- **Every task ends with a passing `cargo test` and a commit.**
- Existing verified module `crates/core/src/amm.rs` is **not to be modified** — its 8 tests are the correctness baseline for all sizing math.

---

### Task 1: Domain types in `cb-core`

**Files:**
- Create: `crates/core/src/types.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod types;`)

**Interfaces:**
- Consumes: nothing.
- Produces: `PoolId(Pubkey)`, `Dex` enum, `PoolState` struct, `Reserves` struct, `Opportunity` struct. Every later task uses these.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_state_exposes_reserves_for_a_given_direction() {
        let p = PoolState {
            id: PoolId([1u8; 32]),
            dex: Dex::RaydiumAmmV4,
            mint_a: [10u8; 32],
            mint_b: [20u8; 32],
            reserve_a: 1_000_000,
            reserve_b: 2_000_000,
            fee_bps: 25,
            slot: 42,
        };
        // Spending mint_a: we pay into reserve_a, receive from reserve_b.
        assert_eq!(p.reserves_for_input(&[10u8; 32]), Some(Reserves { r_in: 1_000_000, r_out: 2_000_000 }));
        // Spending mint_b: the other way round.
        assert_eq!(p.reserves_for_input(&[20u8; 32]), Some(Reserves { r_in: 2_000_000, r_out: 1_000_000 }));
        // A mint this pool does not trade.
        assert_eq!(p.reserves_for_input(&[99u8; 32]), None);
    }

    #[test]
    fn other_mint_returns_the_counterparty() {
        let p = PoolState {
            id: PoolId([1u8; 32]), dex: Dex::PumpSwap,
            mint_a: [10u8; 32], mint_b: [20u8; 32],
            reserve_a: 1, reserve_b: 1, fee_bps: 25, slot: 0,
        };
        assert_eq!(p.other_mint(&[10u8; 32]), Some([20u8; 32]));
        assert_eq!(p.other_mint(&[20u8; 32]), Some([10u8; 32]));
        assert_eq!(p.other_mint(&[99u8; 32]), None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-core types 2>&1 | tail -20
```

Expected: FAIL — `cannot find type PoolState in this scope`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `crates/core/src/types.rs`:

```rust
//! Core domain types. Deliberately free of Solana SDK dependencies so this crate
//! stays pure and fast to compile; pubkeys are raw 32-byte arrays and are converted
//! at the edges.

/// A raw Solana public key. Kept as bytes so `cb-core` needs no solana-sdk dependency.
pub type Pubkey32 = [u8; 32];

/// Identifier for a liquidity pool — its on-chain account address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolId(pub Pubkey32);

/// Which venue a pool belongs to. Determines the decoder and quote math used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dex {
    RaydiumAmmV4,
    PumpSwap,
}

impl Dex {
    /// Human-readable name, used in logs and on the dashboard.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Dex::RaydiumAmmV4 => "Raydium AMM v4",
            Dex::PumpSwap => "PumpSwap",
        }
    }
}

/// Reserves oriented for one swap direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserves {
    /// Reserve of the mint being spent.
    pub r_in: u128,
    /// Reserve of the mint being received.
    pub r_out: u128,
}

/// A decoded constant-product pool at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    pub id: PoolId,
    pub dex: Dex,
    pub mint_a: Pubkey32,
    pub mint_b: Pubkey32,
    pub reserve_a: u128,
    pub reserve_b: u128,
    pub fee_bps: u32,
    /// Slot this state was observed at. Used for staleness checks.
    pub slot: u64,
}

impl PoolState {
    /// Reserves oriented so `r_in` is the reserve of `input_mint`.
    ///
    /// Returns `None` if this pool does not trade `input_mint`.
    #[must_use]
    pub fn reserves_for_input(&self, input_mint: &Pubkey32) -> Option<Reserves> {
        if *input_mint == self.mint_a {
            Some(Reserves { r_in: self.reserve_a, r_out: self.reserve_b })
        } else if *input_mint == self.mint_b {
            Some(Reserves { r_in: self.reserve_b, r_out: self.reserve_a })
        } else {
            None
        }
    }

    /// The counterparty mint to `mint`, or `None` if this pool doesn't trade it.
    #[must_use]
    pub fn other_mint(&self, mint: &Pubkey32) -> Option<Pubkey32> {
        if *mint == self.mint_a {
            Some(self.mint_b)
        } else if *mint == self.mint_b {
            Some(self.mint_a)
        } else {
            None
        }
    }
}

/// A detected (not executed) arbitrage opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opportunity {
    pub pool_buy: PoolId,
    pub pool_sell: PoolId,
    /// Mint we start and end with (the token we hold).
    pub base_mint: Pubkey32,
    /// Intermediate mint we route through.
    pub quote_mint: Pubkey32,
    /// Optimal input size in base-mint base units.
    pub amount_in: u128,
    /// Gross profit in base-mint base units, before fees and tip.
    pub gross_profit: u128,
    pub slot: u64,
}
```

Add to `crates/core/src/lib.rs`:

```rust
pub mod types;
```

- [ ] **Step 4: Run test to verify it passes**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-core 2>&1 | tail -15
```

Expected: PASS — 10 tests total (8 pre-existing amm + 2 new).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/types.rs crates/core/src/lib.rs && git commit -m "feat(core): domain types for pools and opportunities"
```

---

### Task 2: Config with the two-switch live guard

**Files:**
- Create: `crates/core/src/config.rs`
- Create: `config.example.toml`
- Modify: `crates/core/src/lib.rs` (add `pub mod config;`)
- Modify: `crates/core/Cargo.toml` (add `figment`, `serde`)

**Interfaces:**
- Consumes: nothing.
- Produces: `Config`, `Mode`, `Config::load(path) -> anyhow::Result<Config>`, `Config::is_live_enabled() -> bool`.

This task exists to make accidental live trading structurally impossible before any execution code is written.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: Mode) -> Config {
        Config { mode, rpc_ws_url: "wss://x".into(), min_profit_lamports: 1000, max_position_lamports: 10_000_000 }
    }

    #[test]
    fn paper_mode_is_never_live_regardless_of_env() {
        let c = cfg(Mode::Paper);
        assert!(!c.is_live_enabled_with(Some("1")), "paper config must ignore the env switch");
        assert!(!c.is_live_enabled_with(None));
    }

    #[test]
    fn live_mode_requires_the_env_switch_too() {
        let c = cfg(Mode::Live);
        assert!(!c.is_live_enabled_with(None), "config alone must not enable live");
        assert!(!c.is_live_enabled_with(Some("0")));
        assert!(!c.is_live_enabled_with(Some("true")), "only the exact string \"1\" counts");
        assert!(c.is_live_enabled_with(Some("1")), "both switches set must enable live");
    }

    #[test]
    fn default_mode_is_paper() {
        assert_eq!(Mode::default(), Mode::Paper);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-core config 2>&1 | tail -20
```

Expected: FAIL — `cannot find type Config in this scope`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `crates/core/src/config.rs`:

```rust
//! Configuration, and the two-switch guard that gates live trading.
//!
//! Live trading requires BOTH `mode = "live"` in the config file AND the environment
//! variable `CRYPTOBOT_ALLOW_LIVE=1`. Neither alone is sufficient. This is deliberate:
//! no single accidental edit, merge, or stray config file can start spending money.

use serde::{Deserialize, Serialize};

/// Environment variable that forms the second half of the live-trading guard.
pub const LIVE_ENV_VAR: &str = "CRYPTOBOT_ALLOW_LIVE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Detect and record only. Never signs or sends. The default.
    #[default]
    Paper,
    /// Sign and submit real transactions. Requires the env switch as well.
    Live,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub mode: Mode,
    pub rpc_ws_url: String,
    /// Minimum gross profit, in lamports, for an opportunity to be recorded as actionable.
    pub min_profit_lamports: u64,
    /// Hard ceiling on trade size, independent of what the optimiser suggests.
    pub max_position_lamports: u64,
}

impl Config {
    /// Load from a TOML file, with `CRYPTOBOT_` prefixed environment overrides.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        use figment::{providers::{Env, Format, Toml}, Figment};
        Ok(Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("CRYPTOBOT_"))
            .extract()?)
    }

    /// True only if BOTH switches are set. Reads the real environment.
    #[must_use]
    pub fn is_live_enabled(&self) -> bool {
        self.is_live_enabled_with(std::env::var(LIVE_ENV_VAR).ok().as_deref())
    }

    /// Testable core of the guard. `env` is the value of [`LIVE_ENV_VAR`], if set.
    ///
    /// Only the exact string `"1"` counts — not `"true"`, not `"yes"`. Narrow by design,
    /// so that a vaguely-truthy value left in a shell profile cannot arm live trading.
    #[must_use]
    pub fn is_live_enabled_with(&self, env: Option<&str>) -> bool {
        matches!(self.mode, Mode::Live) && env == Some("1")
    }
}
```

Add to `crates/core/src/lib.rs`:

```rust
pub mod config;
```

Add to `crates/core/Cargo.toml` under `[dependencies]`:

```toml
figment = { workspace = true }
anyhow = { workspace = true }
```

Create `config.example.toml`:

```toml
# Copy to config.toml and edit. config.toml is gitignored.
#
# "paper" = detect and record only, never sign or send. This is the default and
# should stay that way until the measured edge justifies otherwise.
# "live"  = real transactions. ALSO requires CRYPTOBOT_ALLOW_LIVE=1 in the environment.
mode = "paper"

# Public mainnet WebSocket. Rate-limited and lags by hundreds of ms; see
# docs/research/03-dex-landscape.md for what that does to measurement accuracy.
rpc_ws_url = "wss://api.mainnet-beta.solana.com"

# Minimum gross profit to record an opportunity as actionable. 1_000_000 lamports
# = 0.001 SOL ≈ $0.077 at $76.97/SOL.
min_profit_lamports = 1000000

# Hard ceiling per trade. 20_000_000 lamports = 0.02 SOL ≈ $1.54.
max_position_lamports = 20000000
```

Add `config.toml` to `.gitignore`.

- [ ] **Step 4: Run test to verify it passes**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-core 2>&1 | tail -15
```

Expected: PASS — 13 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): config with two-switch live-trading guard"
```

---

### Task 3: SQLite ledger schema

**Files:**
- Create: `crates/ledger/src/schema.rs`
- Modify: `crates/ledger/src/lib.rs`
- Modify: `crates/ledger/Cargo.toml` (add `rusqlite`, `anyhow`)

**Interfaces:**
- Consumes: `cb_core::types::Opportunity`.
- Produces: `Ledger::open_in_memory() -> anyhow::Result<Ledger>`, `Ledger::open(path) -> anyhow::Result<Ledger>`, `Ledger::record_opportunity(&Opportunity) -> anyhow::Result<i64>`, `Ledger::count_opportunities() -> anyhow::Result<i64>`.

- [ ] **Step 1: Write the failing test**

Append to `crates/ledger/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::types::{Opportunity, PoolId};

    fn sample() -> Opportunity {
        Opportunity {
            pool_buy: PoolId([1u8; 32]),
            pool_sell: PoolId([2u8; 32]),
            base_mint: [3u8; 32],
            quote_mint: [4u8; 32],
            amount_in: 68_920,
            gross_profit: 9_463,
            slot: 123_456,
        }
    }

    #[test]
    fn opportunities_round_trip() {
        let l = Ledger::open_in_memory().unwrap();
        assert_eq!(l.count_opportunities().unwrap(), 0);
        let id = l.record_opportunity(&sample()).unwrap();
        assert!(id > 0);
        assert_eq!(l.count_opportunities().unwrap(), 1);
    }

    #[test]
    fn every_detection_is_kept_including_duplicates() {
        // Skipped and repeated detections are the research data; never dedupe them away.
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..5 {
            l.record_opportunity(&sample()).unwrap();
        }
        assert_eq!(l.count_opportunities().unwrap(), 5);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-ledger 2>&1 | tail -20
```

Expected: FAIL — `cannot find type Ledger in this scope`.

- [ ] **Step 3: Write minimal implementation**

Replace `crates/ledger/src/lib.rs`:

```rust
//! SQLite persistence. Every detected opportunity is recorded — including ones we
//! skip and why — because the skipped set is the most valuable research output of
//! the paper-trading phase.

mod schema;

use anyhow::Result;
use cb_core::types::Opportunity;
use rusqlite::Connection;

pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        // WAL lets the dashboard read while the bot writes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        schema::migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn record_opportunity(&self, o: &Opportunity) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO opportunities
             (pool_buy, pool_sell, base_mint, quote_mint, amount_in, gross_profit, slot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                &o.pool_buy.0[..],
                &o.pool_sell.0[..],
                &o.base_mint[..],
                &o.quote_mint[..],
                o.amount_in.to_string(),
                o.gross_profit.to_string(),
                o.slot as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn count_opportunities(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM opportunities", [], |r| r.get(0))?)
    }
}
```

Create `crates/ledger/src/schema.rs`:

```rust
//! Schema migrations. `u128` amounts are stored as TEXT because SQLite's INTEGER is
//! 64-bit and token base units can exceed it; storing them as decimal strings keeps
//! them exact and sortable with CAST when needed.

use anyhow::Result;
use rusqlite::Connection;

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS opportunities (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            detected_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            pool_buy      BLOB    NOT NULL,
            pool_sell     BLOB    NOT NULL,
            base_mint     BLOB    NOT NULL,
            quote_mint    BLOB    NOT NULL,
            amount_in     TEXT    NOT NULL,
            gross_profit  TEXT    NOT NULL,
            slot          INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_opp_slot ON opportunities(slot);
        CREATE INDEX IF NOT EXISTS idx_opp_detected ON opportunities(detected_at);
        ",
    )?;
    Ok(())
}
```

Add to `crates/ledger/Cargo.toml` under `[dependencies]`:

```toml
rusqlite = { workspace = true }
```

- [ ] **Step 4: Run test to verify it passes**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-ledger 2>&1 | tail -15
```

Expected: PASS — 2 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(ledger): SQLite schema and opportunity recording"
```

---

### Task 4: PumpSwap pool decoder

PumpSwap is done before Raydium because it is a plain constant-product pool with no
vault indirection — it establishes the decoder pattern with the least complexity.

**Files:**
- Create: `crates/dex/src/pumpswap.rs`
- Create: `crates/dex/src/lib.rs` (replace placeholder)
- Create: `crates/dex/tests/fixtures/README.md`
- Modify: `crates/dex/Cargo.toml`

**Interfaces:**
- Consumes: `cb_core::types::{PoolState, PoolId, Dex}`.
- Produces: `pumpswap::PROGRAM_ID: &str`, `pumpswap::decode(addr: [u8;32], data: &[u8], reserve_a: u128, reserve_b: u128, slot: u64) -> anyhow::Result<PoolState>`.

**Note on reserves:** like Raydium, PumpSwap holds reserves in separate SPL token
accounts, not in the pool account. `decode` therefore takes them as parameters; fetching
them is the feed's job (Task 5). This keeps the decoder a pure function and testable.

- [ ] **Step 1: Write the failing test**

Create `crates/dex/src/pumpswap.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal synthetic PumpSwap `Pool` account: 8-byte Anchor discriminator, then
    /// pool_bump(1) + index(2) + creator(32) + base_mint(32) + quote_mint(32).
    fn synthetic_pool_account() -> Vec<u8> {
        let mut v = vec![0u8; 8]; // discriminator
        v.push(255); // pool_bump
        v.extend_from_slice(&7u16.to_le_bytes()); // index
        v.extend_from_slice(&[9u8; 32]); // creator
        v.extend_from_slice(&[11u8; 32]); // base_mint
        v.extend_from_slice(&[22u8; 32]); // quote_mint
        v
    }

    #[test]
    fn decodes_mints_from_account_bytes() {
        let data = synthetic_pool_account();
        let p = decode([1u8; 32], &data, 5_000, 9_000, 99).unwrap();
        assert_eq!(p.mint_a, [11u8; 32]);
        assert_eq!(p.mint_b, [22u8; 32]);
        assert_eq!(p.reserve_a, 5_000);
        assert_eq!(p.reserve_b, 9_000);
        assert_eq!(p.dex, cb_core::types::Dex::PumpSwap);
        assert_eq!(p.fee_bps, FEE_BPS);
        assert_eq!(p.slot, 99);
    }

    #[test]
    fn rejects_truncated_account_data() {
        let short = vec![0u8; 20];
        assert!(decode([1u8; 32], &short, 1, 1, 0).is_err(), "must not read past the buffer");
    }

    #[test]
    fn rejects_empty_data() {
        assert!(decode([1u8; 32], &[], 1, 1, 0).is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-dex 2>&1 | tail -20
```

Expected: FAIL — `cannot find function decode in this scope`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `crates/dex/src/pumpswap.rs`:

```rust
//! PumpSwap (Pump AMM) pool decoding.
//!
//! Program: pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA
//!
//! Constant-product. The `Pool` account is Anchor-serialised: an 8-byte discriminator
//! followed by the struct. Reserves live in separate SPL token accounts and are passed
//! in by the caller, which keeps this a pure function.

use anyhow::{ensure, Result};
use cb_core::types::{Dex, PoolId, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

/// PumpSwap's swap fee in basis points.
pub const FEE_BPS: u32 = 25;

// Byte offsets into the Pool account, after the 8-byte Anchor discriminator.
const OFF_DISCRIMINATOR: usize = 8;
const OFF_BASE_MINT: usize = OFF_DISCRIMINATOR + 1 + 2 + 32; // bump + index + creator
const OFF_QUOTE_MINT: usize = OFF_BASE_MINT + 32;
const MIN_LEN: usize = OFF_QUOTE_MINT + 32;

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey32 {
    let mut k = [0u8; 32];
    k.copy_from_slice(&data[offset..offset + 32]);
    k
}

/// Decode a PumpSwap pool account into a [`PoolState`].
///
/// `reserve_a` / `reserve_b` are the balances of the base and quote vault token
/// accounts respectively, supplied by the caller.
pub fn decode(
    address: Pubkey32,
    data: &[u8],
    reserve_a: u128,
    reserve_b: u128,
    slot: u64,
) -> Result<PoolState> {
    ensure!(
        data.len() >= MIN_LEN,
        "pumpswap pool account too short: {} bytes, need at least {MIN_LEN}",
        data.len()
    );
    Ok(PoolState {
        id: PoolId(address),
        dex: Dex::PumpSwap,
        mint_a: read_pubkey(data, OFF_BASE_MINT),
        mint_b: read_pubkey(data, OFF_QUOTE_MINT),
        reserve_a,
        reserve_b,
        fee_bps: FEE_BPS,
        slot,
    })
}
```

Replace `crates/dex/src/lib.rs`:

```rust
//! Per-DEX account decoding and quote math.
//!
//! Each venue is one module exposing `PROGRAM_ID` and a pure `decode` function.
//! Decoders take reserves as parameters rather than fetching them, so they remain
//! testable without a network.

pub mod pumpswap;
```

Add to `crates/dex/Cargo.toml` under `[dependencies]`:

```toml
cb-core = { workspace = true }
anyhow = { workspace = true }
```

Create `crates/dex/tests/fixtures/README.md`:

```markdown
# Decoder fixtures

Real mainnet account bytes, committed so decoder tests never touch the network.

Capture a fixture with:

```bash
solana account <POOL_ADDRESS> --output json --output-file <name>.json --url mainnet-beta
```

Record the slot it was captured at in the filename, because layouts change across
program upgrades and a fixture without a slot is unfalsifiable.
```

- [ ] **Step 4: Run test to verify it passes**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-dex 2>&1 | tail -15
```

Expected: PASS — 3 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(dex): PumpSwap pool decoder"
```

---

### Task 5: Pool store with staleness tracking

**Files:**
- Create: `crates/scanner/src/store.rs`
- Modify: `crates/scanner/src/lib.rs`
- Modify: `crates/scanner/Cargo.toml` (add `dashmap`, `cb-core`)

**Interfaces:**
- Consumes: `cb_core::types::{PoolState, PoolId, Pubkey32}`.
- Produces: `PoolStore::new()`, `PoolStore::upsert(PoolState)`, `PoolStore::get(&PoolId) -> Option<PoolState>`, `PoolStore::pools_trading(&Pubkey32) -> Vec<PoolState>`, `PoolStore::len()`, `PoolStore::stale_pools(current_slot, max_lag) -> Vec<PoolId>`.

- [ ] **Step 1: Write the failing test**

Append to `crates/scanner/src/store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::types::{Dex, PoolId, PoolState};

    fn pool(id: u8, a: u8, b: u8, slot: u64) -> PoolState {
        PoolState {
            id: PoolId([id; 32]), dex: Dex::PumpSwap,
            mint_a: [a; 32], mint_b: [b; 32],
            reserve_a: 1_000, reserve_b: 1_000, fee_bps: 25, slot,
        }
    }

    #[test]
    fn upsert_then_get_returns_latest_state() {
        let s = PoolStore::new();
        s.upsert(pool(1, 10, 20, 100));
        assert_eq!(s.get(&PoolId([1; 32])).unwrap().slot, 100);
        s.upsert(pool(1, 10, 20, 200));
        assert_eq!(s.get(&PoolId([1; 32])).unwrap().slot, 200, "newer slot must replace older");
        assert_eq!(s.len(), 1, "same pool must not duplicate");
    }

    #[test]
    fn out_of_order_updates_do_not_regress_state() {
        // WebSocket delivery is not ordered; a late-arriving old slot must be ignored.
        let s = PoolStore::new();
        s.upsert(pool(1, 10, 20, 200));
        s.upsert(pool(1, 10, 20, 100));
        assert_eq!(s.get(&PoolId([1; 32])).unwrap().slot, 200, "stale update must be dropped");
    }

    #[test]
    fn pools_trading_finds_both_orientations() {
        let s = PoolStore::new();
        s.upsert(pool(1, 10, 20, 1));
        s.upsert(pool(2, 20, 10, 1)); // reversed
        s.upsert(pool(3, 30, 40, 1)); // unrelated
        let found = s.pools_trading(&[10; 32]);
        assert_eq!(found.len(), 2, "must match mint in either position");
    }

    #[test]
    fn stale_pools_reports_only_lagging_entries() {
        let s = PoolStore::new();
        s.upsert(pool(1, 10, 20, 100));
        s.upsert(pool(2, 10, 20, 195));
        // current slot 200, tolerate 10 slots of lag
        let stale = s.stale_pools(200, 10);
        assert_eq!(stale, vec![PoolId([1; 32])]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-scanner 2>&1 | tail -20
```

Expected: FAIL — `cannot find type PoolStore in this scope`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `crates/scanner/src/store.rs`:

```rust
//! Concurrent in-memory pool state.
//!
//! The scanner reads this on every account update, so it must be lock-free for
//! readers. `DashMap` gives per-shard locking, which is sufficient: writes are
//! per-pool and readers touch disjoint keys most of the time.

use cb_core::types::{PoolId, PoolState, Pubkey32};
use dashmap::DashMap;

#[derive(Default)]
pub struct PoolStore {
    pools: DashMap<PoolId, PoolState>,
}

impl PoolStore {
    #[must_use]
    pub fn new() -> Self {
        Self { pools: DashMap::new() }
    }

    /// Insert or update a pool.
    ///
    /// Updates carrying an older slot than the stored state are **ignored**.
    /// WebSocket delivery is not ordered, and applying a late old update would
    /// silently regress the book and produce phantom opportunities.
    pub fn upsert(&self, p: PoolState) {
        self.pools
            .entry(p.id)
            .and_modify(|existing| {
                if p.slot >= existing.slot {
                    *existing = p;
                }
            })
            .or_insert(p);
    }

    #[must_use]
    pub fn get(&self, id: &PoolId) -> Option<PoolState> {
        self.pools.get(id).map(|r| *r.value())
    }

    /// Every pool that trades `mint`, in either position.
    #[must_use]
    pub fn pools_trading(&self, mint: &Pubkey32) -> Vec<PoolState> {
        self.pools
            .iter()
            .filter(|r| r.mint_a == *mint || r.mint_b == *mint)
            .map(|r| *r.value())
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pools.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    /// Pools whose last observed slot lags `current_slot` by more than `max_lag`.
    ///
    /// Quoting against stale state is the most likely source of false opportunities,
    /// so the dashboard surfaces this directly.
    #[must_use]
    pub fn stale_pools(&self, current_slot: u64, max_lag: u64) -> Vec<PoolId> {
        let mut v: Vec<PoolId> = self
            .pools
            .iter()
            .filter(|r| current_slot.saturating_sub(r.slot) > max_lag)
            .map(|r| *r.key())
            .collect();
        v.sort();
        v
    }
}
```

Replace `crates/scanner/src/lib.rs`:

```rust
//! Opportunity detection: maintains pool state and tests candidate cycles.

pub mod store;
```

Add to `crates/scanner/Cargo.toml` under `[dependencies]`:

```toml
dashmap = { workspace = true }
```

- [ ] **Step 4: Run test to verify it passes**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-scanner 2>&1 | tail -15
```

Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(scanner): pool store with slot-ordered upsert and staleness"
```

---

### Task 6: Cycle detection over the pool store

**Files:**
- Create: `crates/scanner/src/cycles.rs`
- Modify: `crates/scanner/src/lib.rs` (add `pub mod cycles;`)

**Interfaces:**
- Consumes: `PoolStore`, `cb_core::amm::{CycleReserves, optimal_input, cycle_profit}`, `cb_core::types::{Opportunity, PoolState, Pubkey32}`.
- Produces: `find_two_pool_cycles(&PoolStore, base_mint: &Pubkey32, updated: &PoolState, min_profit: u128) -> Vec<Opportunity>`.

This is where the verified math from `cb-core::amm` finally meets live data.

- [ ] **Step 1: Write the failing test**

Append to `crates/scanner/src/cycles.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PoolStore;
    use cb_core::types::{Dex, PoolId, PoolState};

    const BASE: [u8; 32] = [10; 32];  // e.g. WSOL
    const QUOTE: [u8; 32] = [20; 32]; // the intermediate token

    fn pool(id: u8, ra: u128, rb: u128) -> PoolState {
        PoolState {
            id: PoolId([id; 32]), dex: Dex::PumpSwap,
            mint_a: BASE, mint_b: QUOTE,
            reserve_a: ra, reserve_b: rb, fee_bps: 25, slot: 1,
        }
    }

    #[test]
    fn finds_a_cycle_between_two_dislocated_pools() {
        let s = PoolStore::new();
        // Pool 1 prices QUOTE cheap; pool 2 prices it dear. Round trip profits.
        let p1 = pool(1, 1_000_000, 1_000_000);
        let p2 = pool(2, 1_300_000, 1_000_000);
        s.upsert(p1);
        s.upsert(p2);

        let found = find_two_pool_cycles(&s, &BASE, &p1, 0);
        assert!(!found.is_empty(), "a 30% dislocation must yield an opportunity");
        let o = &found[0];
        assert_eq!(o.base_mint, BASE);
        assert_eq!(o.quote_mint, QUOTE);
        assert!(o.amount_in > 0);
        assert!(o.gross_profit > 0);
    }

    #[test]
    fn identical_pools_yield_nothing() {
        let s = PoolStore::new();
        let p1 = pool(1, 1_000_000, 1_000_000);
        s.upsert(p1);
        s.upsert(pool(2, 1_000_000, 1_000_000));
        assert!(find_two_pool_cycles(&s, &BASE, &p1, 0).is_empty());
    }

    #[test]
    fn min_profit_threshold_filters_marginal_cycles() {
        let s = PoolStore::new();
        let p1 = pool(1, 1_000_000, 1_000_000);
        let p2 = pool(2, 1_300_000, 1_000_000);
        s.upsert(p1);
        s.upsert(p2);
        let permissive = find_two_pool_cycles(&s, &BASE, &p1, 0);
        let strict = find_two_pool_cycles(&s, &BASE, &p1, u128::MAX);
        assert!(!permissive.is_empty());
        assert!(strict.is_empty(), "an impossible threshold must filter everything");
    }

    #[test]
    fn a_pool_is_never_arbitraged_against_itself() {
        let s = PoolStore::new();
        let p1 = pool(1, 1_000_000, 1_300_000);
        s.upsert(p1);
        assert!(find_two_pool_cycles(&s, &BASE, &p1, 0).is_empty(), "self-cycle is not an arb");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test -p cb-scanner cycles 2>&1 | tail -20
```

Expected: FAIL — `cannot find function find_two_pool_cycles`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `crates/scanner/src/cycles.rs`:

```rust
//! Two-pool cycle detection.
//!
//! Triggered by an account update: when pool P changes, we only need to re-test
//! cycles that involve P. That keeps the work proportional to update rate rather
//! than to the square of the pool count.

use crate::store::PoolStore;
use cb_core::amm::{cycle_profit, optimal_input, CycleReserves};
use cb_core::types::{Opportunity, PoolState, Pubkey32};

/// Find profitable `base → quote → base` cycles that route through `updated`.
///
/// `min_profit` is in base-mint base units and is compared against **gross** profit;
/// fee and tip deduction happens in the evaluator, not here.
#[must_use]
pub fn find_two_pool_cycles(
    store: &PoolStore,
    base_mint: &Pubkey32,
    updated: &PoolState,
    min_profit: u128,
) -> Vec<Opportunity> {
    let mut out = Vec::new();

    // The updated pool must trade the base mint for it to start a cycle.
    let Some(quote_mint) = updated.other_mint(base_mint) else {
        return out;
    };

    for other in store.pools_trading(&quote_mint) {
        // A pool cannot arbitrage against itself.
        if other.id == updated.id {
            continue;
        }
        // The counterparty pool must return us to the base mint.
        if other.other_mint(&quote_mint) != Some(*base_mint) {
            continue;
        }

        // Leg 1: spend base in `updated`, receive quote.
        let Some(leg1) = updated.reserves_for_input(base_mint) else { continue };
        // Leg 2: spend quote in `other`, receive base.
        let Some(leg2) = other.reserves_for_input(&quote_mint) else { continue };

        let reserves = CycleReserves {
            a_in: leg1.r_in,
            a_out: leg1.r_out,
            b_in: leg2.r_in,
            b_out: leg2.r_out,
            fee_a_bps: updated.fee_bps,
            fee_b_bps: other.fee_bps,
        };

        let Some(amount_in) = optimal_input(&reserves) else { continue };
        let Some(gross_profit) = cycle_profit(&reserves, amount_in) else { continue };

        if gross_profit < min_profit || gross_profit == 0 {
            continue;
        }

        out.push(Opportunity {
            pool_buy: updated.id,
            pool_sell: other.id,
            base_mint: *base_mint,
            quote_mint,
            amount_in,
            gross_profit,
            slot: updated.slot.max(other.slot),
        });
    }

    out
}
```

Add to `crates/scanner/src/lib.rs`:

```rust
pub mod cycles;
```

- [ ] **Step 4: Run test to verify it passes**

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/cryptobot && cd /mnt/d/Dev/Quant/cryptobot && cargo test 2>&1 | tail -20
```

Expected: PASS — all crates, 22 tests total.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(scanner): two-pool cycle detection wired to verified amm math"
```

---

## Remaining Phase 1 tasks (to be detailed after Task 6 review)

Tasks 7–10 are deliberately not expanded yet: their design depends on what Tasks 4–6
reveal about real account layouts and update rates. Expanding them now would be guesswork
dressed as a plan.

- **Task 7: Raydium AMM v4 decoder.** The one with the trap — reserves come from the two
  vault token accounts **minus** `need_take_pnl_coin`/`need_take_pnl_pc`, not from
  `AmmInfo`. Requires subscribing to three accounts per pool. Needs a real mainnet
  fixture to test against.
- **Task 8: WebSocket feed.** `accountSubscribe` with reconnect/backoff, slot tracking,
  and explicit metrics for dropped/coalesced updates — the measurement-bias problem from
  research finding R5 must be *instrumented*, not assumed away.
- **Task 9: Axum server + lossy broadcast.** Bounded `tokio::sync::broadcast`; a slow
  dashboard client must never apply backpressure to the scanner.
- **Task 10: Next.js dashboard.** Live opportunity table (TanStack Virtual), pool price
  grid, staleness and slot-lag indicators.

## Self-Review

**Spec coverage.** Tasks 1–6 cover the design's core, dex, scanner, and ledger crates and
the paper-first / two-switch safety requirement. The dashboard, feed, and server (design
§2, §5) are Tasks 8–10, listed but not expanded. Executor and on-chain program are Phase 3
and out of this plan's scope by design.

**Placeholder scan.** No TBDs in Tasks 1–6; every step has runnable commands and complete
code. Tasks 7–10 are explicitly flagged as not-yet-planned rather than stubbed, which is
the honest form.

**Type consistency.** `PoolState`, `PoolId`, `Pubkey32`, `Reserves`, `Opportunity`,
`Dex` are defined in Task 1 and used with identical names and field types in Tasks 3–6.
`CycleReserves`, `optimal_input`, `cycle_profit` match the existing verified
`crates/core/src/amm.rs` signatures exactly. `Ledger::record_opportunity` takes
`&Opportunity` as produced by `find_two_pool_cycles`.

**One gap accepted deliberately:** Task 6 emits opportunities using gross profit only.
Net-of-tip/fee filtering belongs to the evaluator, which is not in this plan. Until then
the `min_profit_lamports` config acts as a crude proxy, and recorded opportunities must
be read as *gross*. The dashboard must label them as such so no one mistakes a gross
number for a realisable one.
