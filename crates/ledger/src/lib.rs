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

    /// Record one sampled sweep of the cycle graph.
    pub fn record_sweep(&self, s: &SweepSample) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sweeps
             (slot, evaluated, clearing, best_edge_bps, best_dislocation_bps, best_fee_bps,
              best_route, best_venues, best_hops, best_depth_usd, sol_price_usd,
              pools_ready, sweep_us)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            rusqlite::params![
                s.slot as i64,
                s.evaluated as i64,
                s.clearing as i64,
                s.best_edge_bps,
                s.best_dislocation_bps,
                s.best_fee_bps,
                s.best_route,
                s.best_venues,
                s.best_hops as i64,
                s.best_depth_usd,
                s.sol_price_usd,
                s.pools_ready as i64,
                s.sweep_us as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record one cycle that cleared its own fees, taken or not.
    pub fn record_fill(&self, f: &FillRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO paper_fills
             (slot, route, venues, hops, edge_bps, dislocation_bps, fee_bps, size_usd,
              optimal_size_usd, gross_usd, profit_at_optimal_usd, tip_usd, net_usd,
              taken, skipped_reason)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![
                f.slot as i64,
                f.route,
                f.venues,
                f.hops as i64,
                f.edge_bps,
                f.dislocation_bps,
                f.fee_bps,
                f.size_usd,
                f.optimal_size_usd,
                f.gross_usd,
                f.profit_at_optimal_usd,
                f.tip_usd,
                f.net_usd,
                i64::from(f.taken),
                f.skipped_reason.as_deref(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Everything the run has learned so far, in one struct.
    ///
    /// Reads inside a deferred transaction so every figure describes the same instant.
    /// Without it the counts come from separate snapshots of a database the bot is
    /// still writing to, and a report can claim more clearing samples than samples —
    /// which is exactly the kind of impossible number that makes a reader stop
    /// trusting the rest of the page.
    pub fn summary(&self) -> Result<Summary> {
        let _guard = self.conn.unchecked_transaction()?;
        type Agg = (i64, Option<String>, Option<String>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>);
        let (samples, first_at, last_at, mean_edge, best_edge, mean_gap, best_gap, mean_fee): Agg =
            self.conn.query_row(
                "SELECT COUNT(*), MIN(at), MAX(at), AVG(best_edge_bps), MAX(best_edge_bps),
                        AVG(best_dislocation_bps), MAX(best_dislocation_bps), AVG(best_fee_bps)
                 FROM sweeps",
                [],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?,
                        r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
                    ))
                },
            )?;

        let clearing_samples: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sweeps WHERE best_edge_bps > 0", [], |r| r.get(0))?;

        type Fills = (i64, i64, Option<f64>, Option<f64>, Option<f64>, Option<f64>);
        let (fills, taken, gross, net, best_net, best_at_optimal): Fills = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(taken),0), SUM(gross_usd), SUM(net_usd),
                    MAX(net_usd), MAX(profit_at_optimal_usd)
             FROM paper_fills",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;

        let realised_net: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(net_usd),0) FROM paper_fills WHERE taken = 1",
            [],
            |r| r.get(0),
        )?;

        Ok(Summary {
            samples: samples as u64,
            clearing_samples: clearing_samples as u64,
            first_at,
            last_at,
            mean_edge_bps: mean_edge.unwrap_or(0.0),
            best_edge_bps: best_edge.unwrap_or(0.0),
            mean_dislocation_bps: mean_gap.unwrap_or(0.0),
            best_dislocation_bps: best_gap.unwrap_or(0.0),
            mean_fee_bps: mean_fee.unwrap_or(0.0),
            fills: fills as u64,
            taken: taken as u64,
            gross_usd: gross.unwrap_or(0.0),
            net_usd: net.unwrap_or(0.0),
            realised_net_usd: realised_net,
            best_fill_net_usd: best_net.unwrap_or(0.0),
            best_profit_at_optimal_usd: best_at_optimal.unwrap_or(0.0),
        })
    }

    /// Percentiles of what a clearing opportunity was worth, in USD.
    ///
    /// `at_optimal` is the whole pie: what the cycle would pay someone with unlimited
    /// capital. `taken_net` is our slice after tip and base fee. The distance between
    /// them is the price of a $5 account, and the absolute size of `at_optimal` is the
    /// answer to whether the strategy is worth running at all — a route that pays
    /// eight cents at infinite size pays nothing at any size worth having.
    ///
    /// Percentiles rather than a mean because these distributions are long-tailed: one
    /// unusually wide moment moves an average and tells you nothing about a typical one.
    pub fn fill_percentiles(&self) -> Result<FillPercentiles> {
        let _guard = self.conn.unchecked_transaction()?;
        let p = |col: &str, filter: &str, q: f64| -> Result<f64> {
            let n: i64 = self.conn.query_row(
                &format!("SELECT COUNT(*) FROM paper_fills {filter}"),
                [],
                |r| r.get(0),
            )?;
            if n == 0 {
                return Ok(0.0);
            }
            let offset = (((n - 1) as f64) * q).round() as i64;
            Ok(self.conn.query_row(
                &format!(
                    "SELECT {col} FROM paper_fills {filter} ORDER BY {col} LIMIT 1 OFFSET ?1"
                ),
                [offset],
                |r| r.get(0),
            )?)
        };
        // Both rows over the *same* trades. Taking the optimum over every detection
        // and the net over only the taken ones would put two different populations in
        // adjacent rows, and the gap between them would measure nothing.
        Ok(FillPercentiles {
            at_optimal_p50: p("profit_at_optimal_usd", "WHERE taken = 1", 0.50)?,
            at_optimal_p90: p("profit_at_optimal_usd", "WHERE taken = 1", 0.90)?,
            at_optimal_p99: p("profit_at_optimal_usd", "WHERE taken = 1", 0.99)?,
            taken_net_p50: p("net_usd", "WHERE taken = 1", 0.50)?,
            taken_net_p90: p("net_usd", "WHERE taken = 1", 0.90)?,
            taken_net_p99: p("net_usd", "WHERE taken = 1", 0.99)?,
            size_p50: p("size_usd", "WHERE taken = 1", 0.50)?,
        })
    }

    /// Share of all detections contributed by the single most frequent route.
    ///
    /// Reported because a headline average over a set dominated by one illiquid pair
    /// describes that pair, not the market. If this is high, every other figure needs
    /// reading as "mostly one route".
    pub fn concentration(&self) -> Result<(String, f64)> {
        let total: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM paper_fills", [], |r| r.get(0))?;
        if total == 0 {
            return Ok((String::new(), 0.0));
        }
        let (route, n): (String, i64) = self.conn.query_row(
            "SELECT route, COUNT(*) c FROM paper_fills GROUP BY route ORDER BY c DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((route, n as f64 / total as f64))
    }

    /// Median SOL price across the run, from the recorded sweeps.
    ///
    /// Transaction costs are denominated in SOL, so quoting them in dollars needs a
    /// price. Taking it from the ledger rather than a constant means a report stays
    /// correct when read months later against a different market.
    pub fn median_sol_price(&self) -> Result<f64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sweeps WHERE sol_price_usd > 0",
            [],
            |r| r.get(0),
        )?;
        if n == 0 {
            return Ok(0.0);
        }
        Ok(self.conn.query_row(
            "SELECT sol_price_usd FROM sweeps WHERE sol_price_usd > 0
             ORDER BY sol_price_usd LIMIT 1 OFFSET ?1",
            [(n - 1) / 2],
            |r| r.get(0),
        )?)
    }

    /// Hours spanned by the recorded sweeps, from the ledger's own timestamps.
    pub fn hours_observed(&self) -> Result<f64> {
        let secs: Option<f64> = self.conn.query_row(
            "SELECT (julianday(MAX(at)) - julianday(MIN(at))) * 86400.0 FROM sweeps",
            [],
            |r| r.get(0),
        )?;
        Ok(secs.unwrap_or(0.0) / 3600.0)
    }

    /// Collapse repeated detections of the same standing gap into **episodes**.
    ///
    /// # The mistake this exists to correct
    ///
    /// The sweep runs five times a second and re-detects whatever is currently
    /// mispriced. A gap that stands for an hour therefore produces eighteen thousand
    /// rows in `paper_fills` — and summing their profit says the hour was worth
    /// eighteen thousand trades. It was worth one opportunity, harvested until it
    /// closed.
    ///
    /// An episode is a maximal run of consecutive detections of the same route with
    /// less than `gap_secs` between them. Its value is capped at the best single
    /// detection inside it, because taking an arbitrage is what removes it: the
    /// optimum *is* the whole pie, by definition, and you cannot eat it twice.
    ///
    /// This is still an upper bound — it ignores that our own trade moves the price,
    /// and it assumes we win every race — but it is an upper bound on the right
    /// quantity, which the raw sum is not.
    pub fn episodes(&self, gap_secs: f64) -> Result<EpisodeStats> {
        let _guard = self.conn.unchecked_transaction()?;
        let mut stmt = self.conn.prepare(
            "WITH d AS (
                 SELECT id, route, net_usd, profit_at_optimal_usd, taken,
                        (julianday(at) - julianday(LAG(at) OVER w)) * 86400.0 AS gap_s
                 FROM paper_fills
                 WINDOW w AS (PARTITION BY route ORDER BY id)
             ),
             m AS (
                 SELECT *, CASE WHEN gap_s IS NULL OR gap_s > ?1 THEN 1 ELSE 0 END AS brk FROM d
             ),
             e AS (
                 SELECT *, SUM(brk) OVER (PARTITION BY route ORDER BY id) AS ep FROM m
             )
             SELECT route, MAX(profit_at_optimal_usd), MAX(net_usd), MAX(taken), COUNT(*)
             FROM e GROUP BY route, ep",
        )?;
        let rows = stmt.query_map([gap_secs], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, i64>(3)? == 1,
                r.get::<_, i64>(4)? as u64,
            ))
        })?;

        let mut st = EpisodeStats::default();
        let mut nets: Vec<f64> = Vec::new();
        for row in rows {
            let (_route, pie, best_net, taken, detections) = row?;
            st.count += 1;
            st.detections += detections;
            st.total_pie_usd += pie;
            st.longest_detections = st.longest_detections.max(detections);
            if taken && best_net > 0.0 {
                st.taken += 1;
                st.total_net_usd += best_net;
                nets.push(best_net);
            }
        }
        nets.sort_by(f64::total_cmp);
        st.median_net_usd = nets.get(nets.len() / 2).copied().unwrap_or(0.0);
        st.best_net_usd = nets.last().copied().unwrap_or(0.0);
        Ok(st)
    }

    /// The routes that clear most often, with what they were worth.
    pub fn top_routes(&self, limit: usize) -> Result<Vec<RouteStat>> {
        let mut stmt = self.conn.prepare(
            "SELECT route, venues, COUNT(*), AVG(edge_bps), SUM(net_usd), MAX(net_usd)
             FROM paper_fills GROUP BY route, venues ORDER BY COUNT(*) DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(RouteStat {
                route: r.get(0)?,
                venues: r.get(1)?,
                fills: r.get::<_, i64>(2)? as u64,
                mean_edge_bps: r.get(3)?,
                net_usd: r.get(4)?,
                best_net_usd: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// How the distance-to-profitable was distributed, as counts per bps bucket.
    ///
    /// Buckets are inclusive of their lower bound. The point of a histogram rather
    /// than a mean: a market that sits at -2 bps all day and one that alternates
    /// between -20 and +16 have the same mean and completely different economics.
    pub fn edge_histogram(&self, edges: &[f64]) -> Result<Vec<(f64, f64, u64)>> {
        let mut out = Vec::new();
        for w in edges.windows(2) {
            let n: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sweeps WHERE best_edge_bps >= ?1 AND best_edge_bps < ?2",
                rusqlite::params![w[0], w[1]],
                |r| r.get(0),
            )?;
            out.push((w[0], w[1], n as u64));
        }
        Ok(out)
    }
}

/// One sampled sweep of the cycle graph.
#[derive(Debug, Clone, Default)]
pub struct SweepSample {
    pub slot: u64,
    pub evaluated: u64,
    pub clearing: u64,
    pub best_edge_bps: f64,
    pub best_dislocation_bps: f64,
    pub best_fee_bps: f64,
    pub best_route: String,
    pub best_venues: String,
    pub best_hops: usize,
    pub best_depth_usd: f64,
    pub sol_price_usd: f64,
    pub pools_ready: usize,
    pub sweep_us: u64,
}

/// One cycle that cleared its own fees.
#[derive(Debug, Clone, Default)]
pub struct FillRecord {
    pub slot: u64,
    pub route: String,
    pub venues: String,
    pub hops: usize,
    pub edge_bps: f64,
    pub dislocation_bps: f64,
    pub fee_bps: f64,
    pub size_usd: f64,
    pub optimal_size_usd: f64,
    pub gross_usd: f64,
    pub profit_at_optimal_usd: f64,
    pub tip_usd: f64,
    pub net_usd: f64,
    pub taken: bool,
    pub skipped_reason: Option<String>,
}

/// Everything a run has measured.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub samples: u64,
    /// Samples in which some route was actually profitable.
    pub clearing_samples: u64,
    pub first_at: Option<String>,
    pub last_at: Option<String>,
    pub mean_edge_bps: f64,
    pub best_edge_bps: f64,
    pub mean_dislocation_bps: f64,
    pub best_dislocation_bps: f64,
    pub mean_fee_bps: f64,
    pub fills: u64,
    pub taken: u64,
    pub gross_usd: f64,
    pub net_usd: f64,
    /// Net across only the fills the paper trader would actually have taken.
    pub realised_net_usd: f64,
    pub best_fill_net_usd: f64,
    /// The best any single opportunity was worth to someone with unlimited capital.
    pub best_profit_at_optimal_usd: f64,
}

impl Summary {
    /// Fraction of sampled moments in which *something* was profitable.
    #[must_use]
    pub fn clearing_rate(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.clearing_samples as f64 / self.samples as f64
        }
    }
}

/// Distinct arbitrage opportunities, after collapsing repeated detections.
#[derive(Debug, Clone, Default)]
pub struct EpisodeStats {
    /// Distinct opportunities.
    pub count: u64,
    /// Of those, ones the paper trader would have acted on.
    pub taken: u64,
    /// Detections that collapsed into them. The ratio to `count` is how badly a raw
    /// row count overstates the number of trades available.
    pub detections: u64,
    /// Longest single episode, in detections. A large value means one standing gap
    /// nobody else bothered to close.
    pub longest_detections: u64,
    /// Sum over episodes of what each was worth at unlimited capital.
    pub total_pie_usd: f64,
    /// Sum over episodes of what we would have kept, once each.
    pub total_net_usd: f64,
    pub median_net_usd: f64,
    pub best_net_usd: f64,
}

impl EpisodeStats {
    /// How many times the raw detection count overstates the number of opportunities.
    #[must_use]
    pub fn inflation(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.detections as f64 / self.count as f64
        }
    }
}

/// What a clearing opportunity was worth, at several points in the distribution.
#[derive(Debug, Clone, Default)]
pub struct FillPercentiles {
    /// Profit the cycle would pay at unlimited capital.
    pub at_optimal_p50: f64,
    pub at_optimal_p90: f64,
    pub at_optimal_p99: f64,
    /// Profit we would actually have kept, after tip and base fee.
    pub taken_net_p50: f64,
    pub taken_net_p90: f64,
    pub taken_net_p99: f64,
    /// Trade size we would actually have used.
    pub size_p50: f64,
}

/// Per-route aggregate over the fills.
#[derive(Debug, Clone)]
pub struct RouteStat {
    pub route: String,
    pub venues: String,
    pub fills: u64,
    pub mean_edge_bps: f64,
    pub net_usd: f64,
    pub best_net_usd: f64,
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

    fn sweep(edge: f64) -> SweepSample {
        SweepSample {
            slot: 1,
            evaluated: 400,
            clearing: u64::from(edge > 0.0),
            best_edge_bps: edge,
            best_dislocation_bps: edge + 2.0,
            best_fee_bps: 2.0,
            best_route: "SOL -> USDC -> SOL".into(),
            best_venues: "ORCA 1bp - RAY-CL 1bp".into(),
            best_hops: 2,
            best_depth_usd: 1234.0,
            sol_price_usd: 91.0,
            pools_ready: 83,
            sweep_us: 6000,
        }
    }

    fn fill(net: f64, taken: bool) -> FillRecord {
        FillRecord {
            slot: 1,
            route: "SOL -> USDC -> SOL".into(),
            venues: "ORCA 1bp - RAY-CL 1bp".into(),
            hops: 2,
            edge_bps: 2.5,
            dislocation_bps: 4.5,
            fee_bps: 2.0,
            size_usd: 4.8,
            optimal_size_usd: 24.0,
            gross_usd: net + 0.0011,
            profit_at_optimal_usd: 0.006,
            tip_usd: 0.0007,
            net_usd: net,
            taken,
            skipped_reason: if taken { None } else { Some("net negative after tip".into()) },
        }
    }

    /// The table that matters: a run must record what the market looked like even
    /// when nothing cleared, or it can report the size of a win but never the odds.
    #[test]
    fn sweeps_are_recorded_whether_or_not_anything_cleared() {
        let l = Ledger::open_in_memory().unwrap();
        for e in [-2.0, -1.5, 0.5, -3.0] {
            l.record_sweep(&sweep(e)).unwrap();
        }
        let s = l.summary().unwrap();
        assert_eq!(s.samples, 4);
        assert_eq!(s.clearing_samples, 1);
        assert!((s.clearing_rate() - 0.25).abs() < 1e-9);
        assert!((s.best_edge_bps - 0.5).abs() < 1e-9);
        assert!((s.mean_edge_bps + 1.5).abs() < 1e-9, "mean was {}", s.mean_edge_bps);
        assert!((s.mean_fee_bps - 2.0).abs() < 1e-9);
    }

    #[test]
    fn fills_separate_what_was_taken_from_what_was_merely_seen() {
        let l = Ledger::open_in_memory().unwrap();
        l.record_fill(&fill(0.0002, true)).unwrap();
        l.record_fill(&fill(0.0003, true)).unwrap();
        l.record_fill(&fill(-0.0011, false)).unwrap();

        let s = l.summary().unwrap();
        assert_eq!(s.fills, 3);
        assert_eq!(s.taken, 2);
        assert!((s.realised_net_usd - 0.0005).abs() < 1e-9, "only taken fills count");
        assert!(s.net_usd < s.realised_net_usd, "the skipped loser drags the all-in total");
        assert!((s.best_profit_at_optimal_usd - 0.006).abs() < 1e-9);
    }

    #[test]
    fn top_routes_ranks_by_how_often_a_route_clears() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..3 {
            l.record_fill(&fill(0.0002, true)).unwrap();
        }
        let mut other = fill(0.01, true);
        other.route = "SOL -> USDT -> SOL".into();
        l.record_fill(&other).unwrap();

        let top = l.top_routes(5).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].fills, 3, "the frequent route ranks first");
        assert!(top[1].best_net_usd > top[0].best_net_usd, "the rarer one paid more");
    }

    /// A mean hides the shape. Two markets with the same average distance to
    /// profitable can have completely different economics, and only one of them is
    /// worth trading.
    #[test]
    fn the_histogram_distinguishes_a_flat_market_from_a_volatile_one() {
        let flat = Ledger::open_in_memory().unwrap();
        let swingy = Ledger::open_in_memory().unwrap();
        for _ in 0..10 {
            flat.record_sweep(&sweep(-2.0)).unwrap();
        }
        for i in 0..10 {
            swingy.record_sweep(&sweep(if i % 2 == 0 { -20.0 } else { 16.0 })).unwrap();
        }
        let (a, b) = (flat.summary().unwrap(), swingy.summary().unwrap());
        assert!((a.mean_edge_bps - b.mean_edge_bps).abs() < 1e-9, "same mean, by construction");

        let edges = [-100.0, -5.0, 0.0, 100.0];
        assert_eq!(flat.edge_histogram(&edges).unwrap()[1].2, 10, "all in [-5, 0)");
        let sw = swingy.edge_histogram(&edges).unwrap();
        assert_eq!(sw[0].2, 5, "half below -5");
        assert_eq!(sw[2].2, 5, "half above zero");
    }

    /// The distribution, not the average, is the deliverable. A run whose mean profit
    /// is dragged up by one wide moment must still report that the median is dust.
    #[test]
    fn percentiles_describe_the_typical_opportunity_not_the_luckiest_one() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..90 {
            l.record_fill(&fill(0.0001, true)).unwrap();
        }
        for _ in 0..10 {
            let mut lucky = fill(1.0, true);
            lucky.profit_at_optimal_usd = 50.0;
            l.record_fill(&lucky).unwrap();
        }

        let p = l.fill_percentiles().unwrap();
        assert!((p.taken_net_p50 - 0.0001).abs() < 1e-9, "median is the typical case");
        assert!(p.taken_net_p99 > 0.5, "the tail is still visible at p99");
        assert!((p.at_optimal_p50 - 0.006).abs() < 1e-9);

        // The mean is a hundred times the median here. Reporting it alone would
        // describe ten lucky moments and none of the ninety ordinary ones.
        let mean = l.summary().unwrap().realised_net_usd / 100.0;
        assert!(mean > p.taken_net_p50 * 50.0, "the mean is misleading here: {mean}");
    }

    /// A headline average over a set dominated by one pair describes that pair.
    #[test]
    fn sol_price_comes_from_the_run_not_a_constant() {
        let l = Ledger::open_in_memory().unwrap();
        for p in [80.0, 91.0, 120.0] {
            let mut s = sweep(-1.0);
            s.sol_price_usd = p;
            l.record_sweep(&s).unwrap();
        }
        assert!((l.median_sol_price().unwrap() - 91.0).abs() < 1e-9);
    }

    #[test]
    fn concentration_exposes_a_single_route_dominating_the_sample() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..90 {
            l.record_fill(&fill(0.0001, true)).unwrap();
        }
        let mut other = fill(0.0001, true);
        other.route = "SOL -> USDT -> SOL".into();
        for _ in 0..10 {
            l.record_fill(&other).unwrap();
        }
        let (route, share) = l.concentration().unwrap();
        assert_eq!(route, "SOL -> USDC -> SOL");
        assert!((share - 0.9).abs() < 1e-9);
    }

    /// The correction that matters most. A gap that stands still gets re-detected on
    /// every sweep; counting each detection as a trade turns one opportunity worth
    /// half a cent into a hundred dollars an hour that does not exist.
    #[test]
    fn one_standing_gap_is_one_opportunity_not_a_thousand_trades() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..1000 {
            l.record_fill(&fill(0.005, true)).unwrap();
        }

        let raw = l.summary().unwrap();
        assert!((raw.realised_net_usd - 5.0).abs() < 1e-6, "the naive sum says $5");

        let e = l.episodes(2.0).unwrap();
        assert_eq!(e.count, 1, "it was one gap the whole time");
        assert_eq!(e.detections, 1000);
        assert!(e.inflation() > 900.0, "the raw count overstated by ~1000x");
        assert!((e.total_net_usd - 0.005).abs() < 1e-9, "worth one fill, not a thousand");
    }

    /// Two routes that clear at the same time are two opportunities, not one.
    #[test]
    fn distinct_routes_are_distinct_episodes() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..10 {
            l.record_fill(&fill(0.001, true)).unwrap();
            let mut other = fill(0.002, true);
            other.route = "SOL -> USDT -> SOL".into();
            l.record_fill(&other).unwrap();
        }
        let e = l.episodes(2.0).unwrap();
        assert_eq!(e.count, 2);
        assert!((e.total_net_usd - 0.003).abs() < 1e-9, "best of each, summed once");
    }

    /// An episode is worth its best moment, not the sum of its moments — taking an
    /// arbitrage is what removes it.
    #[test]
    fn an_episode_is_worth_its_best_detection() {
        let l = Ledger::open_in_memory().unwrap();
        for net in [0.001, 0.004, 0.002] {
            l.record_fill(&fill(net, true)).unwrap();
        }
        let e = l.episodes(2.0).unwrap();
        assert_eq!(e.count, 1);
        assert!((e.total_net_usd - 0.004).abs() < 1e-9);
        assert!((e.best_net_usd - 0.004).abs() < 1e-9);
    }

    /// Detections we would not have acted on must not count toward the total, but
    /// they still count as an opportunity having existed.
    #[test]
    fn an_episode_nobody_would_have_taken_contributes_no_money() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..5 {
            l.record_fill(&fill(-0.001, false)).unwrap();
        }
        let e = l.episodes(2.0).unwrap();
        assert_eq!(e.count, 1);
        assert_eq!(e.taken, 0);
        assert_eq!(e.total_net_usd, 0.0);
    }

    #[test]
    fn an_empty_ledger_summarises_to_zeroes_rather_than_erroring() {
        let l = Ledger::open_in_memory().unwrap();
        let s = l.summary().unwrap();
        assert_eq!(s.samples, 0);
        assert_eq!(s.clearing_rate(), 0.0);
        assert!(l.top_routes(5).unwrap().is_empty());
        assert_eq!(l.concentration().unwrap().1, 0.0);
        assert_eq!(l.episodes(2.0).unwrap().count, 0);
        assert_eq!(l.median_sol_price().unwrap(), 0.0);
        assert_eq!(l.episodes(2.0).unwrap().inflation(), 0.0);
        assert_eq!(l.hours_observed().unwrap(), 0.0);
        assert_eq!(l.fill_percentiles().unwrap().taken_net_p50, 0.0);
    }
}

