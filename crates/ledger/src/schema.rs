//! Schema migrations. `u128` amounts are stored as TEXT because SQLite's INTEGER is
//! 64-bit and token base units can exceed it; storing them as decimal strings keeps
//! them exact, and they can still be ordered with CAST where needed.
//!
//! # What is worth recording
//!
//! `paper_fills` holds the cycles that cleared. `sweeps` holds a sample of the market
//! *whether or not anything cleared* — and that is the more valuable table. A run that
//! records only its wins can tell you the average size of a win but nothing about how
//! often one exists, and "how often" is the entire question when the answer is
//! measured in fractions of a cent. Sampling the distance-to-profitable on a fixed
//! cadence gives a distribution instead of an anecdote.

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

        -- One row per sampled sweep of the whole cycle graph, clearing or not.
        CREATE TABLE IF NOT EXISTS sweeps (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            at                   TEXT    NOT NULL DEFAULT (datetime('now')),
            slot                 INTEGER NOT NULL,
            evaluated            INTEGER NOT NULL,
            clearing             INTEGER NOT NULL,
            best_edge_bps        REAL    NOT NULL,
            best_dislocation_bps REAL    NOT NULL,
            best_fee_bps         REAL    NOT NULL,
            best_route           TEXT    NOT NULL,
            best_venues          TEXT    NOT NULL,
            best_hops            INTEGER NOT NULL,
            best_depth_usd       REAL    NOT NULL,
            sol_price_usd        REAL    NOT NULL,
            pools_ready          INTEGER NOT NULL,
            sweep_us             INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_sweep_at ON sweeps(at);
        CREATE INDEX IF NOT EXISTS idx_sweep_edge ON sweeps(best_edge_bps);

        -- One row per cycle that cleared its own fees, taken or not.
        CREATE TABLE IF NOT EXISTS paper_fills (
            id                    INTEGER PRIMARY KEY AUTOINCREMENT,
            at                    TEXT    NOT NULL DEFAULT (datetime('now')),
            slot                  INTEGER NOT NULL,
            route                 TEXT    NOT NULL,
            venues                TEXT    NOT NULL,
            hops                  INTEGER NOT NULL,
            edge_bps              REAL    NOT NULL,
            dislocation_bps       REAL    NOT NULL,
            fee_bps               REAL    NOT NULL,
            size_usd              REAL    NOT NULL,
            optimal_size_usd      REAL    NOT NULL,
            gross_usd             REAL    NOT NULL,
            profit_at_optimal_usd REAL    NOT NULL,
            tip_usd               REAL    NOT NULL,
            net_usd               REAL    NOT NULL,
            taken                 INTEGER NOT NULL,
            skipped_reason        TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_fill_at ON paper_fills(at);
        CREATE INDEX IF NOT EXISTS idx_fill_net ON paper_fills(net_usd);
        ",
    )?;
    Ok(())
}
