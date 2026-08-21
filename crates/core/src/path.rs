//! Multi-hop cycles: chained quotes and numeric optimal sizing.
//!
//! The two-pool closed form in [`crate::amm`] does not extend cleanly past two hops,
//! and it only applies to constant-product pools. This module takes the general route:
//! evaluate the exact chained quote, and find the optimum by search.
//!
//! That is slower than a closed form but it is *correct for any pool type*, which
//! matters because concentrated-liquidity (Orca, Raydium CLMM) and bin (Meteora DLMM)
//! quotes are piecewise and have no global closed form at all. When those land, only
//! [`Leg::quote`] changes; the search is unaffected.
//!
//! # Why search is safe here
//!
//! Profit as a function of input size is **unimodal** on a cycle: it rises while the
//! marginal output exceeds the marginal input, then falls once your own trade has
//! moved the price against you. Ternary search converges on the maximum of a unimodal
//! function, and each iteration discards a third of the interval.

use crate::amm::{cp_swap_out, FeeBps};

/// One hop of a cycle, oriented in the direction of travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    /// Reserve of the token we spend on this hop.
    pub reserve_in: u128,
    /// Reserve of the token we receive on this hop.
    pub reserve_out: u128,
    pub fee_bps: FeeBps,
}

impl Leg {
    #[must_use]
    pub fn quote(&self, amount_in: u128) -> Option<u128> {
        cp_swap_out(amount_in, self.reserve_in, self.reserve_out, self.fee_bps)
    }
}

/// Run `amount_in` through every leg in order. Returns the final output amount.
///
/// Returns `None` if any leg fails (empty reserve, overflow, or a quote that would
/// drain the pool).
#[must_use]
pub fn chain_quote(legs: &[Leg], amount_in: u128) -> Option<u128> {
    let mut amount = amount_in;
    for leg in legs {
        amount = leg.quote(amount)?;
    }
    Some(amount)
}

/// Profit of running `amount_in` around a closed cycle, in input-token base units.
///
/// The cycle must start and end in the same token — the caller is responsible for
/// that; this only checks that the round trip came back with more than it started.
#[must_use]
pub fn cycle_profit(legs: &[Leg], amount_in: u128) -> Option<u128> {
    chain_quote(legs, amount_in)?.checked_sub(amount_in)
}

/// Cheap test for whether a cycle is profitable at *any* size.
///
/// Compares the product of marginal (infinitesimal-size) exchange rates against 1.
/// At the limit, a hop's marginal rate is `γ · reserve_out / reserve_in`, so the
/// cycle is worth exploring iff `∏ (γᵢ · out_i / in_i) > 1`.
///
/// Rearranged to avoid division and stay in integers:
/// `∏ (γᵢ · out_i) > ∏ (in_i · BPS)`.
///
/// This is the same test the two-pool closed form uses, generalised to N hops. It is
/// a *necessary* condition, and cheap enough to run on every candidate before paying
/// for a search.
#[must_use]
pub fn is_profitable(legs: &[Leg]) -> bool {
    if legs.is_empty() {
        return false;
    }
    // Accumulate as f64 in log space? No — precision matters and reserves are large.
    // Instead accumulate the ratio as a rational, reducing each step to stay bounded.
    let mut num: u128 = 1; // ∏ γᵢ · out_i
    let mut den: u128 = 1; // ∏ in_i · BPS

    for leg in legs {
        if leg.reserve_in == 0 || leg.reserve_out == 0 {
            return false;
        }
        let Some(gamma) = 10_000u128.checked_sub(u128::from(leg.fee_bps)) else {
            return false;
        };
        let Some(n) = gamma.checked_mul(leg.reserve_out) else { return false };
        let Some(d) = leg.reserve_in.checked_mul(10_000) else { return false };

        // Multiply into the running ratio, reducing by the gcd each step so the
        // accumulators cannot overflow across an arbitrary number of hops.
        let (n, d) = (n, d);
        let g1 = gcd(num, d);
        let g2 = gcd(n, den);
        let Some(new_num) = (num / g1).checked_mul(n / g2) else { return false };
        let Some(new_den) = (den / g2).checked_mul(d / g1) else { return false };
        num = new_num;
        den = new_den;
    }
    num > den
}

/// How far a cycle is from breaking even, in basis points, at infinitesimal size.
///
/// This is `∏ (γᵢ · out_i / in_i) − 1`, expressed in bps. Positive means profitable;
/// negative means the round trip loses that much to fees and adverse pricing.
///
/// This exists because "no opportunity found" is an uninformative result. Knowing the
/// market sits at −3 bps tells you fees are the binding constraint and the venues are
/// tightly arbitraged; −250 bps tells you the route is junk. Both are *measurements*.
/// The paper-trading phase is meant to produce numbers, not silence.
///
/// Uses `f64` deliberately: this is a reporting statistic, never a trade decision.
/// Trade decisions go through [`is_profitable`] and [`optimal_input`], which are exact.
#[must_use]
pub fn marginal_edge_bps(legs: &[Leg]) -> Option<f64> {
    if legs.is_empty() {
        return None;
    }
    let mut ratio = 1.0f64;
    for leg in legs {
        if leg.reserve_in == 0 || leg.reserve_out == 0 {
            return None;
        }
        let gamma = (10_000.0 - f64::from(leg.fee_bps)) / 10_000.0;
        ratio *= gamma * (leg.reserve_out as f64) / (leg.reserve_in as f64);
    }
    Some((ratio - 1.0) * 10_000.0)
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

/// Largest input worth considering: you can never sensibly put in more than the
/// first pool's input reserve, and going far past it only wastes search iterations.
#[must_use]
pub fn search_ceiling(legs: &[Leg]) -> u128 {
    legs.first().map_or(0, |l| l.reserve_in)
}

/// Find the input size that maximises cycle profit, by ternary search.
///
/// `max_in` caps the search — pass your available capital to get the best size you
/// can *actually* trade, or [`search_ceiling`] for the unconstrained optimum.
///
/// Returns `None` if no size in range profits.
#[must_use]
pub fn optimal_input(legs: &[Leg], max_in: u128) -> Option<u128> {
    if legs.is_empty() || max_in == 0 || !is_profitable(legs) {
        return None;
    }
    let mut lo: u128 = 1;
    let mut hi: u128 = max_in.min(search_ceiling(legs)).max(1);

    // Each iteration removes a third of the interval; 200 is far more than enough to
    // converge on a u128 range, and the loop exits early once the interval is tiny.
    for _ in 0..200 {
        if hi <= lo + 2 {
            break;
        }
        let third = (hi - lo) / 3;
        let m1 = lo + third;
        let m2 = hi - third;
        let p1 = cycle_profit(legs, m1).unwrap_or(0);
        let p2 = cycle_profit(legs, m2).unwrap_or(0);
        if p1 < p2 {
            lo = m1 + 1;
        } else {
            hi = m2 - 1;
        }
    }

    // Ternary search brackets the optimum; check the few remaining candidates exactly.
    let mut best: Option<(u128, u128)> = None;
    for x in lo..=hi.min(lo.saturating_add(4)) {
        if let Some(p) = cycle_profit(legs, x) {
            if p > 0 && best.is_none_or(|(_, bp)| p > bp) {
                best = Some((x, p));
            }
        }
    }
    best.map(|(x, _)| x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::{optimal_input as closed_form, CycleReserves};

    fn leg(r_in: u128, r_out: u128) -> Leg {
        Leg { reserve_in: r_in, reserve_out: r_out, fee_bps: 25 }
    }

    #[test]
    fn chain_of_one_leg_matches_a_plain_swap() {
        let l = leg(1_000_000, 1_000_000);
        assert_eq!(chain_quote(&[l], 1000), l.quote(1000));
    }

    #[test]
    fn balanced_two_hop_cycle_is_not_profitable() {
        let legs = [leg(1_000_000, 1_000_000), leg(1_000_000, 1_000_000)];
        assert!(!is_profitable(&legs));
        assert_eq!(optimal_input(&legs, 1_000_000), None);
    }

    /// Cross-validation: on two constant-product pools the numeric search must agree
    /// with the independently-derived closed form. If these ever diverge, one of them
    /// is wrong and the money is real.
    #[test]
    fn search_agrees_with_the_closed_form_on_two_hops() {
        let cases = [
            (1_000_000u128, 1_000_000u128, 1_000_000u128, 1_300_000u128),
            (5_000_000, 5_000_000, 5_000_000, 6_500_000),
            (138_186_000_000, 1_800_000_000_000, 4_200_000_000_000, 335_412_000_000),
        ];
        for (a_in, a_out, b_in, b_out) in cases {
            let cr = CycleReserves { a_in, a_out, b_in, b_out, fee_a_bps: 25, fee_b_bps: 25 };
            let legs = [leg(a_in, a_out), leg(b_in, b_out)];

            assert_eq!(
                is_profitable(&legs),
                crate::amm::is_profitable(&cr),
                "profitability test disagrees for {a_in}/{a_out}/{b_in}/{b_out}"
            );

            let Some(exact) = closed_form(&cr) else { continue };
            let found = optimal_input(&legs, u128::MAX).expect("search must find it too");

            let p_exact = cycle_profit(&legs, exact).unwrap_or(0);
            let p_found = cycle_profit(&legs, found).unwrap_or(0);
            // Sizes may differ by rounding; the profit achieved must not.
            assert!(
                p_found + 2 >= p_exact,
                "search profit {p_found} materially below closed form {p_exact}"
            );
        }
    }

    #[test]
    fn finds_a_profitable_triangle() {
        // A→B→C→A where the round trip returns more than it started with.
        let legs = [
            leg(1_000_000, 1_050_000),
            leg(1_000_000, 1_050_000),
            leg(1_000_000, 1_050_000),
        ];
        assert!(is_profitable(&legs), "15% of headroom must clear two-and-a-half lots of fees");
        let x = optimal_input(&legs, u128::MAX).expect("must size the triangle");
        let p = cycle_profit(&legs, x).expect("must profit");
        assert!(p > 0);

        for pct in [40u128, 70, 90, 110, 150, 250] {
            let probe = x * pct / 100;
            if probe == 0 {
                continue;
            }
            if let Some(other) = cycle_profit(&legs, probe) {
                assert!(other <= p + 2, "size at {pct}% beat the optimum ({other} > {p})");
            }
        }
    }

    #[test]
    fn capital_cap_limits_the_size_returned() {
        let legs = [leg(1_000_000, 1_050_000), leg(1_000_000, 1_050_000), leg(1_000_000, 1_050_000)];
        let unconstrained = optimal_input(&legs, u128::MAX).unwrap();
        let capped = optimal_input(&legs, unconstrained / 4).unwrap();
        assert!(capped <= unconstrained / 4, "must respect the cap");
        assert!(cycle_profit(&legs, capped).unwrap() > 0, "a capped trade must still profit");
    }

    #[test]
    fn a_cycle_that_only_loses_returns_none() {
        // Heavy fees against a tiny edge.
        let legs = [
            Leg { reserve_in: 1_000_000, reserve_out: 1_001_000, fee_bps: 300 },
            Leg { reserve_in: 1_000_000, reserve_out: 1_001_000, fee_bps: 300 },
        ];
        assert!(!is_profitable(&legs));
        assert_eq!(optimal_input(&legs, u128::MAX), None);
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        assert!(!is_profitable(&[]));
        assert_eq!(optimal_input(&[], 100), None);
        assert!(!is_profitable(&[leg(0, 100)]));
        assert!(!is_profitable(&[leg(100, 0)]));
        assert_eq!(optimal_input(&[leg(1_000_000, 2_000_000)], 0), None);
    }

    #[test]
    fn handles_mainnet_scale_reserves_without_overflow() {
        // The same class of bug that made the u128 closed form silently return None.
        let legs = [
            leg(5_932_889_497_097, 66_664_588_573_034),
            leg(66_664_588_573_034, 5_932_889_497_097),
        ];
        let _ = is_profitable(&legs);
        let _ = optimal_input(&legs, 5_000_000);
    }

    #[test]
    fn marginal_edge_agrees_with_the_exact_profitability_test() {
        // The reporting statistic and the exact integer test must never disagree
        // about the sign, or the dashboard would contradict the trading logic.
        let cases: Vec<Vec<Leg>> = vec![
            vec![leg(1_000_000, 1_050_000), leg(1_000_000, 1_050_000), leg(1_000_000, 1_050_000)],
            vec![leg(1_000_000, 1_000_000), leg(1_000_000, 1_000_000)],
            vec![leg(1_000_000, 1_000_000), leg(1_000_000, 1_300_000)],
            vec![leg(138_186_000_000, 1_800_000_000_000), leg(4_200_000_000_000, 335_412_000_000)],
        ];
        for legs in cases {
            let edge = marginal_edge_bps(&legs).unwrap();
            assert_eq!(
                edge > 0.0,
                is_profitable(&legs),
                "edge {edge}bps disagrees with the exact test for {legs:?}"
            );
        }
    }

    #[test]
    fn balanced_two_hop_edge_is_exactly_the_fee_cost() {
        // Two 25bp pools with no dislocation: the round trip loses ~50bps.
        let legs = [leg(1_000_000, 1_000_000), leg(1_000_000, 1_000_000)];
        let edge = marginal_edge_bps(&legs).unwrap();
        assert!((edge + 49.94).abs() < 0.1, "expected about -49.94bps, got {edge}");
    }

    #[test]
    fn marginal_edge_handles_degenerate_input() {
        assert!(marginal_edge_bps(&[]).is_none());
        assert!(marginal_edge_bps(&[leg(0, 100)]).is_none());
    }

    #[test]
    fn four_hop_cycles_work_too() {
        let legs = [
            leg(1_000_000, 1_040_000),
            leg(1_000_000, 1_040_000),
            leg(1_000_000, 1_040_000),
            leg(1_000_000, 1_040_000),
        ];
        assert!(is_profitable(&legs));
        assert!(optimal_input(&legs, u128::MAX).is_some());
    }
}
