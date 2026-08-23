//! Multi-hop cycle discovery over the pool graph.
//!
//! # What a cycle looks like once there are many venues
//!
//! With one venue per pair, the only closed loops are **triangles** — SOL → USDC →
//! RAY → SOL — and each hop pays that venue's fee. Across four decoders and eighty-odd
//! pools the graph looks different: the same pair is quoted several times over at
//! different fee tiers, so the shortest loop is a **two-hop round trip between two
//! venues on one pair**. SOL → USDC on a 1 bp pool and straight back on a 2 bp pool
//! costs 3 bps of fees, where the same trip through Raydium AMM v4 twice costs 50.
//!
//! Both shapes fall out of the same search, which is why this module special-cases
//! neither.
//!
//! # Anchored versus full search
//!
//! [`enumerate_cycles`] searches only cycles containing one named pool — cheap, and
//! right when reacting to a single account update. But it can only find cycles whose
//! changed pool touches the base mint, so a change to a pool in the *middle* of a
//! triangle is invisible to it.
//!
//! [`enumerate_from_base`] searches every cycle from a base mint regardless of what
//! changed. At this graph size a full sweep costs well under a millisecond, so the
//! live path runs that on a short timer and never has to reason about which updates
//! could have made which cycles profitable.

use std::fmt::Write as _;

use crate::snapshot::Snapshot;
use cb_core::path::{
    cycle_profit, is_profitable, marginal_edge_bps, optimal_input, route_fee_bps, Leg,
};
use cb_core::types::{PoolId, PoolState, Pubkey32};

/// A closed loop of pools starting and ending in the same mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    /// Pools in travel order.
    pub pools: Vec<PoolId>,
    /// Mints visited, starting and ending with the base mint. Length is `pools + 1`.
    pub mints: Vec<Pubkey32>,
    /// Legs oriented in travel order.
    pub legs: Vec<Leg>,
}

impl Cycle {
    #[must_use]
    pub fn hops(&self) -> usize {
        self.pools.len()
    }

    /// Freshness is bounded by the stalest pool in the loop.
    #[must_use]
    pub fn slot(&self, snap: &Snapshot) -> u64 {
        self.pools.iter().filter_map(|p| snap.get(p)).map(|p| p.slot).min().unwrap_or(0)
    }

    /// Total swap fee along the route, in bps.
    #[must_use]
    pub fn fee_bps(&self) -> f64 {
        route_fee_bps(&self.legs)
    }

    /// Identity of this cycle as an *opportunity*, invariant to where the loop is
    /// entered.
    ///
    /// `SOL → USDC → SOL` over pools (P, Q) and `USDC → SOL → USDC` over (Q, P) are
    /// one closed loop entered at two different points. There is a single arbitrage
    /// there, and taking it at either entry removes it from both — so treating the
    /// printed route as the identity counts one opportunity twice.
    ///
    /// Rotation is collapsed. **Direction is not**: going round the same pools the
    /// other way is a different trade, and at most one of the two can be profitable.
    /// So each hop is keyed by the pool *and the mint going into it*, which differs
    /// between the two directions while staying fixed under rotation.
    #[must_use]
    pub fn canonical_key(&self) -> String {
        let n = self.pools.len();
        if n == 0 {
            return String::new();
        }
        // The smallest of all n rotations. Picking the smallest *pool* would be
        // ambiguous if a loop ever used one pool twice; comparing whole rotations is
        // canonical either way, and n is at most 3.
        (0..n)
            .map(|start| {
                let mut out = String::with_capacity(n * 14);
                for k in 0..n {
                    let i = (start + k) % n;
                    if k > 0 {
                        out.push('>');
                    }
                    for b in &self.pools[i].0[..4] {
                        let _ = write!(out, "{b:02x}");
                    }
                    out.push(':');
                    for b in &self.mints[i][..2] {
                        let _ = write!(out, "{b:02x}");
                    }
                }
                out
            })
            .min()
            .unwrap_or_default()
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

impl SurveyedCycle {
    /// The edge with fees added back: how far apart the venues' prices actually are,
    /// before paying to cross them.
    ///
    /// This is the number that says whether an opportunity *exists* and is merely too
    /// expensive to take. Reporting only `edge_bps` conflates "these venues agree on
    /// the price" with "these venues disagree by 40 bps and the fees are 75".
    #[must_use]
    pub fn dislocation_bps(&self) -> f64 {
        self.edge_bps + self.cycle.fee_bps()
    }
}

/// Every closed cycle of 2..=`max_hops` pools starting and ending at `base_mint`.
///
/// Unlike [`enumerate_cycles`] this is not anchored on a changed pool, so it finds
/// cycles whose movement happened anywhere in the loop.
#[must_use]
pub fn enumerate_from_base(snap: &Snapshot, base_mint: &Pubkey32, max_hops: usize) -> Vec<Cycle> {
    let mut found = Vec::new();
    if max_hops < 2 {
        return found;
    }
    for &i in snap.pools_trading(base_mint) {
        let first = snap.at(i);
        let Some(mid) = first.other_mint(base_mint) else { continue };
        let Some(leg) = first.leg_for_input(base_mint) else { continue };
        let mut path =
            Path { pools: vec![first.id], mints: vec![*base_mint, mid], legs: vec![leg] };
        collect(snap, base_mint, &mut path, max_hops, &mut found);
    }
    found
}

/// Every cycle from `base_mint`, priced for its marginal edge. Sorted best first.
#[must_use]
pub fn survey_from_base(
    snap: &Snapshot,
    base_mint: &Pubkey32,
    max_hops: usize,
) -> Vec<SurveyedCycle> {
    let mut out: Vec<SurveyedCycle> = enumerate_from_base(snap, base_mint, max_hops)
        .into_iter()
        .filter_map(|c| {
            marginal_edge_bps(&c.legs).map(|edge_bps| SurveyedCycle { cycle: c, edge_bps })
        })
        .collect();
    out.sort_by(|a, b| b.edge_bps.total_cmp(&a.edge_bps));
    out
}

/// Profitable cycles from `base_mint`, sized against `max_in`. Sorted by profit.
#[must_use]
pub fn find_from_base(
    snap: &Snapshot,
    base_mint: &Pubkey32,
    max_hops: usize,
    max_in: u128,
) -> Vec<PricedCycle> {
    if max_in == 0 {
        return Vec::new();
    }
    let mut out: Vec<PricedCycle> = enumerate_from_base(snap, base_mint, max_hops)
        .into_iter()
        .filter_map(|c| price(&c, max_in))
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.profit));
    out
}

/// Enumerate every cycle through `updated`, profitable or not, with its edge.
///
/// Sorted best-edge-first.
#[must_use]
pub fn survey_cycles(
    snap: &Snapshot,
    base_mint: &Pubkey32,
    updated: &PoolState,
    max_hops: usize,
) -> Vec<SurveyedCycle> {
    let mut out: Vec<SurveyedCycle> = enumerate_cycles(snap, base_mint, updated, max_hops)
        .into_iter()
        .filter_map(|c| {
            marginal_edge_bps(&c.legs).map(|edge_bps| SurveyedCycle { cycle: c, edge_bps })
        })
        .collect();
    out.sort_by(|a, b| b.edge_bps.total_cmp(&a.edge_bps));
    out
}

/// Every closed cycle of 2..=`max_hops` pools through `updated`, unpriced.
///
/// Only finds cycles in which `updated` itself trades the base mint. Use
/// [`enumerate_from_base`] when that restriction matters.
#[must_use]
pub fn enumerate_cycles(
    snap: &Snapshot,
    base_mint: &Pubkey32,
    updated: &PoolState,
    max_hops: usize,
) -> Vec<Cycle> {
    let mut found = Vec::new();
    if max_hops < 2 {
        return found;
    }
    let Some(first_mid) = updated.other_mint(base_mint) else { return found };
    let Some(first_leg) = updated.leg_for_input(base_mint) else { return found };

    let mut path =
        Path { pools: vec![updated.id], mints: vec![*base_mint, first_mid], legs: vec![first_leg] };
    collect(snap, base_mint, &mut path, max_hops, &mut found);
    found
}

fn collect(
    snap: &Snapshot,
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
    for &i in snap.pools_trading(&current) {
        let next = snap.at(i);
        if path.pools.contains(&next.id) {
            continue;
        }
        let Some(next_mint) = next.other_mint(&current) else { continue };
        if next_mint != *base_mint && path.mints.contains(&next_mint) {
            continue;
        }
        let Some(leg) = next.leg_for_input(&current) else { continue };
        path.pools.push(next.id);
        path.mints.push(next_mint);
        path.legs.push(leg);
        collect(snap, base_mint, path, max_hops, out);
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
    snap: &Snapshot,
    base_mint: &Pubkey32,
    updated: &PoolState,
    max_hops: usize,
    max_in: u128,
) -> Vec<PricedCycle> {
    let mut out = Vec::new();
    if max_in == 0 {
        return out;
    }
    for cycle in enumerate_cycles(snap, base_mint, updated, max_hops) {
        if let Some(priced) = price(&cycle, max_in) {
            out.push(priced);
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.profit));
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
        PoolState::constant_product(PoolId([id; 32]), Dex::RaydiumAmmV4, a, b, ra, rb, 2500, 100)
    }

    fn fee_pool(id: u8, a: [u8; 32], b: [u8; 32], ra: u128, rb: u128, ppm: u32) -> PoolState {
        PoolState::constant_product(PoolId([id; 32]), Dex::OrcaWhirlpool, a, b, ra, rb, ppm, 100)
    }

    /// SOL→USDC→RAY→SOL, with the RAY legs mispriced enough to profit.
    fn triangle() -> (Snapshot, PoolState) {
        let p1 = pool(1, SOL, USDC, 1_000_000, 1_000_000); // SOL/USDC
        let p2 = pool(2, RAY, USDC, 1_000_000, 900_000); // RAY cheap in USDC
        let p3 = pool(3, RAY, SOL, 1_000_000, 1_200_000); // RAY dear in SOL
        (Snapshot::new(vec![p1, p2, p3]), p1)
    }

    /// The mirror bug, pinned. Both entries into one loop must key the same, or
    /// every "distinct opportunities" figure doubles.
    #[test]
    fn one_loop_entered_at_two_points_is_one_opportunity() {
        let p1 = fee_pool(1, SOL, USDC, 1_000_000, 1_000_000, 100);
        let p2 = fee_pool(2, SOL, USDC, 1_000_000, 1_010_000, 200);
        let snap = Snapshot::new(vec![p1, p2]);

        // The same round trip, found from each of its two mints.
        let from_sol = find_from_base(&snap, &SOL, 2, u128::MAX);
        let from_usdc = find_from_base(&snap, &USDC, 2, u128::MAX);
        assert!(!from_sol.is_empty() && !from_usdc.is_empty());

        assert_eq!(
            from_sol[0].cycle.canonical_key(),
            from_usdc[0].cycle.canonical_key(),
            "entering the same loop at SOL or at USDC is one arbitrage, not two"
        );
        assert_ne!(
            from_sol[0].cycle.mints[0], from_usdc[0].cycle.mints[0],
            "the two really were found from different base mints"
        );
    }

    /// ...but going round the other way is a different trade, and must not collapse.
    #[test]
    fn the_same_pools_traversed_backwards_is_a_different_opportunity() {
        let p1 = fee_pool(1, SOL, USDC, 1_000_000, 1_000_000, 100);
        let p2 = fee_pool(2, SOL, USDC, 1_000_000, 1_010_000, 200);
        let snap = Snapshot::new(vec![p1, p2]);

        let cycles = enumerate_from_base(&snap, &SOL, 2);
        let keys: std::collections::HashSet<_> =
            cycles.iter().map(super::Cycle::canonical_key).collect();
        assert_eq!(
            keys.len(),
            cycles.len(),
            "buy-on-1-sell-on-2 and buy-on-2-sell-on-1 are separate trades"
        );
        assert!(cycles.len() >= 2, "both directions must be enumerated at all");
    }

    #[test]
    fn finds_a_triangular_cycle() {
        let (snap, updated) = triangle();
        let found = find_cycles(&snap, &SOL, &updated, 3, u128::MAX);
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
        let (snap, updated) = triangle();
        assert!(find_cycles(&snap, &SOL, &updated, 2, u128::MAX).is_empty());
    }

    #[test]
    fn capital_cap_reduces_size_but_keeps_profit_positive() {
        let (snap, updated) = triangle();
        let uncapped = &find_cycles(&snap, &SOL, &updated, 3, u128::MAX)[0];
        let capped_list = find_cycles(&snap, &SOL, &updated, 3, uncapped.optimal_in / 10);
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
        let (snap, updated) = triangle();
        for p in find_cycles(&snap, &SOL, &updated, 4, u128::MAX) {
            let mut seen = p.cycle.pools.clone();
            seen.sort();
            let before = seen.len();
            seen.dedup();
            assert_eq!(before, seen.len(), "pool repeated in cycle: {:?}", p.cycle.pools);
        }
    }

    #[test]
    fn balanced_triangle_yields_nothing() {
        let p1 = pool(1, SOL, USDC, 1_000_000, 1_000_000);
        let snap = Snapshot::new(vec![
            p1,
            pool(2, RAY, USDC, 1_000_000, 1_000_000),
            pool(3, RAY, SOL, 1_000_000, 1_000_000),
        ]);
        assert!(find_cycles(&snap, &SOL, &p1, 3, u128::MAX).is_empty());
    }

    #[test]
    fn ignores_pools_that_cannot_reach_the_base() {
        // A dead-end token hanging off the triangle must not produce a false cycle.
        let (snap, updated) = triangle();
        let mut pools = snap.pools().to_vec();
        pools.push(pool(9, USDC, WIF, 1_000_000, 5_000_000));
        let snap = Snapshot::new(pools);
        for p in find_cycles(&snap, &SOL, &updated, 4, u128::MAX) {
            assert_eq!(p.cycle.mints.last(), Some(&SOL));
            assert!(!p.cycle.mints[1..p.cycle.mints.len() - 1].contains(&SOL));
        }
    }

    #[test]
    fn results_are_sorted_best_first() {
        let (snap, updated) = triangle();
        let found = find_cycles(&snap, &SOL, &updated, 4, u128::MAX);
        for w in found.windows(2) {
            assert!(w[0].profit >= w[1].profit, "results must be sorted by profit");
        }
    }

    #[test]
    fn updated_pool_not_touching_base_yields_nothing() {
        let (snap, _) = triangle();
        let unrelated = pool(9, USDC, WIF, 1_000_000, 5_000_000);
        assert!(find_cycles(&snap, &SOL, &unrelated, 3, u128::MAX).is_empty());
    }

    // ---- full-graph search ----

    /// The blind spot the anchored search has, and the reason `enumerate_from_base`
    /// exists: when the middle pool of a triangle moves, the anchored search cannot
    /// see the cycle at all, because that pool does not trade the base mint.
    #[test]
    fn a_full_sweep_finds_cycles_the_anchored_search_cannot() {
        let (snap, _) = triangle();
        let middle = *snap.get(&PoolId([2; 32])).unwrap(); // RAY/USDC — no SOL in it

        assert!(
            find_cycles(&snap, &SOL, &middle, 3, u128::MAX).is_empty(),
            "anchored search is blind to a pool that does not touch the base mint"
        );
        assert!(
            !find_from_base(&snap, &SOL, 3, u128::MAX).is_empty(),
            "the full sweep must find it"
        );
    }

    #[test]
    fn full_sweep_and_anchored_search_agree_when_both_can_see_a_cycle() {
        let (snap, updated) = triangle();
        let anchored = find_cycles(&snap, &SOL, &updated, 3, u128::MAX);
        let swept = find_from_base(&snap, &SOL, 3, u128::MAX);
        assert_eq!(anchored[0].profit, swept[0].profit, "the same cycle must price the same");
    }

    /// The shape that only appears once several venues quote one pair: a two-hop
    /// round trip. With a 1 bp and a 2 bp pool the whole loop costs 3 bps, so a
    /// dislocation of 11 bps clears easily — the same trip across two 25 bp pools
    /// would need 50 bps and never sees one.
    #[test]
    fn two_venues_on_one_pair_form_the_cheapest_possible_loop() {
        let cheap = fee_pool(1, SOL, USDC, 1_000_000_000, 91_000_000, 100);
        let dear = fee_pool(2, SOL, USDC, 1_000_000_000, 91_100_000, 200); // 11 bps higher
        let snap = Snapshot::new(vec![cheap, dear]);

        let found = find_from_base(&snap, &SOL, 2, u128::MAX);
        assert!(!found.is_empty(), "an 11 bp gap must clear 3 bps of fees");
        assert_eq!(found[0].cycle.hops(), 2);
        assert!((found[0].cycle.fee_bps() - 3.0).abs() < 0.01, "1bp + 2bp = 3bps");

        // The same dislocation across two 25 bp venues is unreachable.
        let v4 = Snapshot::new(vec![
            pool(3, SOL, USDC, 1_000_000_000, 91_000_000),
            pool(4, SOL, USDC, 1_000_000_000, 91_100_000),
        ]);
        assert!(
            find_from_base(&v4, &SOL, 2, u128::MAX).is_empty(),
            "11 bps cannot pay for 50 bps of fees"
        );
    }

    #[test]
    fn dislocation_separates_the_price_gap_from_the_fee_cost() {
        let snap = Snapshot::new(vec![
            fee_pool(1, SOL, USDC, 1_000_000_000, 91_000_000, 100),
            fee_pool(2, SOL, USDC, 1_000_000_000, 91_100_000, 200),
        ]);

        let best = &survey_from_base(&snap, &SOL, 2)[0];
        assert!((best.cycle.fee_bps() - 3.0).abs() < 0.01);
        assert!(
            (best.dislocation_bps() - 10.99).abs() < 0.2,
            "an 11 bp price gap must be reported as such, got {}",
            best.dislocation_bps()
        );
        assert!((best.dislocation_bps() - best.cycle.fee_bps() - best.edge_bps).abs() < 1e-9);
    }

    #[test]
    fn a_full_sweep_of_an_empty_market_is_harmless() {
        let snap = Snapshot::default();
        assert!(enumerate_from_base(&snap, &SOL, 3).is_empty());
        assert!(survey_from_base(&snap, &SOL, 3).is_empty());
        assert!(find_from_base(&snap, &SOL, 3, 1_000).is_empty());
    }
}
