//! The event model shared between the trading loop and the dashboard.
//!
//! Every variant is a *fact that already happened*. The dashboard cannot command the
//! bot through this channel — it only observes. Control actions travel a separate,
//! explicitly-guarded path.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// How many events the bus buffers before dropping the oldest for slow receivers.
///
/// Deliberately finite. If the dashboard cannot keep up, it loses events — that is the
/// correct trade. The alternative (unbounded buffering) turns a slow browser tab into
/// unbounded memory growth in the trading process.
pub const BUS_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    /// Periodic heartbeat carrying overall system state.
    #[serde(rename_all = "camelCase")]
    Status {
        mode: String,
        connected: bool,
        slot: u64,
        /// Slots between our newest pool observation and the chain head.
        slot_lag: u64,
        pools_tracked: usize,
        sol_price_usd: f64,
        uptime_secs: u64,
        /// Feed health. Dropped updates are opportunities we could not evaluate.
        #[serde(default)]
        updates: u64,
        #[serde(default)]
        dropped: u64,
        #[serde(default)]
        reconnects: u64,
        /// Connections abandoned because data stopped while the socket stayed open.
        #[serde(default)]
        stalls: u64,
        /// Seconds since the last account update. The honest freshness number:
        /// everything else on the dashboard is only as current as this.
        #[serde(default)]
        data_age_secs: u64,
        /// Best cycle edge currently visible, in bps. Negative means the market is
        /// that far from profitable — the single most informative research number,
        /// and the reason "found nothing" is not an acceptable output.
        #[serde(default)]
        best_edge_bps: f64,
        #[serde(default)]
        best_route: String,
        /// Best edge among cycles that can actually absorb the run's capital.
        ///
        /// `null` when nothing qualifies, and it must render as an em dash rather than
        /// a zero — "no tradeable cycle right now" and "a tradeable cycle at 0 bps" are
        /// different states of the market. `best_edge_bps` above is the marginal
        /// maximum over every cycle regardless of depth, which is routinely far larger
        /// and is a diagnostic, not an opportunity.
        #[serde(default)]
        tradeable_edge_bps: Option<f64>,
        #[serde(default)]
        tradeable_route: String,
        /// Depth a cycle needs before it counts as tradeable, in USD.
        #[serde(default)]
        tradeable_min_usd: f64,
        /// Pools the last sweep refused to quote because they had gone quiet too long.
        #[serde(default)]
        stale_excluded: usize,
        /// The feed has been silent long enough that the ledger has stopped recording.
        /// Sweeps continue; the numbers on screen are the last ones observed.
        #[serde(default)]
        feed_stalled: bool,
        #[serde(default)]
        best_hops: usize,
        /// Total fee cost of the best route, in bps. Compare against the edge to see
        /// whether fees or price efficiency is the binding constraint.
        #[serde(default)]
        best_fee_bps: f64,
        #[serde(default)]
        cycles_evaluated: u64,
        /// Distinct venues contributing quotable pools right now.
        #[serde(default)]
        venues: usize,
        /// Pairs quoted by more than one venue. These are where a two-hop round trip
        /// exists at all, so the count is a direct measure of how much of the search
        /// space is the cheap kind.
        #[serde(default)]
        duplicate_pairs: usize,
        /// Cheapest complete round trip available anywhere in the universe, in bps.
        /// The floor any opportunity has to clear.
        #[serde(default)]
        cheapest_round_trip_bps: f64,
        /// How long the last full cycle sweep took.
        #[serde(default)]
        sweep_us: u64,
        /// Accounts the RPC confirmed a subscription for, and refused. A refused
        /// account is a pool that will never update, which looks exactly like a pool
        /// nobody is trading — so it has to be counted rather than inferred.
        #[serde(default)]
        subscribed: u64,
        #[serde(default)]
        subscribe_errors: u64,
        /// Pools whose on-chain state differed from what the feed had delivered, at
        /// the last reconciliation. Persistently non-zero means the WebSocket is
        /// dropping updates and every number here is worth less than it looks.
        #[serde(default)]
        reconcile_drift: usize,
        #[serde(default)]
        reconcile_checked: usize,
    },

    /// The current leaderboard of routes, ranked by how close they are to clearing.
    ///
    /// This is the instrument's primary output. A route that never clears still
    /// reports *how far* it fell short and *why* — a wide price gap that fees ate is a
    /// different finding from two venues that simply agree on the price, and only one
    /// of them says anything about whether cheaper execution would help.
    #[serde(rename_all = "camelCase")]
    Routes {
        rows: Vec<RouteRow>,
        /// Depth a row needs to be tradeable rather than a quoted rate, in USD. Rows
        /// at or above it are shown as tradeable; the rest are rates with no size.
        #[serde(default)]
        tradeable_min_usd: f64,
        /// Cycles priced in this sweep.
        evaluated: u64,
        sweep_us: u64,
        slot: u64,
        ts_ms: u64,
    },

    /// A pool's reserves changed.
    #[serde(rename_all = "camelCase")]
    PoolUpdate {
        pool: String,
        dex: String,
        pair: String,
        /// Price of the base token quoted in the counter token.
        price: f64,
        reserve_a: f64,
        reserve_b: f64,
        slot: u64,
        ts_ms: u64,
    },

    /// A profitable cycle was detected. In paper mode this is as far as it goes.
    #[serde(rename_all = "camelCase")]
    Opportunity {
        id: u64,
        /// Mints in travel order.
        route: String,
        /// Venue and fee tier per hop.
        venues: String,
        hops: usize,
        /// Profit per unit at infinitesimal size, in bps.
        edge_bps: f64,
        /// Price gap between the venues before fees, in bps.
        dislocation_bps: f64,
        /// Cost of crossing those venues, in bps.
        fee_bps: f64,
        /// Optimal size the maths wants, in USD.
        optimal_size_usd: f64,
        /// Size we could actually take given capital, in USD.
        capped_size_usd: f64,
        /// What fraction of the available opportunity our capital reaches, as a
        /// percentage. The single clearest statement of what $5 costs us.
        capital_reach_pct: f64,
        /// Profit at the capped size, before tip and tax.
        gross_profit_usd: f64,
        /// Profit the same cycle would yield with unlimited capital. The gap against
        /// `gross_profit_usd` is what a $5 account costs, stated in dollars.
        #[serde(default)]
        profit_at_optimal_usd: f64,
        /// Estimated tip required to win this one.
        est_tip_usd: f64,
        /// Profit after tip. What actually reaches the wallet.
        net_profit_usd: f64,
        /// Whether anything else appears to be competing for this cycle.
        contested: bool,
        /// Why we did not act, if we did not.
        skipped_reason: Option<String>,
        slot: u64,
        ts_ms: u64,
    },

    /// An execution attempt concluded. Paper mode emits these as simulations.
    #[serde(rename_all = "camelCase")]
    Execution {
        id: u64,
        opportunity_id: u64,
        paper: bool,
        landed: bool,
        /// Realised profit after tip, in USD. Negative only if a guard failed.
        realised_usd: f64,
        tip_paid_usd: f64,
        latency_ms: u64,
        signature: Option<String>,
        reason: Option<String>,
        ts_ms: u64,
    },

    /// One line of the bot's own log, pushed the instant it is written.
    ///
    /// This is what makes the Log tab real-time rather than a poll of the file: a
    /// `tracing` writer taps every formatted line as it is emitted and forwards it here
    /// alongside writing it to disk as normal, so a connected dashboard sees it in the
    /// same round trip as any other live event — no interval, no delay tied to how
    /// often anyone asks. The file remains the durable copy and the source of truth;
    /// this is a live echo of it, not a replacement — a client that was not connected
    /// when a line was written still gets it from the file once it asks.
    #[serde(rename_all = "camelCase")]
    LogLine { line: String, ts_ms: u64 },
}

/// One route on the leaderboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RouteRow {
    /// Mints in travel order, e.g. `SOL -> USDC -> SOL`.
    pub route: String,
    /// Venue and fee tier for each hop, e.g. `ORCA 1bp - RAY-CL 2bp`.
    pub venues: String,
    pub hops: usize,
    /// Profit per unit at infinitesimal size, in bps. Positive clears.
    pub edge_bps: f64,
    /// How far apart the venues' prices are, before fees.
    pub dislocation_bps: f64,
    /// What crossing those venues costs.
    pub fee_bps: f64,
    /// Largest trade the route can price exactly, in USD. Bounded by the tightest
    /// concentrated-liquidity tick along the way.
    pub depth_usd: f64,
    pub slot: u64,
}

/// Broadcast bus. Cloning is cheap; every consumer gets its own receiver.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }

    /// Publish an event.
    ///
    /// Returns the number of receivers that got it. **Errors are intentionally
    /// swallowed**: "no dashboard connected" is the normal case, not a failure, and
    /// the trading loop must never care whether anyone is watching.
    pub fn publish(&self, event: Event) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Event {
        Event::Status {
            mode: "paper".into(),
            connected: true,
            slot: 1,
            slot_lag: 0,
            pools_tracked: 0,
            sol_price_usd: 76.97,
            uptime_secs: 0,
            tradeable_edge_bps: None,
            tradeable_route: String::new(),
            tradeable_min_usd: 0.0,
            stale_excluded: 0,
            feed_stalled: false,
            updates: 0,
            dropped: 0,
            reconnects: 0,
            stalls: 0,
            data_age_secs: 0,
            best_edge_bps: 0.0,
            best_route: String::new(),
            best_hops: 0,
            best_fee_bps: 0.0,
            cycles_evaluated: 0,
            venues: 0,
            duplicate_pairs: 0,
            cheapest_round_trip_bps: 0.0,
            sweep_us: 0,
            subscribed: 0,
            subscribe_errors: 0,
            reconcile_drift: 0,
            reconcile_checked: 0,
        }
    }

    #[test]
    fn publishing_with_no_subscribers_is_not_an_error() {
        // The trading loop runs headless most of the time. This must be a no-op.
        let bus = EventBus::new();
        assert_eq!(bus.publish(status()), 0);
    }

    #[tokio::test]
    async fn subscribers_receive_published_events() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(status());
        let got = rx.recv().await.unwrap();
        assert!(matches!(got, Event::Status { .. }));
    }

    #[tokio::test]
    async fn a_slow_subscriber_lags_instead_of_blocking_the_publisher() {
        // This is the property the whole design depends on: the UI cannot apply
        // backpressure to trading. Overfilling the bus must drop, never block.
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        for _ in 0..(BUS_CAPACITY + 50) {
            bus.publish(status());
        }
        // The receiver is told it lagged rather than the publisher being stalled.
        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert!(n > 0),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[test]
    fn events_serialise_with_a_discriminant_tag() {
        // The frontend switches on `type`; if this changes, the dashboard breaks.
        let json = serde_json::to_string(&status()).unwrap();
        assert!(json.contains("\"type\":\"status\""), "got: {json}");
        assert!(json.contains("\"solPriceUsd\""), "fields must be camelCase: {json}");
    }

    /// The tag app.js actually switches on for a live log line, pinned the same way
    /// the other variants are — this is the one where a silent rename is easiest to
    /// miss, because nothing about a log tab looking merely "a bit slow" points back
    /// at a JSON tag mismatch.
    #[test]
    fn a_log_line_serialises_with_the_tag_the_frontend_expects() {
        let json = serde_json::to_string(&Event::LogLine {
            line: "INFO cb_bot: refused: net negative after tip".into(),
            ts_ms: 1,
        })
        .unwrap();
        assert!(json.contains("\"type\":\"logLine\""), "got: {json}");
        assert!(json.contains("\"line\":"), "got: {json}");
    }

    #[tokio::test]
    async fn a_log_line_travels_the_same_bus_as_everything_else() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(Event::LogLine { line: "hello".into(), ts_ms: 42 });
        match rx.recv().await.unwrap() {
            Event::LogLine { line, ts_ms } => {
                assert_eq!(line, "hello");
                assert_eq!(ts_ms, 42);
            }
            other => panic!("expected LogLine, got {other:?}"),
        }
    }
}
