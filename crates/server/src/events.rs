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
        /// Best cycle edge currently visible, in bps. Negative means the market is
        /// that far from profitable — the single most informative research number,
        /// and the reason "found nothing" is not an acceptable output.
        #[serde(default)]
        best_edge_bps: f64,
        #[serde(default)]
        best_route: String,
        #[serde(default)]
        best_hops: usize,
        /// Total fee cost of the best route, in bps. Compare against the edge to see
        /// whether fees or price efficiency is the binding constraint.
        #[serde(default)]
        best_fee_bps: f64,
        #[serde(default)]
        cycles_evaluated: u64,
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
        pair: String,
        dex_buy: String,
        dex_sell: String,
        /// Net price dislocation between the two venues, in basis points.
        spread_bps: f64,
        /// Optimal size the maths wants, in USD.
        optimal_size_usd: f64,
        /// Size we could actually take given capital, in USD.
        capped_size_usd: f64,
        /// Profit at the capped size, before tip and tax.
        gross_profit_usd: f64,
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
            updates: 0,
            dropped: 0,
            reconnects: 0,
            best_edge_bps: 0.0,
            best_route: String::new(),
            best_hops: 0,
            best_fee_bps: 0.0,
            cycles_evaluated: 0,
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
}
