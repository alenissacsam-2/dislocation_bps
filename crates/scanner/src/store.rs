//! Concurrent in-memory pool state.
//!
//! The scanner reads this on every account update, so it must not serialise readers.
//! `DashMap` gives per-shard locking, which is sufficient: writes are per-pool and
//! readers mostly touch disjoint keys.

use crate::snapshot::Snapshot;
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
    /// Updates carrying an older slot than the stored state are **ignored**. WebSocket
    /// delivery is not ordered, and applying a late old update would silently regress
    /// the book and manufacture phantom opportunities out of nothing.
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

    /// A consistent, indexed copy of the whole market.
    ///
    /// Every search pass takes one of these rather than walking the live map. It keeps
    /// a pass from pricing a cycle out of two pool states that were never true at the
    /// same moment, and it turns the per-step mint lookup from a full scan into a hash
    /// lookup.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::new(self.pools.iter().map(|r| *r.value()).collect())
    }

    /// A snapshot with lagging pools left out, and the count of what was left out.
    ///
    /// # What this does and does not prove
    ///
    /// For an AMM, no update means no change — so an old slot is normally a *correct*
    /// price on a pool nobody is trading, which is exactly why a silently dropped
    /// subscription is dangerous: it looks the same from the stream. Slot lag cannot
    /// tell those apart, and this method does not claim to.
    ///
    /// What it does is bound the damage. A pool that has not been heard from in
    /// `max_lag` slots is dropped from the search rather than quoted, so the worst a
    /// stale quote can be is `max_lag` slots out of date instead of however long it
    /// takes the next reconcile to notice. The cost of being wrong the other way is
    /// small: a genuinely quiet pool sits out one sweep and returns the moment it
    /// trades.
    ///
    /// The reference is the store's own newest slot, not the chain head, so this stays
    /// correct while the RPC's head is unknown. That leaves one hole it cannot cover:
    /// if the feed dies completely, every pool ages together and nothing ever looks
    /// stale. Detecting *that* needs a wall clock, and belongs to the caller.
    #[must_use]
    pub fn snapshot_fresh(&self, max_lag: u64) -> (Snapshot, usize) {
        let newest = self.pools.iter().map(|r| r.slot).max().unwrap_or(0);
        let mut kept: Vec<PoolState> = Vec::with_capacity(self.pools.len());
        let mut excluded = 0usize;
        for r in self.pools.iter() {
            if newest.saturating_sub(r.slot) > max_lag {
                excluded += 1;
            } else {
                kept.push(*r.value());
            }
        }
        (Snapshot::new(kept), excluded)
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
    /// so the dashboard surfaces this directly rather than hiding it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::types::{Dex, PoolId, PoolState};

    fn pool(id: u8, a: u8, b: u8, slot: u64) -> PoolState {
        PoolState::constant_product(
            PoolId([id; 32]),
            Dex::PumpSwap,
            [a; 32],
            [b; 32],
            1_000,
            1_000,
            2500,
            slot,
        )
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

    #[test]
    fn a_snapshot_leaves_out_pools_that_have_gone_quiet_too_long() {
        let s = PoolStore::new();
        s.upsert(pool(1, 10, 20, 1_000)); // current
        s.upsert(pool(2, 20, 30, 995));   // a little behind, still trusted
        s.upsert(pool(3, 30, 40, 400));   // far behind
        let (snap, excluded) = s.snapshot_fresh(300);
        assert_eq!(excluded, 1, "only the lagging pool may be dropped");
        assert_eq!(snap.len(), 2);
        assert!(snap.get(&PoolId([3; 32])).is_none(), "the stale pool must not be quotable");
    }

    #[test]
    fn a_quiet_market_is_not_mistaken_for_a_stale_one() {
        // Every pool equally old is what a slow hour looks like, not a broken feed.
        // The reference is our own newest slot, so nothing here lags anything.
        let s = PoolStore::new();
        s.upsert(pool(1, 10, 20, 500));
        s.upsert(pool(2, 20, 30, 500));
        let (snap, excluded) = s.snapshot_fresh(10);
        assert_eq!(excluded, 0);
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn stale_pools_does_not_underflow_on_future_slots() {
        // A pool observed ahead of our current slot view must not wrap into "stale".
        let s = PoolStore::new();
        s.upsert(pool(1, 10, 20, 500));
        assert!(s.stale_pools(100, 10).is_empty());
    }
}
