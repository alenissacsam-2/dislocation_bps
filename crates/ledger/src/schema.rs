//! Schema migrations. `u128` amounts are stored as TEXT because SQLite's INTEGER is
//! 64-bit and token base units can exceed it; storing them as decimal strings keeps
//! them exact, and they can still be ordered with CAST where needed.

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
