//! A consistent, indexed view of the market, taken once per search pass.
//!
//! The live store is a concurrent map that the feed writes to continuously. Walking
//! it directly during a cycle search has two problems: every `pools_trading` call
//! rescans the whole map, and the graph can change underneath the walk, so a cycle
//! can be built from two pools that were never simultaneously true.
//!
//! Taking one snapshot per pass fixes both. The scan cost becomes O(pools) per pass
//! instead of O(pools) per graph step, and every cycle in a pass is priced against
//! one coherent picture of the market.

use cb_core::types::{PoolId, PoolState, Pubkey32};
use std::collections::HashMap;

/// An immutable view of every pool, indexed by the mints it trades.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pools: Vec<PoolState>,
    by_mint: HashMap<Pubkey32, Vec<usize>>,
    by_id: HashMap<PoolId, usize>,
}

impl Snapshot {
    #[must_use]
    pub fn new(pools: Vec<PoolState>) -> Self {
        let mut by_mint: HashMap<Pubkey32, Vec<usize>> = HashMap::new();
        let mut by_id = HashMap::with_capacity(pools.len());
        for (i, p) in pools.iter().enumerate() {
            by_mint.entry(p.mint_a).or_default().push(i);
            by_mint.entry(p.mint_b).or_default().push(i);
            by_id.insert(p.id, i);
        }
        Self { pools, by_mint, by_id }
    }

    /// Every pool that trades `mint`, in either position.
    #[must_use]
    pub fn pools_trading(&self, mint: &Pubkey32) -> &[usize] {
        self.by_mint.get(mint).map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn at(&self, index: usize) -> &PoolState {
        &self.pools[index]
    }

    #[must_use]
    pub fn get(&self, id: &PoolId) -> Option<&PoolState> {
        self.by_id.get(id).map(|&i| &self.pools[i])
    }

    #[must_use]
    pub fn pools(&self) -> &[PoolState] {
        &self.pools
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pools.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    /// Number of distinct mints in the graph.
    #[must_use]
    pub fn mint_count(&self) -> usize {
        self.by_mint.len()
    }

    /// Oldest slot in the snapshot. A cycle can be no fresher than this.
    #[must_use]
    pub fn oldest_slot(&self) -> u64 {
        self.pools.iter().map(|p| p.slot).min().unwrap_or(0)
    }

    #[must_use]
    pub fn newest_slot(&self) -> u64 {
        self.pools.iter().map(|p| p.slot).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::types::{Dex, PoolId, PoolState};

    fn pool(id: u8, a: u8, b: u8, slot: u64) -> PoolState {
        PoolState::constant_product(
            PoolId([id; 32]),
            Dex::RaydiumAmmV4,
            [a; 32],
            [b; 32],
            1_000_000,
            1_000_000,
            2500,
            slot,
        )
    }

    #[test]
    fn indexes_pools_by_both_mints() {
        let s = Snapshot::new(vec![pool(1, 10, 20, 1), pool(2, 20, 30, 1), pool(3, 40, 50, 1)]);
        assert_eq!(s.pools_trading(&[20; 32]).len(), 2, "mint 20 is in two pools");
        assert_eq!(s.pools_trading(&[10; 32]).len(), 1);
        assert_eq!(s.pools_trading(&[99; 32]).len(), 0, "unknown mint must not panic");
        assert_eq!(s.mint_count(), 5, "mints 10,20,30,40,50");
    }

    #[test]
    fn looks_pools_up_by_id() {
        let s = Snapshot::new(vec![pool(1, 10, 20, 7)]);
        assert_eq!(s.get(&PoolId([1; 32])).unwrap().slot, 7);
        assert!(s.get(&PoolId([9; 32])).is_none());
    }

    #[test]
    fn slot_range_bounds_the_freshness_of_any_cycle() {
        let s = Snapshot::new(vec![pool(1, 10, 20, 100), pool(2, 20, 30, 250)]);
        assert_eq!(s.oldest_slot(), 100);
        assert_eq!(s.newest_slot(), 250);
    }

    #[test]
    fn an_empty_snapshot_is_harmless() {
        let s = Snapshot::default();
        assert!(s.is_empty());
        assert_eq!(s.oldest_slot(), 0);
        assert_eq!(s.pools_trading(&[1; 32]).len(), 0);
    }
}
