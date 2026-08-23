//! SQLite persistence. Every detected opportunity is recorded — including ones we
//! skip and why — because the skipped set is the most valuable research output of
//! the paper-trading phase.

mod schema;

use anyhow::Result;
use cb_core::types::Opportunity;
use rusqlite::Connection;

/// Account sizes the run prices every opportunity against, in USD.
///
/// # Why a ladder and not a single book size
///
/// The question "would more capital help?" cannot be answered by a run at one book
/// size. Profit is concave in trade size and capped by cycle depth, so what a $5 run
/// observes says nothing reliable about a $100 one — the honest way to find out is to
/// price the *same* opportunity at several sizes as it happens, and record all of them.
///
/// $100 is the rung that matters for a real starting book; the two above it exist to
/// show where the depth ceiling bites, because a ladder that stops climbing is the
/// measurement that says borrowed capital would not have helped.
pub const CAPITAL_LADDER_USD: [f64; 3] = [100.0, 1_000.0, 10_000.0];

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

    /// Open an existing ledger for reading only.
    ///
    /// For readers that are not the run: the dashboard, a report, anything that must be
    /// unable to alter the measurement. Deliberately does **not** migrate — a reader
    /// that can rewrite the schema is a reader that can corrupt a run in flight — and
    /// deliberately fails rather than creating the file, because a missing ledger is a
    /// fact worth surfacing and an empty new one looks like a run that found nothing.
    pub fn open_read_only(path: &str) -> Result<Self> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
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
              pools_ready, sweep_us, tradeable_edge_bps, tradeable_dislocation_bps,
              tradeable_fee_bps, tradeable_depth_usd, tradeable_route, stale_excluded,
              depth_measured)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
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
                s.tradeable_edge_bps,
                s.tradeable_dislocation_bps,
                s.tradeable_fee_bps,
                s.tradeable_depth_usd,
                s.tradeable_route,
                s.stale_excluded as i64,
                i64::from(s.depth_measured),
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
              taken, skipped_reason, cycle_key,
              profit_at_100_usd, profit_at_1k_usd, profit_at_10k_usd, slot_spread)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
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
                f.cycle_key,
                f.profit_at_capital_usd.map(|l| l[0]),
                f.profit_at_capital_usd.map(|l| l[1]),
                f.profit_at_capital_usd.map(|l| l[2]),
                f.slot_spread.map(|s| s as i64),
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
        type Agg = (i64, Option<String>, Option<String>, Option<f64>);
        let (samples, first_at, last_at, marginal_best): Agg = self.conn.query_row(
            "SELECT COUNT(*), MIN(at), MAX(at), MAX(best_edge_bps) FROM sweeps",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

        // Everything headline comes from the depth-qualified set. Rows written before
        // depth was measured are excluded rather than folded in: for those we cannot
        // tell "nothing was tradeable" from "we never looked", and counting an unknown
        // as a zero is the kind of quiet substitution this whole table exists to avoid.
        //
        // SQLite's aggregates skip NULLs, so the means below are over the samples that
        // *had* a tradeable cycle. `tradeable_samples` reports that coverage next to
        // them, because a mean taken over the good moments only is a biased number
        // unless the reader can see how many moments it left out.
        type Trade = (i64, i64, i64, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>);
        let (
            depth_samples,
            tradeable_samples,
            clearing_samples,
            mean_edge,
            best_edge,
            mean_gap,
            best_gap,
            mean_fee,
        ): Trade = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(tradeable_edge_bps IS NOT NULL), 0),
                    COALESCE(SUM(tradeable_edge_bps > 0), 0),
                    AVG(tradeable_edge_bps), MAX(tradeable_edge_bps),
                    AVG(tradeable_dislocation_bps), MAX(tradeable_dislocation_bps),
                    AVG(tradeable_fee_bps)
             FROM sweeps WHERE depth_measured = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?,
                    r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
                ))
            },
        )?;

        // Samples whose best rate had no size behind it at all. This is the size of
        // the gap between the two searches, and it is information rather than noise:
        // it says how much of the visible book is untouchable at this capital.
        let untradeable_leader_samples: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sweeps
             WHERE depth_measured = 1 AND best_edge_bps > 0 AND tradeable_edge_bps IS NULL",
            [],
            |r| r.get(0),
        )?;

        let stale_excluded_max: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(stale_excluded), 0) FROM sweeps",
            [],
            |r| r.get(0),
        )?;

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
            depth_samples: depth_samples as u64,
            tradeable_samples: tradeable_samples as u64,
            clearing_samples: clearing_samples as u64,
            untradeable_leader_samples: untradeable_leader_samples as u64,
            first_at,
            last_at,
            mean_edge_bps: mean_edge.unwrap_or(0.0),
            best_edge_bps: best_edge.unwrap_or(0.0),
            marginal_best_edge_bps: marginal_best.unwrap_or(0.0),
            stale_excluded_max: stale_excluded_max as u64,
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
    pub fn episodes(&self, gap_slots: u64) -> Result<EpisodeStats> {
        let mut st = EpisodeStats::default();
        let mut nets: Vec<f64> = Vec::new();
        for e in self.episode_rows(gap_slots)? {
            st.count += 1;
            st.detections += e.detections;
            st.total_pie_usd += e.pie_usd;
            st.longest_detections = st.longest_detections.max(e.detections);
            st.longest_slots = st.longest_slots.max(e.lifetime_slots);
            if e.taken && e.best_net_usd > 0.0 {
                st.taken += 1;
                st.total_net_usd += e.best_net_usd;
                nets.push(e.best_net_usd);
            }
        }
        nets.sort_by(f64::total_cmp);
        st.median_net_usd = nets.get(nets.len() / 2).copied().unwrap_or(0.0);
        st.best_net_usd = nets.last().copied().unwrap_or(0.0);
        Ok(st)
    }

    /// How long an opportunity survives, against how much it is worth.
    ///
    /// # Why this is the measurement that settles the question
    ///
    /// Every other number here can be argued with — maybe more capital, maybe more
    /// venues, maybe a faster machine. This one closes the argument, because size and
    /// lifetime turn out to be *inversely* coupled: the gaps that would repay real
    /// capital are gone within a slot, and the ones that sit around waiting are worth
    /// a few thousandths of a cent. There is no size at which an opportunity is both
    /// worth taking and still there when you arrive.
    ///
    /// Lifetime is counted in **slots**, not wall-clock. A slot is the chain's own
    /// tick (~400 ms), it is what the ledger already records, and it is immune to the
    /// local clock drifting.
    pub fn survival(&self, gap_slots: u64) -> Result<Vec<SurvivalBand>> {
        // Boundaries in dollars of whole-pie value, with the label for each bucket.
        const BANDS: [(f64, &str); 5] = [
            (0.001, "under $0.001"),
            (0.01, "$0.001 - $0.01"),
            (0.10, "$0.01 - $0.10"),
            (1.00, "$0.10 - $1"),
            (f64::INFINITY, "over $1"),
        ];
        let mut out: Vec<SurvivalBand> = BANDS
            .iter()
            .map(|(_, label)| SurvivalBand { label: (*label).to_string(), ..Default::default() })
            .collect();

        for e in self.episode_rows(gap_slots)? {
            let i = BANDS.iter().position(|(hi, _)| e.pie_usd < *hi).unwrap_or(BANDS.len() - 1);
            let b = &mut out[i];
            b.episodes += 1;
            b.total_slots += e.lifetime_slots;
            b.longest_slots = b.longest_slots.max(e.lifetime_slots);
            b.total_detections += e.detections;
            b.total_capital_usd += e.optimal_size_usd;
        }
        out.retain(|b| b.episodes > 0);
        Ok(out)
    }

    /// One row per episode. The single definition of what an episode *is* — both
    /// [`Self::episodes`] and [`Self::survival`] read it, so they cannot disagree.
    fn episode_rows(&self, gap_slots: u64) -> Result<Vec<EpisodeRow>> {
        let _guard = self.conn.unchecked_transaction()?;
        let mut stmt = self.conn.prepare(
            "WITH k AS (
                 SELECT id, at, slot, net_usd, profit_at_optimal_usd, optimal_size_usd, taken,
                        profit_at_100_usd, profit_at_1k_usd, profit_at_10k_usd, skipped_reason,
                        COALESCE(NULLIF(cycle_key, ''), route || '|' || venues) AS ck
                 FROM paper_fills
             ),
             d AS (
                 SELECT *, slot - LAG(slot) OVER (PARTITION BY ck ORDER BY id) AS gap
                 FROM k
             ),
             m AS (
                 SELECT *, CASE WHEN gap IS NULL OR gap > ?1 THEN 1 ELSE 0 END AS brk FROM d
             ),
             e AS (
                 SELECT *, SUM(brk) OVER (PARTITION BY ck ORDER BY id) AS ep FROM m
             )
             SELECT MAX(profit_at_optimal_usd), MAX(net_usd), MAX(taken), COUNT(*),
                    MAX(slot) - MIN(slot), MAX(optimal_size_usd), MIN(at),
                    MAX(profit_at_100_usd), MAX(profit_at_1k_usd), MAX(profit_at_10k_usd),
                    MAX(skipped_reason LIKE 'contested%')
             FROM e GROUP BY ck, ep ORDER BY MIN(id)",
        )?;
        let rows = stmt.query_map([gap_slots as i64], |r| {
            // The ladder survives as `None` unless every rung was measured. A partially
            // filled ladder would silently read a missing rung as a zero, which is the
            // one substitution this table exists to prevent.
            let ladder = match (
                r.get::<_, Option<f64>>(7)?,
                r.get::<_, Option<f64>>(8)?,
                r.get::<_, Option<f64>>(9)?,
            ) {
                (Some(a), Some(b), Some(c)) => Some([a, b, c]),
                _ => None,
            };
            Ok(EpisodeRow {
                pie_usd: r.get(0)?,
                best_net_usd: r.get(1)?,
                taken: r.get::<_, i64>(2)? == 1,
                detections: r.get::<_, i64>(3)? as u64,
                lifetime_slots: r.get::<_, i64>(4)?.max(0) as u64,
                optimal_size_usd: r.get(5)?,
                first_at: r.get(6)?,
                ladder,
                contested: r.get::<_, Option<i64>>(10)?.unwrap_or(0) == 1,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Whether the contest classifier is measuring competition or only measuring size.
    ///
    /// # The claim being audited
    ///
    /// Opportunities above a profit threshold are declined on the assumption that a
    /// faster searcher has already seen the same cycle and would win the race. That
    /// assumption decides the large majority of the value this instrument ever sees,
    /// and nothing has ever checked it. It is also, as written, a threshold on profit —
    /// so by construction it cannot distinguish "contested" from "big", and the two
    /// only coincide if big opportunities really are the contested ones.
    ///
    /// # What survival proves, and what it does not
    ///
    /// An opportunity still quotable in a later slot was, in that window, taken by
    /// nobody — we are in paper mode, so we did not take it either. Persistence is
    /// therefore direct evidence that the race was not lost. The converse is weaker:
    /// vanishing within the slot could mean a competitor took it *or* that the price
    /// simply moved, and this cannot tell those apart.
    ///
    /// So the honest reading is the **comparison**. If declined opportunities survive
    /// about as often as accepted ones, the threshold is sorting by size and calling it
    /// competition.
    pub fn contest_audit(&self, gap_slots: u64) -> Result<ContestAudit> {
        let mut a = ContestAudit::default();
        for e in self.episode_rows(gap_slots)? {
            let survived = e.lifetime_slots > 0;
            if e.contested {
                a.contested_episodes += 1;
                a.contested_slots += e.lifetime_slots;
                a.declined_usd += e.pie_usd.max(0.0);
                if survived {
                    a.contested_survived += 1;
                    a.declined_but_survived_usd += e.pie_usd.max(0.0);
                }
            } else {
                a.uncontested_episodes += 1;
                a.uncontested_slots += e.lifetime_slots;
                if survived {
                    a.uncontested_survived += 1;
                }
            }
        }
        Ok(a)
    }

    /// Cumulative paper P&L over the life of the run, as a series ready to plot.
    ///
    /// # Why this is built from episodes rather than fills
    ///
    /// A gap that stands for a minute is re-detected on every sweep. Accumulating
    /// `net_usd` across those rows draws a line climbing steadily through money that was
    /// only ever available once, and the steeper that line the more wrong it is. Each
    /// episode contributes its best single detection, exactly once, on the timestamp it
    /// opened — so the curve steps when an opportunity arrives and stays flat when the
    /// same one is merely still visible.
    ///
    /// Three series come back together because the interesting quantity is the distance
    /// between them: `realised` is what this run's book actually reached, the ladder is
    /// what larger books would have reached from the identical opportunities, and
    /// `at_optimal` is the ceiling at any capital.
    ///
    /// `max_points` downsamples for the plot. The last point is always kept, so the
    /// totals a caller reads off the tail are exact rather than whatever the stride
    /// happened to land on.
    pub fn equity_curve(&self, gap_slots: u64, max_points: usize) -> Result<Vec<EquityPoint>> {
        let episodes = self.episode_rows(gap_slots)?;
        let mut curve: Vec<EquityPoint> = Vec::with_capacity(episodes.len());
        let mut acc = EquityPoint::default();
        for e in &episodes {
            acc.episodes += 1;
            acc.at_optimal_usd += e.pie_usd.max(0.0);
            if e.taken && e.best_net_usd > 0.0 {
                acc.realised_usd += e.best_net_usd;
                acc.taken += 1;
            }
            match e.ladder {
                Some(l) => {
                    for (slot, v) in acc.at_capital_usd.iter_mut().zip(l) {
                        *slot += v.max(0.0);
                    }
                    acc.ladder_episodes += 1;
                }
                None => acc.unmeasured_episodes += 1,
            }
            acc.at.clone_from(&e.first_at);
            curve.push(acc.clone());
        }

        if max_points == 0 || curve.len() <= max_points {
            return Ok(curve);
        }
        let stride = curve.len().div_ceil(max_points);
        let last = curve.len() - 1;
        Ok(curve
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % stride == 0 || *i == last)
            .map(|(_, p)| p)
            .collect())
    }

    /// Every opportunity as a point of (what it was worth, how long it lasted).
    ///
    /// This is the plot that carries the project's central finding, and it needs to be
    /// a scatter rather than an average because the relationship is an *inverse* one:
    /// the gaps worth real money are gone within a slot, and the ones that sit around
    /// waiting are worth thousandths of a cent. A mean over both says neither.
    ///
    /// Capped at `limit` points, keeping the most valuable — a scatter of ten thousand
    /// dust episodes hides the handful that decide whether any of this works, and those
    /// are exactly the ones a reader needs to see.
    pub fn episode_scatter(&self, gap_slots: u64, limit: usize) -> Result<Vec<EpisodePoint>> {
        let mut pts: Vec<EpisodePoint> = self
            .episode_rows(gap_slots)?
            .into_iter()
            .map(|e| EpisodePoint {
                pie_usd: e.pie_usd.max(0.0),
                net_usd: if e.taken { e.best_net_usd } else { 0.0 },
                lifetime_slots: e.lifetime_slots,
                optimal_size_usd: e.optimal_size_usd,
                detections: e.detections,
                taken: e.taken,
                contested: e.contested,
            })
            .collect();
        pts.sort_by(|a, b| b.pie_usd.total_cmp(&a.pie_usd));
        pts.truncate(limit);
        Ok(pts)
    }

    /// What this run's opportunities were worth to accounts of several sizes.
    ///
    /// The direct answer to "would a bigger book have made money", measured rather than
    /// extrapolated: every rung prices the *same* episodes, so the gaps between them are
    /// what capital actually buys. A ladder that flattens between two rungs is the
    /// finding — it says depth, not capital, is what the run ran out of.
    pub fn capital_ladder(&self, gap_slots: u64) -> Result<CapitalLadder> {
        let tail = self.equity_curve(gap_slots, 0)?.pop().unwrap_or_default();
        Ok(CapitalLadder {
            rungs: CAPITAL_LADDER_USD.iter().copied().zip(tail.at_capital_usd).collect(),
            at_optimal_usd: tail.at_optimal_usd,
            realised_usd: tail.realised_usd,
            measured_episodes: tail.ladder_episodes,
            unmeasured_episodes: tail.unmeasured_episodes,
        })
    }

    /// The run priced across a range of assumed win rates for contested races.
    ///
    /// See [`RaceLadder`] for why this exists. Episodes the run took are counted at
    /// their net; episodes declined as contested are counted at `win_rate × net`, and
    /// only when that net was positive — a contested cycle that was already negative
    /// after its tip is correctly worth nothing however often you would win it.
    pub fn race_ladder(&self, gap_slots: u64) -> Result<RaceLadder> {
        let rows = self.episode_rows(gap_slots)?;
        let mut realised = 0.0;
        let mut declined_net = 0.0;
        let mut declined = 0u64;
        let mut declined_unprofitable = 0u64;
        for r in &rows {
            if r.taken {
                realised += r.best_net_usd;
            } else if r.contested {
                if r.best_net_usd > 0.0 {
                    declined_net += r.best_net_usd;
                    declined += 1;
                } else {
                    declined_unprofitable += 1;
                }
            }
        }
        Ok(RaceLadder {
            rungs: RACE_LADDER_WIN_RATES
                .iter()
                .map(|&p| (p, realised + p * declined_net))
                .collect(),
            realised_usd: realised,
            declined_net_usd: declined_net,
            declined_episodes: declined,
            declined_unprofitable_episodes: declined_unprofitable,
        })
    }

    /// Claimed dislocation grouped by how far apart in time the legs actually were.
    ///
    /// # What a healthy result looks like
    ///
    /// Flat. If two venues genuinely disagree, the size of the disagreement has no
    /// reason to depend on whether we observed them one slot apart or five hundred.
    ///
    /// # What a broken one looks like
    ///
    /// Dislocation rising with the spread. That is the instrument reporting the
    /// market's *movement between two observations* as a disagreement between two
    /// venues — an edge that was never simultaneously available and cannot be taken.
    /// It is invisible to `--verify`, which checks one pool at a time against a router
    /// at one instant and so can never see a gap that only exists across two.
    pub fn spread_audit(&self) -> Result<Vec<SpreadBand>> {
        let bands = [
            ("same slot", 0i64, 0i64),
            ("1 slot", 1, 1),
            ("2-10", 2, 10),
            ("11-100", 11, 100),
            ("over 100", 101, i64::MAX),
        ];
        let mut out = Vec::new();
        for (label, lo, hi) in bands {
            let row = self.conn.query_row(
                "SELECT COUNT(*), AVG(dislocation_bps), AVG(fee_bps),
                        COALESCE(SUM(profit_at_100_usd), 0)
                 FROM paper_fills
                 WHERE slot_spread IS NOT NULL AND slot_spread >= ?1 AND slot_spread <= ?2",
                rusqlite::params![lo, hi],
                |r| {
                    Ok(SpreadBand {
                        label: label.to_string(),
                        fills: r.get::<_, i64>(0)? as u64,
                        mean_dislocation_bps: r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                        mean_fee_bps: r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                        value_at_100_usd: r.get(3)?,
                    })
                },
            )?;
            out.push(row);
        }
        Ok(out)
    }

    /// Edge by fee tier, priced twice: over every loop, and over only those whose legs
    /// were read in the **same slot**.
    ///
    /// # The question this answers
    ///
    /// An arbitrage is a claim that two venues disagree at one moment. 82% of loops here
    /// price their legs from different slots, because `MAX_STALE_LAG_SLOTS` admits a pool
    /// minutes behind the head. So part of any reported edge may be the market moving
    /// between two observations rather than two venues disagreeing — and that part cannot
    /// be traded, because the older price has already gone.
    ///
    /// Comparing the two columns *within a fee tier* is what isolates it. Comparing
    /// spread bands instead does not: each band mixes fee tiers and the effect hides.
    /// A tier whose edge falls when simultaneity is required was reporting timing.
    pub fn simultaneity_audit(&self) -> Result<Vec<FeeTierEdge>> {
        let tiers = [
            ("under 5 bps", 0.0f64, 5.0f64),
            ("5-20", 5.0, 20.0),
            ("20-50", 20.0, 50.0),
            ("over 50", 50.0, f64::INFINITY),
        ];
        let mut out = Vec::new();
        for (label, lo, hi) in tiers {
            let hi = if hi.is_finite() { hi } else { f64::MAX };
            let (fills_all, edge_all): (i64, Option<f64>) = self.conn.query_row(
                "SELECT COUNT(*), AVG(edge_bps) FROM paper_fills
                 WHERE slot_spread IS NOT NULL AND fee_bps >= ?1 AND fee_bps < ?2",
                rusqlite::params![lo, hi],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let (fills_same, edge_same): (i64, Option<f64>) = self.conn.query_row(
                "SELECT COUNT(*), AVG(edge_bps) FROM paper_fills
                 WHERE slot_spread = 0 AND fee_bps >= ?1 AND fee_bps < ?2",
                rusqlite::params![lo, hi],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            out.push(FeeTierEdge {
                label: label.to_string(),
                fills_all: fills_all as u64,
                edge_all_bps: edge_all.unwrap_or(0.0),
                fills_same_slot: fills_same as u64,
                edge_same_slot_bps: edge_same.unwrap_or(0.0),
            });
        }
        Ok(out)
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
    ///
    /// Distributed over the *tradeable* edge. A histogram of marginal rates has a fat
    /// right tail made entirely of routes with no size behind them, which is a picture
    /// of the search space rather than of the opportunity.
    pub fn edge_histogram(&self, edges: &[f64]) -> Result<Vec<(f64, f64, u64)>> {
        let mut out = Vec::new();
        for w in edges.windows(2) {
            let n: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sweeps
                 WHERE depth_measured = 1
                   AND tradeable_edge_bps >= ?1 AND tradeable_edge_bps < ?2",
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
    /// Cycles that cleared their fees *and* had the depth to take the capital.
    pub clearing: u64,
    /// Highest marginal rate over every cycle, whatever its depth. A diagnostic:
    /// routinely far above anything that could be traded.
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
    /// Best edge among cycles deep enough to absorb the capital, the honest headline.
    /// `None` means nothing qualified — recorded as NULL, never as zero.
    pub tradeable_edge_bps: Option<f64>,
    pub tradeable_dislocation_bps: Option<f64>,
    pub tradeable_fee_bps: Option<f64>,
    pub tradeable_depth_usd: Option<f64>,
    pub tradeable_route: String,
    /// Pools dropped from this sweep for lagging too far behind the newest slot.
    pub stale_excluded: usize,
    /// Whether this sweep measured cycle depth at all. False only for rows written by
    /// builds that predate the tradeable/marginal split.
    pub depth_measured: bool,
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
    /// Rotation-invariant identity of the loop. See `Cycle::canonical_key`.
    pub cycle_key: String,
    /// What this opportunity would have paid at each rung of [`CAPITAL_LADDER_USD`].
    /// `None` when the run did not price the ladder — recorded as NULL, never as zero.
    pub profit_at_capital_usd: Option<[f64; 3]>,
    /// Slots between the freshest and stalest leg of the loop.
    ///
    /// Zero is the only value for which the word *dislocation* is honest: it means
    /// every leg was observed in the same slot, so the difference between them is a
    /// disagreement between venues rather than the market having moved in between.
    /// `None` for runs that did not measure it.
    pub slot_spread: Option<u64>,
}

/// One episode: a gap, from the moment it opened to the moment it closed.
#[derive(Debug, Clone)]
struct EpisodeRow {
    /// What it was worth at the size that maximises it, at any capital.
    pie_usd: f64,
    best_net_usd: f64,
    taken: bool,
    detections: u64,
    /// Slots between first and last sighting. Zero means it did not survive to the
    /// next slot.
    lifetime_slots: u64,
    /// Capital the pie would have required.
    optimal_size_usd: f64,
    /// When the episode opened. Its first sighting, not its best one — the curve steps
    /// when an opportunity arrives.
    first_at: String,
    /// Best value at each rung of [`CAPITAL_LADDER_USD`]. `None` unless every rung was
    /// measured.
    ladder: Option<[f64; 3]>,
    /// Whether any detection in this episode was declined as a race we would lose.
    contested: bool,
}

/// One opportunity, as a point on the value-against-lifetime plot.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodePoint {
    /// What it was worth at the size that maximises it, at any capital.
    pub pie_usd: f64,
    /// What this run's book would have kept. Zero when it was not taken.
    pub net_usd: f64,
    /// Slots between first and last sighting. Zero means gone before the next slot.
    pub lifetime_slots: u64,
    pub optimal_size_usd: f64,
    pub detections: u64,
    pub taken: bool,
    pub contested: bool,
}

/// Evidence for or against the contest classifier, from the run's own data.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContestAudit {
    pub contested_episodes: u64,
    /// Of those, ones still quotable at least a slot later — which nobody had taken.
    pub contested_survived: u64,
    contested_slots: u64,
    pub uncontested_episodes: u64,
    pub uncontested_survived: u64,
    uncontested_slots: u64,
    /// Whole-pie value the classifier declined.
    pub declined_usd: f64,
    /// The part of it that outlived the slot it was declined in.
    pub declined_but_survived_usd: f64,
}

impl ContestAudit {
    /// Share of declined opportunities that were still there a slot later.
    #[must_use]
    pub fn contested_survival_rate(&self) -> f64 {
        if self.contested_episodes == 0 {
            0.0
        } else {
            self.contested_survived as f64 / self.contested_episodes as f64
        }
    }

    #[must_use]
    pub fn uncontested_survival_rate(&self) -> f64 {
        if self.uncontested_episodes == 0 {
            0.0
        } else {
            self.uncontested_survived as f64 / self.uncontested_episodes as f64
        }
    }

    #[must_use]
    pub fn contested_mean_slots(&self) -> f64 {
        if self.contested_episodes == 0 {
            0.0
        } else {
            self.contested_slots as f64 / self.contested_episodes as f64
        }
    }

    #[must_use]
    pub fn uncontested_mean_slots(&self) -> f64 {
        if self.uncontested_episodes == 0 {
            0.0
        } else {
            self.uncontested_slots as f64 / self.uncontested_episodes as f64
        }
    }

    /// Whether there is enough of both groups for the comparison to mean anything.
    ///
    /// Below this the two rates are noise, and a report that printed them anyway would
    /// invite exactly the conclusion the audit exists to avoid drawing prematurely.
    #[must_use]
    pub fn has_enough_evidence(&self) -> bool {
        self.contested_episodes >= 20 && self.uncontested_episodes >= 20
    }
}

/// One point on the run's cumulative P&L curve.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EquityPoint {
    /// UTC timestamp of the episode that produced this step.
    pub at: String,
    /// Cumulative net across episodes this run's book would have taken, once each.
    pub realised_usd: f64,
    /// The same episodes valued at each rung of [`CAPITAL_LADDER_USD`], before tip.
    pub at_capital_usd: [f64; 3],
    /// Cumulative whole-pie value at unlimited capital: the ceiling.
    pub at_optimal_usd: f64,
    /// Distinct opportunities seen so far.
    pub episodes: u64,
    /// Of those, ones the paper trader acted on.
    pub taken: u64,
    /// Episodes that carried a full ladder measurement, and ones that did not. The
    /// second number is the honest caveat on the first: ladder totals describe only the
    /// episodes that measured it.
    pub ladder_episodes: u64,
    pub unmeasured_episodes: u64,
}

/// Edge in one fee tier, over every loop and over only the simultaneous ones.
///
/// `fills_same_slot` is the sample size that decides whether the comparison means
/// anything. A tier with a handful of same-slot fills is not evidence of either case.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeTierEdge {
    pub label: String,
    pub fills_all: u64,
    pub edge_all_bps: f64,
    pub fills_same_slot: u64,
    pub edge_same_slot_bps: f64,
}

impl FeeTierEdge {
    /// What requiring simultaneity costs this tier, as a share of its pooled edge.
    /// Positive means the pooled figure was flattered by non-simultaneous loops.
    #[must_use]
    pub fn timing_share(&self) -> Option<f64> {
        if self.fills_same_slot == 0 || self.edge_all_bps.abs() < 1e-9 {
            return None;
        }
        Some((self.edge_all_bps - self.edge_same_slot_bps) / self.edge_all_bps)
    }
}

/// Claimed dislocation, grouped by how far apart in time the legs were observed.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpreadBand {
    pub label: String,
    pub fills: u64,
    pub mean_dislocation_bps: f64,
    pub mean_fee_bps: f64,
    pub value_at_100_usd: f64,
}

/// Win rates the race ladder is priced at. Not predictions — the span of assumptions
/// worth seeing, from "we never win a contested race" to "we always do".
pub const RACE_LADDER_WIN_RATES: [f64; 5] = [0.0, 0.05, 0.25, 0.50, 1.00];

/// What the run would have earned at several assumed win rates for contested races.
///
/// # The assumption this exists to expose
///
/// A cycle worth more than a threshold is declined on the belief that a faster searcher
/// takes it first. That single rule decides the large majority of the value this
/// instrument ever sees — and the decline is applied *after* the same cycle has already
/// been charged a tip large enough to win the race, so competition is priced twice: once
/// as a haircut, again as a refusal.
///
/// Which of the two is right cannot be settled from paper. What can be done is to stop
/// hiding the assumption inside a boolean and price the run across the range, exactly as
/// [`CapitalLadder`] does for capital. A losing race costs the base fee or nothing at
/// all — the bundle simply does not land — so the downside of attempting is close to
/// zero and the ladder is close to linear in the win rate.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RaceLadder {
    /// `(assumed win rate, net USD the run would have made)`.
    pub rungs: Vec<(f64, f64)>,
    /// Net actually booked — uncontested episodes only. The 0.0 rung equals this.
    pub realised_usd: f64,
    /// Net-after-tip currently refused for being contested, and how many episodes.
    /// This is the whole size of the question.
    pub declined_net_usd: f64,
    pub declined_episodes: u64,
    /// Contested episodes whose net was already negative. Refusing these costs nothing
    /// and the ladder correctly gives them no weight at any win rate.
    pub declined_unprofitable_episodes: u64,
}

/// What a run's opportunities were worth to accounts of several sizes.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapitalLadder {
    /// `(book size in USD, what this run's episodes would have paid it)`.
    pub rungs: Vec<(f64, f64)>,
    /// The same episodes at unlimited capital.
    pub at_optimal_usd: f64,
    /// What the run's actual book took, after tip. The only figure here that is a
    /// result rather than a counterfactual.
    pub realised_usd: f64,
    pub measured_episodes: u64,
    pub unmeasured_episodes: u64,
}

/// Opportunities of one size, and how long they lasted.
#[derive(Debug, Clone, Default)]
pub struct SurvivalBand {
    pub label: String,
    pub episodes: u64,
    total_slots: u64,
    pub longest_slots: u64,
    total_detections: u64,
    total_capital_usd: f64,
}

impl SurvivalBand {
    /// Mean lifetime in slots.
    #[must_use]
    pub fn mean_slots(&self) -> f64 {
        if self.episodes == 0 { 0.0 } else { self.total_slots as f64 / self.episodes as f64 }
    }

    /// Mean lifetime in seconds, at ~400 ms per slot.
    #[must_use]
    pub fn mean_secs(&self) -> f64 {
        self.mean_slots() * 0.4
    }

    /// Mean capital the opportunities in this band would have needed.
    #[must_use]
    pub fn mean_capital_usd(&self) -> f64 {
        if self.episodes == 0 { 0.0 } else { self.total_capital_usd / self.episodes as f64 }
    }

    /// Mean sightings before it closed.
    #[must_use]
    pub fn mean_detections(&self) -> f64 {
        if self.episodes == 0 { 0.0 } else { self.total_detections as f64 / self.episodes as f64 }
    }
}

/// Everything a run has measured.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Every sweep ever sampled, including ones from before depth was measured.
    pub samples: u64,
    /// Sweeps that measured cycle depth, and so can say anything about tradeability.
    /// The denominator for every rate below.
    pub depth_samples: u64,
    /// Of those, ones with at least one cycle deep enough to take the capital —
    /// profitable or not. The means below are taken over exactly these.
    pub tradeable_samples: u64,
    /// Samples in which some route was profitable *and* could absorb the capital.
    pub clearing_samples: u64,
    /// Samples whose best marginal rate had no tradeable size behind it at all. The
    /// distance between what the book advertises and what it will actually fill.
    pub untradeable_leader_samples: u64,
    pub first_at: Option<String>,
    pub last_at: Option<String>,
    /// Depth-qualified: the edge of the best route that could actually be traded.
    pub mean_edge_bps: f64,
    pub best_edge_bps: f64,
    /// The raw marginal maximum over every cycle regardless of depth. Kept beside the
    /// tradeable figure as a diagnostic — it is routinely an order of magnitude larger,
    /// and reading it as an opportunity is the mistake this pair exists to prevent.
    pub marginal_best_edge_bps: f64,
    /// Most pools any single sweep had to drop for lagging behind.
    pub stale_excluded_max: u64,
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
    /// Fraction of sampled moments in which something profitable was also tradeable.
    ///
    /// Denominated in depth-measuring samples, not all samples: a run that predates
    /// the depth split has nothing to say about this, and dividing by its samples
    /// would report a confident zero where the honest answer is "not measured".
    #[must_use]
    pub fn clearing_rate(&self) -> f64 {
        if self.depth_samples == 0 {
            0.0
        } else {
            self.clearing_samples as f64 / self.depth_samples as f64
        }
    }

    /// Whether any sample in this ledger measured depth. When false, every
    /// depth-qualified figure is absent rather than zero, and must be rendered that way.
    #[must_use]
    pub fn has_depth_measurement(&self) -> bool {
        self.depth_samples > 0
    }

    /// Fraction of depth-measuring samples whose leading rate could not be traded.
    #[must_use]
    pub fn untradeable_leader_rate(&self) -> f64 {
        if self.depth_samples == 0 {
            0.0
        } else {
            self.untradeable_leader_samples as f64 / self.depth_samples as f64
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
    /// Longest single episode in slots — the same thing on the chain's own clock.
    pub longest_slots: u64,
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
            // Deep enough to trade by default: these fixtures exist to exercise the
            // statistics, and a sample with no tradeable cycle is its own case below.
            tradeable_edge_bps: Some(edge),
            tradeable_dislocation_bps: Some(edge + 2.0),
            tradeable_fee_bps: Some(2.0),
            tradeable_depth_usd: Some(1234.0),
            tradeable_route: "SOL -> USDC -> SOL".into(),
            stale_excluded: 0,
            depth_measured: true,
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
            cycle_key: "aabbccdd:0101>eeff0011:0202".into(),
            // Rising, then flat: this fixture's cycle runs out of depth between $1k and
            // $10k, which is the shape the ladder exists to expose.
            profit_at_capital_usd: Some([0.004, 0.006, 0.006]),
            // Simultaneous by default: a fixture asserting a dislocation should assert
            // one that was actually observable at a single moment.
            slot_spread: Some(0),
        }
    }

    /// A fill on its own cycle and slot, so each becomes its own episode.
    fn contested_fill(slot: u64, key: &str, net: f64) -> FillRecord {
        FillRecord {
            slot,
            cycle_key: key.into(),
            net_usd: net,
            taken: false,
            skipped_reason: Some("contested — would lose the race".into()),
            ..fill(net, false)
        }
    }

    #[test]
    fn the_race_ladder_starts_at_what_the_run_actually_booked() {
        let l = Ledger::open_in_memory().unwrap();
        l.record_fill(&FillRecord { slot: 1, cycle_key: "a".into(), ..fill(0.10, true) }).unwrap();
        l.record_fill(&FillRecord { slot: 2, cycle_key: "b".into(), ..fill(0.20, true) }).unwrap();
        let r = l.race_ladder(5).unwrap();
        assert!((r.realised_usd - 0.30).abs() < 1e-9);
        // Nothing was declined, so every rung is the same number: the win rate has
        // nothing to act on.
        for (_, v) in &r.rungs {
            assert!((v - 0.30).abs() < 1e-9, "flat ladder expected, got {v}");
        }
    }

    #[test]
    fn declined_contested_value_is_priced_in_proportion_to_the_assumed_win_rate() {
        let l = Ledger::open_in_memory().unwrap();
        l.record_fill(&FillRecord { slot: 1, cycle_key: "a".into(), ..fill(0.10, true) }).unwrap();
        l.record_fill(&contested_fill(2, "b", 1.00)).unwrap();
        l.record_fill(&contested_fill(3, "c", 3.00)).unwrap();
        let r = l.race_ladder(5).unwrap();
        assert_eq!(r.declined_episodes, 2);
        assert!((r.declined_net_usd - 4.00).abs() < 1e-9);
        let at = |p: f64| r.rungs.iter().find(|(w, _)| (w - p).abs() < 1e-9).unwrap().1;
        assert!((at(0.0) - 0.10).abs() < 1e-9, "never winning is what we book today");
        assert!((at(0.25) - 1.10).abs() < 1e-9);
        assert!((at(1.0) - 4.10).abs() < 1e-9, "winning every race is the ceiling");
    }

    /// A contested cycle already negative after its tip is worth nothing however often
    /// you would win it — counting it would make refusing look expensive when refusing
    /// is exactly right.
    #[test]
    fn a_contested_episode_that_was_already_unprofitable_adds_nothing_at_any_win_rate() {
        let l = Ledger::open_in_memory().unwrap();
        l.record_fill(&contested_fill(1, "a", -0.50)).unwrap();
        let r = l.race_ladder(5).unwrap();
        assert_eq!(r.declined_episodes, 0);
        assert_eq!(r.declined_unprofitable_episodes, 1);
        for (_, v) in &r.rungs {
            assert!(v.abs() < 1e-9, "a losing trade cannot become profit by winning it");
        }
    }

    /// The same standing gap seen five hundred times is one decision, not five hundred.
    #[test]
    fn one_contested_gap_is_counted_once_however_often_it_was_re_detected() {
        let l = Ledger::open_in_memory().unwrap();
        for slot in 1..=8 {
            l.record_fill(&contested_fill(slot, "same", 2.00)).unwrap();
        }
        let r = l.race_ladder(5).unwrap();
        assert_eq!(r.declined_episodes, 1, "eight detections of one gap are one episode");
        assert!((r.declined_net_usd - 2.00).abs() < 1e-9);
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

        let e = l.episodes(5).unwrap();
        assert_eq!(e.count, 1, "it was one gap the whole time");
        assert_eq!(e.detections, 1000);
        assert!(e.inflation() > 900.0, "the raw count overstated by ~1000x");
        assert!((e.total_net_usd - 0.005).abs() < 1e-9, "worth one fill, not a thousand");
    }

    /// Two different loops that clear at the same time are two opportunities.
    #[test]
    fn distinct_cycles_are_distinct_episodes() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..10 {
            l.record_fill(&fill(0.001, true)).unwrap();
            let mut other = fill(0.002, true);
            other.route = "SOL -> USDT -> SOL".into();
            other.cycle_key = "11223344:0303>55667788:0404".into();
            l.record_fill(&other).unwrap();
        }
        let e = l.episodes(5).unwrap();
        assert_eq!(e.count, 2);
        assert!((e.total_net_usd - 0.003).abs() < 1e-9, "best of each, summed once");
    }

    /// The mirror bug, pinned at the ledger. One round trip is reported under two
    /// routes — one per mint you could enter it at — and they must collapse, or every
    /// opportunity count this instrument prints is doubled.
    #[test]
    fn one_loop_reported_under_two_route_names_is_one_episode() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..20 {
            l.record_fill(&fill(0.004, true)).unwrap();
            // Same loop, entered at the other mint: different printed route and a
            // reversed venue list, but the identical set of pools in the same order.
            let mut mirror = fill(0.004, true);
            mirror.route = "USDC -> SOL -> USDC".into();
            mirror.venues = "RAY-CL 1bp - ORCA 1bp".into();
            l.record_fill(&mirror).unwrap();
        }
        let e = l.episodes(5).unwrap();
        assert_eq!(e.count, 1, "one loop, entered two ways, is one arbitrage");
        assert!((e.total_net_usd - 0.004).abs() < 1e-9, "and it pays once");
    }

    /// Opportunities big enough to matter do not survive; small ones linger. If this
    /// ever inverts, the whole verdict changes and it should be re-examined.
    #[test]
    fn survival_buckets_by_value_and_measures_lifetime_in_slots() {
        let l = Ledger::open_in_memory().unwrap();
        // A fat gap, seen once and gone.
        let mut fat = fill(0.5, true);
        fat.profit_at_optimal_usd = 3.0;
        fat.optimal_size_usd = 4000.0;
        fat.cycle_key = "fa000000:0101>fb000000:0202".into();
        l.record_fill(&fat).unwrap();
        // A crumb, standing for ten slots.
        for slot in 0..10 {
            let mut thin = fill(0.0001, true);
            thin.profit_at_optimal_usd = 0.0002;
            thin.optimal_size_usd = 1.0;
            thin.slot = 100 + slot;
            l.record_fill(&thin).unwrap();
        }

        let bands = l.survival(5).unwrap();
        let over = bands.iter().find(|b| b.label == "over $1").expect("fat band");
        let under = bands.iter().find(|b| b.label == "under $0.001").expect("thin band");
        assert_eq!(over.episodes, 1);
        assert_eq!(over.longest_slots, 0, "gone before the next slot");
        assert_eq!(under.episodes, 1);
        assert_eq!(under.longest_slots, 9, "still there ten slots later");
        assert!(over.mean_capital_usd() > under.mean_capital_usd() * 100.0);
    }

    /// An episode is worth its best moment, not the sum of its moments — taking an
    /// arbitrage is what removes it.
    #[test]
    fn an_episode_is_worth_its_best_detection() {
        let l = Ledger::open_in_memory().unwrap();
        for net in [0.001, 0.004, 0.002] {
            l.record_fill(&fill(net, true)).unwrap();
        }
        let e = l.episodes(5).unwrap();
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
        let e = l.episodes(5).unwrap();
        assert_eq!(e.count, 1);
        assert_eq!(e.taken, 0);
        assert_eq!(e.total_net_usd, 0.0);
    }

    /// The curve must step once per opportunity, not once per sighting. A gap standing
    /// for a thousand sweeps is one step — otherwise the chart draws a steady climb
    /// through money that was only ever there once, and the steeper it looks the more
    /// wrong it is.
    #[test]
    fn the_equity_curve_steps_per_episode_not_per_detection() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..500 {
            l.record_fill(&fill(0.004, true)).unwrap();
        }
        let curve = l.equity_curve(5, 0).unwrap();
        assert_eq!(curve.len(), 1, "one standing gap is one point");
        assert!((curve[0].realised_usd - 0.004).abs() < 1e-9);
        assert_eq!(curve[0].episodes, 1);
        assert_eq!(curve[0].taken, 1);
    }

    #[test]
    fn the_curve_accumulates_across_distinct_opportunities() {
        let l = Ledger::open_in_memory().unwrap();
        for (i, net) in [0.001, 0.002, 0.004].into_iter().enumerate() {
            let mut f = fill(net, true);
            // Distinct loops, so each is its own episode.
            f.cycle_key = format!("{i:02}aabbcc:0101>ddeeff00:0202");
            l.record_fill(&f).unwrap();
        }
        let curve = l.equity_curve(5, 0).unwrap();
        assert_eq!(curve.len(), 3);
        assert!(curve[0].realised_usd < curve[1].realised_usd);
        assert!((curve[2].realised_usd - 0.007).abs() < 1e-9, "cumulative, not per-point");
        assert_eq!(curve[2].episodes, 3);
    }

    /// Downsampling must never move the total. A caller reading the run's P&L off the
    /// tail of a plotted curve has to get the same number the full curve ends on.
    #[test]
    fn downsampling_keeps_the_final_total_exact() {
        let l = Ledger::open_in_memory().unwrap();
        for i in 0..50 {
            let mut f = fill(0.001, true);
            f.cycle_key = format!("{i:04}bbcc:0101>ddeeff00:0202");
            l.record_fill(&f).unwrap();
        }
        let full = l.equity_curve(5, 0).unwrap();
        let sampled = l.equity_curve(5, 7).unwrap();
        assert!(sampled.len() <= 8, "downsampled to {} points", sampled.len());
        assert!(sampled.len() < full.len());
        let (a, b) = (full.last().unwrap(), sampled.last().unwrap());
        assert!((a.realised_usd - b.realised_usd).abs() < 1e-12);
        assert_eq!(a.episodes, b.episodes);
    }

    /// The measurement the whole ladder exists for: what a bigger book would have taken
    /// from the *same* opportunities, and where it stops helping.
    #[test]
    fn the_capital_ladder_prices_the_same_episodes_at_several_book_sizes() {
        let l = Ledger::open_in_memory().unwrap();
        for _ in 0..20 {
            l.record_fill(&fill(0.0002, true)).unwrap();
        }
        let ladder = l.capital_ladder(5).unwrap();
        assert_eq!(ladder.rungs.len(), CAPITAL_LADDER_USD.len());
        assert_eq!(ladder.measured_episodes, 1);
        assert_eq!(ladder.unmeasured_episodes, 0);

        // Counted once each, like every other episode figure.
        assert!((ladder.rungs[0].1 - 0.004).abs() < 1e-9);
        assert!((ladder.rungs[1].1 - 0.006).abs() < 1e-9);
        // Flat between $1k and $10k: depth ran out, so the extra capital bought nothing.
        // This is the shape that says a flash loan would not have helped.
        assert!((ladder.rungs[2].1 - ladder.rungs[1].1).abs() < 1e-12);
        assert!(ladder.realised_usd < ladder.rungs[0].1, "our $5 book took less than $100 would");
    }

    /// A ladder from a run that never measured it must read as absent, not as zero.
    /// "This opportunity was worth nothing at $100" is a claim the older run never made.
    #[test]
    fn episodes_without_a_ladder_are_counted_as_unmeasured_not_as_zero() {
        let l = Ledger::open_in_memory().unwrap();
        let mut old = fill(0.001, true);
        old.profit_at_capital_usd = None;
        l.record_fill(&old).unwrap();

        let ladder = l.capital_ladder(5).unwrap();
        assert_eq!(ladder.measured_episodes, 0);
        assert_eq!(ladder.unmeasured_episodes, 1);
        assert_eq!(ladder.rungs[0].1, 0.0, "nothing measured contributes nothing");
        assert!((ladder.realised_usd - 0.001).abs() < 1e-9, "but its P&L is still real");
    }

    /// The audit's core comparison: an opportunity still quotable a slot later was
    /// taken by nobody, so declining it as a lost race was wrong.
    #[test]
    fn the_contest_audit_separates_declined_episodes_that_survived() {
        let l = Ledger::open_in_memory().unwrap();

        // Declined, and still there four slots later — nobody took it.
        for slot in 0..5 {
            let mut f = fill(0.05, false);
            f.skipped_reason = Some("contested — would lose the race".into());
            f.profit_at_optimal_usd = 0.5;
            f.slot = 100 + slot;
            f.cycle_key = "aa000000:0101>bb000000:0202".into();
            l.record_fill(&f).unwrap();
        }
        // Taken, and gone before the next slot.
        let mut quick = fill(0.001, true);
        quick.slot = 400;
        quick.cycle_key = "cc000000:0101>dd000000:0202".into();
        l.record_fill(&quick).unwrap();

        let a = l.contest_audit(5).unwrap();
        assert_eq!(a.contested_episodes, 1);
        assert_eq!(a.contested_survived, 1, "it outlived the slot it was declined in");
        assert!((a.contested_mean_slots() - 4.0).abs() < 1e-9);
        assert_eq!(a.uncontested_episodes, 1);
        assert_eq!(a.uncontested_survived, 0);
        assert!((a.declined_usd - 0.5).abs() < 1e-9);
        assert!((a.declined_but_survived_usd - 0.5).abs() < 1e-9);
    }

    /// Two episodes of each is not evidence. The report must say so rather than print a
    /// verdict off four data points.
    #[test]
    fn the_audit_withholds_a_verdict_until_both_groups_are_large_enough() {
        let l = Ledger::open_in_memory().unwrap();
        for i in 0..3 {
            let mut f = fill(0.05, false);
            f.skipped_reason = Some("contested — would lose the race".into());
            f.cycle_key = format!("{i:02}aa0000:0101>bb000000:0202");
            l.record_fill(&f).unwrap();
        }
        assert!(!l.contest_audit(5).unwrap().has_enough_evidence());
    }

    /// A reader must not be able to alter the measurement, and must not conjure an
    /// empty ledger that would read as a run which found nothing.
    #[test]
    fn a_read_only_open_cannot_write_and_refuses_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("cb-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.db");
        let p = path.to_str().unwrap();

        assert!(Ledger::open_read_only(p).is_err(), "must not create the file");

        {
            let w = Ledger::open(p).unwrap();
            w.record_fill(&fill(0.002, true)).unwrap();
        }
        let ro = Ledger::open_read_only(p).unwrap();
        assert_eq!(ro.summary().unwrap().fills, 1, "it can read");
        assert!(ro.record_fill(&fill(0.002, true)).is_err(), "and only read");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_ledger_summarises_to_zeroes_rather_than_erroring() {
        let l = Ledger::open_in_memory().unwrap();
        let s = l.summary().unwrap();
        assert_eq!(s.samples, 0);
        assert_eq!(s.clearing_rate(), 0.0);
        assert!(l.top_routes(5).unwrap().is_empty());
        assert_eq!(l.concentration().unwrap().1, 0.0);
        assert_eq!(l.episodes(5).unwrap().count, 0);
        assert_eq!(l.median_sol_price().unwrap(), 0.0);
        assert_eq!(l.episodes(5).unwrap().inflation(), 0.0);
        assert_eq!(l.hours_observed().unwrap(), 0.0);
        assert_eq!(l.fill_percentiles().unwrap().taken_net_p50, 0.0);
        assert!(l.equity_curve(5, 100).unwrap().is_empty());
        assert_eq!(l.capital_ladder(5).unwrap().rungs[0].1, 0.0);
    }

    #[test]
    fn a_sample_with_no_tradeable_cycle_records_null_not_zero() {
        // The market showed a 40 bps rate with nothing behind it. That is not a
        // 40 bps opportunity, and it is not a 0 bps one either — it is an absence,
        // and it has to survive the round trip through SQLite as one.
        let l = Ledger::open_in_memory().unwrap();
        let mut s = sweep(40.0);
        s.tradeable_edge_bps = None;
        s.tradeable_dislocation_bps = None;
        s.tradeable_fee_bps = None;
        s.tradeable_depth_usd = None;
        s.tradeable_route = String::new();
        s.clearing = 0;
        l.record_sweep(&s).unwrap();

        let got = l.summary().unwrap();
        assert_eq!(got.depth_samples, 1, "the sample still counts as measured");
        assert_eq!(got.tradeable_samples, 0, "nothing was tradeable in it");
        assert_eq!(got.clearing_samples, 0, "an untradeable rate does not clear");
        assert_eq!(got.clearing_rate(), 0.0);
        assert_eq!(got.untradeable_leader_samples, 1, "the gap must be counted, not hidden");
        assert!(
            (got.marginal_best_edge_bps - 40.0).abs() < 1e-9,
            "the marginal rate is still reported, as a diagnostic"
        );
        assert_eq!(got.best_edge_bps, 0.0, "no tradeable edge existed to report");
    }

    #[test]
    fn an_untradeable_leader_does_not_reach_the_headline_or_the_histogram() {
        let l = Ledger::open_in_memory().unwrap();
        // One real, small, tradeable opportunity...
        l.record_sweep(&sweep(3.0)).unwrap();
        // ...and one enormous rate with no size behind it.
        let mut phantom = sweep(1156.0);
        phantom.tradeable_edge_bps = None;
        phantom.tradeable_dislocation_bps = None;
        phantom.tradeable_fee_bps = None;
        phantom.clearing = 0;
        l.record_sweep(&phantom).unwrap();

        let got = l.summary().unwrap();
        assert!(
            (got.best_edge_bps - 3.0).abs() < 1e-9,
            "the headline must be the tradeable 3 bps, not the phantom 1156"
        );
        assert!((got.marginal_best_edge_bps - 1156.0).abs() < 1e-9);
        assert!((got.untradeable_leader_rate() - 0.5).abs() < 1e-9);

        let hist = l.edge_histogram(&[0.0, 10.0, 100_000.0]).unwrap();
        assert_eq!(hist[0].2, 1, "the tradeable 3 bps lands in the low bucket");
        assert_eq!(hist[1].2, 0, "the phantom must not appear in the tail");
    }

    #[test]
    fn a_ledger_written_before_depth_was_measured_still_opens_and_reports() {
        // Migration: a sweeps table from the older build, with none of the new
        // columns. It must gain them, keep its rows, and refuse to pass its old
        // marginal numbers off as tradeable ones.
        let l = Ledger::open_in_memory().unwrap();
        l.conn
            .execute_batch(
                "DROP TABLE sweeps;
                 CREATE TABLE sweeps (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    at TEXT NOT NULL DEFAULT (datetime('now')),
                    slot INTEGER NOT NULL, evaluated INTEGER NOT NULL,
                    clearing INTEGER NOT NULL, best_edge_bps REAL NOT NULL,
                    best_dislocation_bps REAL NOT NULL, best_fee_bps REAL NOT NULL,
                    best_route TEXT NOT NULL, best_venues TEXT NOT NULL,
                    best_hops INTEGER NOT NULL, best_depth_usd REAL NOT NULL,
                    sol_price_usd REAL NOT NULL, pools_ready INTEGER NOT NULL,
                    sweep_us INTEGER NOT NULL);
                 INSERT INTO sweeps
                   (slot, evaluated, clearing, best_edge_bps, best_dislocation_bps,
                    best_fee_bps, best_route, best_venues, best_hops, best_depth_usd,
                    sol_price_usd, pools_ready, sweep_us)
                 VALUES (1, 400, 1, 12.5, 14.5, 2.0, 'SOL -> USDC -> SOL', 'ORCA', 2,
                         1000.0, 91.0, 83, 6000);",
            )
            .unwrap();

        crate::schema::migrate(&l.conn).expect("an older ledger must migrate in place");

        let got = l.summary().unwrap();
        assert_eq!(got.samples, 1, "the old row must survive the migration");
        assert_eq!(got.depth_samples, 0, "it never measured depth");
        assert!(!got.has_depth_measurement());
        assert_eq!(got.clearing_samples, 0);
        assert_eq!(
            got.best_edge_bps, 0.0,
            "an old marginal number must not be promoted to a tradeable one"
        );
        assert!((got.marginal_best_edge_bps - 12.5).abs() < 1e-9);

        // And a new row lands alongside it without disturbing the old one.
        l.record_sweep(&sweep(4.0)).unwrap();
        let got = l.summary().unwrap();
        assert_eq!(got.samples, 2);
        assert_eq!(got.depth_samples, 1);
        assert!((got.best_edge_bps - 4.0).abs() < 1e-9);
    }

    #[test]
    fn excluded_stale_pools_are_counted_not_hidden() {
        let l = Ledger::open_in_memory().unwrap();
        let mut s = sweep(2.0);
        s.stale_excluded = 7;
        l.record_sweep(&s).unwrap();
        l.record_sweep(&sweep(2.0)).unwrap();
        assert_eq!(l.summary().unwrap().stale_excluded_max, 7);
    }
}
