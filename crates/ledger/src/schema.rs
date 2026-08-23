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
            sweep_us             INTEGER NOT NULL,
            -- Best edge among cycles deep enough to absorb the run's capital. NULL
            -- means no cycle qualified, which is a measurement, not a zero.
            -- `best_edge_bps` beside it is the raw marginal maximum over every cycle
            -- regardless of depth: a diagnostic, and routinely much larger.
            tradeable_edge_bps   REAL,
            -- Recorded beside the edge so the tradeable route decomposes the same way
            -- the marginal one does: edge = dislocation - fees. Mixing a tradeable
            -- edge with a marginal dislocation would print a sum that does not add up.
            tradeable_dislocation_bps REAL,
            tradeable_fee_bps    REAL,
            tradeable_depth_usd  REAL,
            tradeable_route      TEXT    NOT NULL DEFAULT '',
            -- Pools left out of this sweep for lagging too far behind.
            stale_excluded       INTEGER NOT NULL DEFAULT 0,
            -- 1 once cycle depth was measured at all. Rows written before that
            -- cannot tell nothing-was-tradeable apart from we-never-looked, so they
            -- are excluded from tradeable statistics rather than counted as zero.
            -- `clearing` also changed meaning here: it now requires depth as well
            -- as a positive edge.
            depth_measured       INTEGER NOT NULL DEFAULT 0
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
            skipped_reason        TEXT,
            -- Identity of the loop itself, invariant to which mint it was entered at.
            -- The printed `route` is not that identity: one round trip shows up under
            -- two routes, one per entry point, and grouping on it counts it twice.
            cycle_key             TEXT,
            -- What this same opportunity would have paid an account of a given size.
            -- Profit is concave in size, so these are separate measurements rather than
            -- one number rescaled: `size_usd` is what *this* run's book reached, and the
            -- ladder is what a bigger one would have. NULL means the rung was never
            -- measured, which is not the same as measuring zero.
            profit_at_100_usd     REAL,
            profit_at_1k_usd      REAL,
            profit_at_10k_usd     REAL
        );
        CREATE INDEX IF NOT EXISTS idx_fill_at ON paper_fills(at);
        CREATE INDEX IF NOT EXISTS idx_fill_net ON paper_fills(net_usd);
        ",
    )?;

    // Columns that arrived after runs were already collecting. Added in place rather
    // than by rebuilding, so a ledger mid-flight keeps its history; old rows fall back
    // to the older, coarser meaning, which each column's comment states.
    //
    // `cycle_key`: one loop entered at two mints is one opportunity, not two.
    add_column_if_missing(conn, "paper_fills", "cycle_key", "TEXT")?;
    // The depth-qualified headline, and the staleness guard's count.
    add_column_if_missing(conn, "sweeps", "tradeable_edge_bps", "REAL")?;
    add_column_if_missing(conn, "sweeps", "tradeable_dislocation_bps", "REAL")?;
    add_column_if_missing(conn, "sweeps", "tradeable_fee_bps", "REAL")?;
    add_column_if_missing(conn, "sweeps", "tradeable_depth_usd", "REAL")?;
    add_column_if_missing(conn, "sweeps", "tradeable_route", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "sweeps", "stale_excluded", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "sweeps", "depth_measured", "INTEGER NOT NULL DEFAULT 0")?;
    // The capital ladder. Nullable on purpose: rows from before it existed must read as
    // "not measured" rather than "this opportunity was worth nothing at $100", which is
    // a claim the older run never made.
    add_column_if_missing(conn, "paper_fills", "profit_at_100_usd", "REAL")?;
    add_column_if_missing(conn, "paper_fills", "profit_at_1k_usd", "REAL")?;
    add_column_if_missing(conn, "paper_fills", "profit_at_10k_usd", "REAL")?;
    // Slots between the freshest and stalest leg of the loop. A dislocation is a claim
    // that two venues disagree at one moment; this is how far from that the claim was.
    // Non-zero means part of the "gap" is the market having moved between two
    // observations rather than two venues disagreeing, and you cannot trade against a
    // price that has already gone. NULL for rows written before it was measured —
    // never zero, which would assert simultaneity nobody checked.
    add_column_if_missing(conn, "paper_fills", "slot_spread", "INTEGER")?;

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_fill_cycle ON paper_fills(cycle_key, id);",
    )?;
    Ok(())
}

/// Add a column to an existing table, if a ledger from an older build lacks it.
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, and running the `ALTER` unconditionally
/// fails on every start after the first. Checking `pragma_table_info` first keeps the
/// migration idempotent, so an existing run's history survives a schema change instead
/// of forcing a rebuild that would throw the measurement away.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let present: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
        [column],
        |r| r.get(0),
    )?;
    if present == 0 {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}
