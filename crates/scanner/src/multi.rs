//! Multi-hop cycle discovery over the pool graph.
//!
//! Two-pool cycles need two venues quoting the *same* pair, which is rare among the
//! majors. The cycles that actually exist there are **triangles** — SOL → USDC → RAY
//! → SOL — where three pools that each trade a different pair close a loop.
//!
//! Search is anchored on the pool that just changed: only cycles containing it can
//! have become profitable, so we never re-scan the whole graph.

use crate::store::PoolStore;
use cb_core::path::{cycle_profit, is_profitable, marginal_edge_bps, optimal_input, Leg};
use cb_core::types::{PoolId, PoolState, Pubkey32};

/// A closed loop of pools starting and ending in the same mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    /// Pools in travel order.
    pub pools: Vec<PoolId>,
    /// Mints visited, starting and ending with the base mint. Length is `pools + 1`.
    pub mints: Vec<Pubkey32>,
    /// Reserves oriented in travel order.
    pub legs: Vec<Leg>,
}

impl Cycle {
    #[must_use]
    pub fn hops(&self) -> usize {
        self.pools.len()
    }

    /// Freshness is bounded by the stalest pool in the loop.
    #[must_use]
    pub fn slot(&self, store: &PoolStore) -> u64 {
        self.pools.iter().filter_map(|p| store.get(p)).map(|p| p.slot).min().unwrap_or(0)
    }
}

/// A cycle we have priced.
#[derive(Debug, Clone)]
pub struct PricedCycle {
    pub cycle: Cycle,
    /// Best size ignoring capital, in base-mint units.
    pub optimal_in: u128,
    /// Size we can actually afford.
    pub capped_in: u128,
    /// Profit at `capped_in`.
    pub profit: u128,
    /// Profit at `optimal_in` — the ceiling capital is costing us.
    pub profit_at_optimal: u128,
}

/// A cycle and how far it is from profitable, whether or not it clears.
///
/// Surveying every cycle — not just the winning ones — is what turns "found nothing"
/// into a measurement. A market sitting at −3 bps is tightly arbitraged and fees are
/// the binding constraint; −400 bps means the route is junk. Only one of those is
/// worth spending infrastructure money to chase.
#[derive(Debug, Clone)]
pub struct SurveyedCycle {
    pub cycle: Cycle,
    /// `∏(γᵢ·out/in) − 1` in bps. Positive is profitable.
    pub edge_bps: f64,
}

/// Enumerate every cycle through `updated`, profitable or not, with its edge.
///
/// Sorted best-edge-first.
#[must_use]
pub fn survey_cycles(
    store: &PoolStore,
    base_mint: &Pubkey32,
    updated: &PoolState,
    max_hops: usize,
) -> Vec<SurveyedCycle> {
    let mut out: Vec<SurveyedCycle> = enumerate_cycles(store, base_mint, updated, max_hops)
        .into_iter()
        .filter_map(|c| marginal_edge_bps(&c.legs).map(|edge_bps| SurveyedCycle { cycle: c, edge_bps }))
        .collect();
    out.sort_by(|a, b| b.edge_bps.total_cmp(&a.edge_bps));
    out
}

/// Every closed cycle of 2..=`max_hops` pools through `updated`, unpriced.
#[must_use]
pub fn enumerate_cycles(
    store: &PoolStore,
    base_mint: &Pubkey32,
    updated: &PoolState,
    max_hops: usize,
) -> Vec<Cycle> {
    let mut found = Vec::new();
    if max_hops < 2 {
        return found;
    }
    let Some(first_mid) = updated.other_mint(base_mint) else { return found };
    let Some(first_res) = updated.reserves_for_input(base_mint) else { return found };

    let mut path = Path {
        pools: vec![updated.id],
        mints: vec![*base_mint, first_mid],
        legs: vec![Leg {
            reserve_in: first_res.r_in,
            reserve_out: first_res.r_out,
            fee_bps: updated.fee_bps,
        }],
    };
    collect(store, base_mint, &mut path, max_hops, &mut found);
    found
}

fn collect(
    store: &PoolStore,
    base_mint: &Pubkey32,
    path: &mut Path,
    max_hops: usize,
    out: &mut Vec<Cycle>,
) {
    let current = path.current_mint();
    if current == *base_mint && path.legs.len() >= 2 {
        out.push(path.to_cycle());
        return;
    }
    if path.legs.len() >= max_hops {
        return;
    }
    for next in store.pools_trading(&current) {
        if path.pools.contains(&next.id) {
            continue;
        }
        let Some(next_mint) = next.other_mint(&current) else { continue };
        if next_mint != *base_mint && path.mints.contains(&next_mint) {
            continue;
        }
        let Some(res) = next.reserves_for_input(&current) else { continue };
        path.pools.push(next.id);
        path.mints.push(next_mint);
        path.legs.push(Leg { reserve_in: res.r_in, reserve_out: res.r_out, fee_bps: next.fee_bps });
        collect(store, base_mint, path, max_hops, out);
        path.pools.pop();
        path.mints.pop();
        path.legs.pop();
    }
}

/// Find profitable cycles of 2..=`max_hops` pools that start at `base_mint` and pass
/// through `updated`.
///
/// `max_in` caps trade size to available capital. Cycles are returned sorted by
/// profit, best first.
#[must_use]
pub fn find_cycles(
    store: &PoolStore,
    base_mint: &Pubkey32,
    updated: &PoolState,
    max_hops: usize,
    max_in: u128,
) -> Vec<PricedCycle> {
    let mut out = Vec::new();
    if max_in == 0 {
        return out;
    }
    for cycle in enumerate_cycles(store, base_mint, updated, max_hops) {
        if let Some(priced) = price(&cycle, max_in) {
            out.push(priced);
        }
    }
    out.sort_by(|a, b| b.profit.cmp(&a.profit));
    out
}

struct Path {
    pools: Vec<PoolId>,
    mints: Vec<Pubkey32>,
    legs: Vec<Leg>,
}

impl Path {
    fn current_mint(&self) -> Pubkey32 {
        *self.mints.last().expect("path always has a mint")
    }
    fn to_cycle(&self) -> Cycle {
        Cycle { pools: self.pools.clone(), mints: self.mints.clone(), legs: self.legs.clone() }
    }
}

fn price(cycle: &Cycle, max_in: u128) -> Option<PricedCycle> {
    if !is_profitable(&cycle.legs) {
        return None;
    }
    let optimal_in = optimal_input(&cycle.legs, u128::MAX)?;
    let profit_at_optimal = cycle_profit(&cycle.legs, optimal_in).unwrap_or(0);

    let capped_in = optimal_input(&cycle.legs, max_in)?;
    let profit = cycle_profit(&cycle.legs, capped_in)?;
    if profit == 0 {
        return None;
    }

    Some(PricedCycle { cycle: cycle.clone(), optimal_in, capped_in, profit, profit_at_optimal })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::types::{Dex, PoolId, PoolState};

    const SOL: [u8; 32] = [1; 32];
    const USDC: [u8; 32] = [2; 32];
    const RAY: [u8; 32] = [3; 32];
    const WIF: [u8; 32] = [4; 32];

    fn pool(id: u8, a: [u8; 32], b: [u8; 32], ra: u128, rb: u128) -> PoolState {
        PoolState {
            id: PoolId([id; 32]),
            dex: Dex::RaydiumAmmV4,
            mint_a: a,
            mint_b: b,
            reserve_a: ra,
            reserve_b: rb,
            fee_bps: 25,
            slot: 100,
        }
    }

    /// SOL→USDC→RAY→SOL, with the RAY legs mispriced enough to profit.
    fn triangle_store() -> (PoolStore, PoolState) {
        let s = PoolStore::new();
        let p1 = pool(1, SOL, USDC, 1_000_000, 1_000_000); // SOL/USDC
        let p2 = pool(2, RAY, USDC, 1_000_000, 900_000); // RAY cheap in USDC
        let p3 = pool(3, RAY, SOL, 1_000_000, 1_200_000); // RAY dear in SOL
        s.upsert(p1);
        s.upsert(p2);
        s.upsert(p3);
        (s, p1)
    }

    #[test]
    fn finds_a_triangular_cycle() {
        let (store, updated) = triangle_store();
        let found = find_cycles(&store, &SOL, &updated, 3, u128::MAX);
        assert!(!found.is_empty(), "a mispriced triangle must be found");

        let best = &found[0];
        assert_eq!(best.cycle.hops(), 3);
        assert_eq!(best.cycle.mints.first(), Some(&SOL));
        assert_eq!(best.cycle.mints.last(), Some(&SOL), "cycle must return to base");
        assert!(best.profit > 0);
        assert!(best.capped_in > 0);
    }

    #[test]
    fn max_hops_two_finds_nothing_in_a_triangle() {
        // Guards against the search silently taking shortcuts it should not.
        let (store, updated) = triangle_store();
        assert!(find_cycles(&store, &SOL, &updated, 2, u128::MAX).is_empty());
    }

    #[test]
    fn capital_cap_reduces_size_but_keeps_profit_positive() {
        let (store, updated) = triangle_store();
        let uncapped = &find_cycles(&store, &SOL, &updated, 3, u128::MAX)[0];
        let capped_list = find_cycles(&store, &SOL, &updated, 3, uncapped.optimal_in / 10);
        let capped = &capped_list[0];

        assert!(capped.capped_in <= uncapped.optimal_in / 10);
        assert!(capped.profit > 0, "a capped trade must still profit");
        assert!(capped.profit < uncapped.profit, "the cap must cost us profit");
        assert_eq!(
            capped.profit_at_optimal, uncapped.profit_at_optimal,
            "the unconstrained ceiling is a property of the pools, not of our capital"
        );
    }

    #[test]
    fn a_pool_is_never_used_twice_in_one_cycle() {
        let (store, updated) = triangle_store();
        for p in find_cycles(&store, &SOL, &updated, 4, u128::MAX) {
            let mut seen = p.cycle.pools.clone();
            seen.sort();
            let before = seen.len();
            seen.dedup();
            assert_eq!(before, seen.len(), "pool repeated in cycle: {:?}", p.cycle.pools);
        }
    }

    #[test]
    fn balanced_triangle_yields_nothing() {
        let s = PoolStore::new();
        let p1 = pool(1, SOL, USDC, 1_000_000, 1_000_000);
        s.upsert(p1);
        s.upsert(pool(2, RAY, USDC, 1_000_000, 1_000_000));
        s.upsert(pool(3, RAY, SOL, 1_000_000, 1_000_000));
        assert!(find_cycles(&s, &SOL, &p1, 3, u128::MAX).is_empty());
    }

    #[test]
    fn ignores_pools_that_cannot_reach_the_base() {
        // A dead-end token hanging off the triangle must not produce a false cycle.
        let (store, updated) = triangle_store();
        store.upsert(pool(9, USDC, WIF, 1_000_000, 5_000_000));
        for p in find_cycles(&store, &SOL, &updated, 4, u128::MAX) {
            assert_eq!(p.cycle.mints.last(), Some(&SOL));
            assert!(!p.cycle.mints[1..p.cycle.mints.len() - 1].contains(&SOL));
        }
    }

    #[test]
    fn results_are_sorted_best_first() {
        let (store, updated) = triangle_store();
        let found = find_cycles(&store, &SOL, &updated, 4, u128::MAX);
        for w in found.windows(2) {
            assert!(w[0].profit >= w[1].profit, "results must be sorted by profit");
        }
    }

    #[test]
    fn updated_pool_not_touching_base_yields_nothing() {
        let (store, _) = triangle_store();
        let unrelated = pool(9, USDC, WIF, 1_000_000, 5_000_000);
        store.upsert(unrelated);
        assert!(find_cycles(&store, &SOL, &unrelated, 3, u128::MAX).is_empty());
    }
}
