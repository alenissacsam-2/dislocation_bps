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
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM opportunities", [], |r| r.get(0))?)
    }
}

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

    #[test]
    fn u128_amounts_survive_the_round_trip_exactly() {
        // The reason amounts are TEXT: SQLite INTEGER is 64-bit and would silently
        // truncate a large token amount into a wrong-but-plausible number.
        let l = Ledger::open_in_memory().unwrap();
        let mut o = sample();
        o.amount_in = u128::MAX;
        o.gross_profit = u64::MAX as u128 + 1;
        l.record_opportunity(&o).unwrap();

        let (amt, profit): (String, String) = l
            .conn
            .query_row(
                "SELECT amount_in, gross_profit FROM opportunities LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(amt.parse::<u128>().unwrap(), u128::MAX);
        assert_eq!(profit.parse::<u128>().unwrap(), u64::MAX as u128 + 1);
    }
}
