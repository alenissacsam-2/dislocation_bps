//! Multi-hop cycles: chained quotes and numeric optimal sizing.
//!
//! The two-pool closed form in [`crate::amm`] does not extend cleanly past two hops,
//! so this module takes the general route: evaluate the exact chained quote, and find
//! the optimum by search. Slower than a closed form, but it works for any number of
//! hops, and it cross-validates against the closed form on two.
//!
//! # One leg type, two pool families
//!
//! Every leg here is a constant-product leg. That is not a limitation: as
//! [`crate::clmm`] shows, a concentrated-liquidity pool inside its current tick *is*
//! a constant-product pool over virtual reserves `L/√P` and `L·√P`. Orca Whirlpool
//! and Raydium CLMM legs arrive here already converted, carrying a [`Leg::max_in`]
//! that says how much size the current tick can absorb before that equivalence
//! stops holding.
//!
//! # Why search is safe here
//!
//! Profit as a function of input size is **unimodal** on a cycle: it rises while the
//! marginal output exceeds the marginal input, then falls once your own trade has
//! moved the price against you. Ternary search converges on the maximum of a unimodal
//! function, and each iteration discards a third of the interval.
//!
//! Feasibility — whether every leg stays inside its `max_in` — is **monotone** in
//! size, since a bigger input produces a bigger amount at every downstream leg. So
//! the search first binary-searches the largest feasible size, then ternary-searches
//! inside it. Treating an infeasible size as "zero profit" instead would put a cliff
//! in the middle of the interval and break the unimodality the search depends on.

use crate::amm::{cp_swap_out, FeePpm};

/// One hop of a cycle, oriented in the direction of travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    /// Reserve of the token we spend on this hop. Virtual, for a CLMM leg.
    pub reserve_in: u128,
    /// Reserve of the token we receive on this hop. Virtual, for a CLMM leg.
    pub reserve_out: u128,
    pub fee_ppm: FeePpm,
    /// Largest input for which this leg's quote is exact.
    ///
    /// `u128::MAX` for a true constant-product pool, whose curve holds at any size.
    /// For a concentrated-liquidity leg this is the depth of the current tick: past
    /// it the pool's liquidity changes and the constant-product equivalence breaks,
    /// so the quote is refused rather than extrapolated.
    pub max_in: u128,
}

impl Leg {
    /// A constant-product leg, exact at any size.
    #[must_use]
    pub fn cp(reserve_in: u128, reserve_out: u128, fee_ppm: FeePpm) -> Self {
        Self { reserve_in, reserve_out, fee_ppm, max_in: u128::MAX }
    }

    /// A leg whose quote is only exact up to `max_in`.
    #[must_use]
    pub fn bounded(reserve_in: u128, reserve_out: u128, fee_ppm: FeePpm, max_in: u128) -> Self {
        Self { reserve_in, reserve_out, fee_ppm, max_in }
    }

    #[must_use]
    pub fn quote(&self, amount_in: u128) -> Option<u128> {
        if amount_in > self.max_in {
            return None;
        }
        cp_swap_out(amount_in, self.reserve_in, self.reserve_out, self.fee_ppm)
    }
}

/// Run `amount_in` through every leg in order. Returns the final output amount.
///
/// Returns `None` if any leg fails — empty reserve, overflow, a quote that would
/// drain the pool, or a size past a leg's [`Leg::max_in`].
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
/// `∏ (γᵢ · out_i) > ∏ (in_i · PPM)`.
///
/// This is a *necessary* condition and ignores `max_in`, deliberately: a cycle whose
/// marginal rates lose money loses money at every size, but one that clears here may
/// still turn out to be unsizeable. The sizing step is where that gets decided.
#[must_use]
pub fn is_profitable(legs: &[Leg]) -> bool {
    if legs.is_empty() {
        return false;
    }
    // Accumulate the ratio as a rational, reducing each step to stay bounded.
    let mut num: u128 = 1; // ∏ γᵢ · out_i
    let mut den: u128 = 1; // ∏ in_i · PPM

    for leg in legs {
        if leg.reserve_in == 0 || leg.reserve_out == 0 {
            return false;
        }
        let Some(gamma) = 1_000_000u128.checked_sub(u128::from(leg.fee_ppm)) else {
            return false;
        };
        let Some(n) = gamma.checked_mul(leg.reserve_out) else { return false };
        let Some(d) = leg.reserve_in.checked_mul(1_000_000) else { return false };

        // Multiply into the running ratio, reducing by the gcd each step so the
        // accumulators cannot overflow across an arbitrary number of hops.
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
        let gamma = (1_000_000.0 - f64::from(leg.fee_ppm)) / 1_000_000.0;
        ratio *= gamma * (leg.reserve_out as f64) / (leg.reserve_in as f64);
    }
    Some((ratio - 1.0) * 10_000.0)
}

/// Total fee cost of a route in basis points — what the edge has to beat.
///
/// `1 − ∏(1 − feeᵢ)`, in bps. Reported next to [`marginal_edge_bps`] so a negative
/// edge decomposes into "the price dislocation was X, the fees were Y" instead of
/// arriving as one number with no explanation.
#[must_use]
pub fn route_fee_bps(legs: &[Leg]) -> f64 {
    let kept: f64 = legs
        .iter()
        .map(|l| (1_000_000.0 - f64::from(l.fee_ppm)) / 1_000_000.0)
        .product();
    (1.0 - kept) * 10_000.0
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

/// Largest input worth considering: never more than the first pool's input reserve,
/// and never past the first leg's own exactness bound.
#[must_use]
pub fn search_ceiling(legs: &[Leg]) -> u128 {
    legs.first().map_or(0, |l| l.reserve_in.min(l.max_in))
}

/// Largest input the whole cycle can absorb while every leg still quotes exactly,
/// in units of the base (first leg's input) mint.
///
/// [`search_ceiling`] answers this for the *first* leg only, which is the wrong
/// question. The binding constraint is usually downstream: a cycle whose second hop
/// sits at the end of its tick has almost no capacity no matter how deep the pool you
/// enter through, and reporting the entry pool's depth makes it look tradeable when
/// it is not. That gap is exactly how a route with no size behind it can lead a
/// leaderboard ranked by marginal rate.
///
/// Each leg's own capacity is `min(max_in, reserve_in)`, denominated in *that leg's*
/// input mint. To compare them they are converted back to base units through the
/// marginal rates of the legs ahead of them:
///
/// ```text
/// depth_base = min_i ( cap_i / prod_{j<i} r_j ),   r_j = gamma_j * out_j / in_j
/// ```
///
/// Exact to first order, one pass, no iteration. It is deliberately an *upper* bound
/// on tradeable size: a real fill moves the price against itself, so [`optimal_input`]
/// — which solves the composed curve exactly — always sizes at or under this. A depth
/// figure that errs high is honest about capacity while the sizing stays exact.
///
/// `f64` for the same reason as [`marginal_edge_bps`]: this is a reporting statistic,
/// never a trade decision.
#[must_use]
pub fn cycle_depth_base(legs: &[Leg]) -> u128 {
    if legs.is_empty() {
        return 0;
    }
    // Rate from one base unit into the current leg's input mint.
    let mut rate = 1.0f64;
    let mut depth = f64::INFINITY;

    for leg in legs {
        if leg.reserve_in == 0 || leg.reserve_out == 0 || rate <= 0.0 || rate.is_nan() {
            return 0;
        }
        let cap = leg.max_in.min(leg.reserve_in) as f64;
        depth = depth.min(cap / rate);
        let gamma = (1_000_000.0 - f64::from(leg.fee_ppm)) / 1_000_000.0;
        rate *= gamma * (leg.reserve_out as f64) / (leg.reserve_in as f64);
    }

    if depth.is_nan() || depth <= 0.0 {
        return 0;
    }
    if depth >= u128::MAX as f64 {
        return u128::MAX;
    }
    depth as u128
}

/// Largest size at which every leg still quotes, at or below `hi`.
///
/// Feasibility is monotone in size, so this is a plain binary search. Returns 0 when
/// even the smallest size fails.
#[must_use]
pub fn largest_feasible(legs: &[Leg], hi: u128) -> u128 {
    if hi == 0 {
        return 0;
    }
    if chain_quote(legs, hi).is_some() {
        return hi;
    }
    let (mut lo, mut hi) = (0u128, hi);
    // Invariant: `lo` is feasible or zero, `hi` is infeasible.
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if chain_quote(legs, mid).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Find the input size that maximises cycle profit, by ternary search.
///
/// `max_in` caps the search — pass your available capital to get the best size you
/// can *actually* trade, or [`u128::MAX`] for the unconstrained optimum.
///
/// Returns `None` if no size in range profits.
#[must_use]
pub fn optimal_input(legs: &[Leg], max_in: u128) -> Option<u128> {
    if legs.is_empty() || max_in == 0 || !is_profitable(legs) {
        return None;
    }
    let ceiling = largest_feasible(legs, max_in.min(search_ceiling(legs)).max(1));
    if ceiling == 0 {
        return None;
    }
    let mut lo: u128 = 1;
    let mut hi: u128 = ceiling;

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

/// Split `amount_in` across **parallel** legs to maximise total output.
///
/// Parallel means what the routing literature calls a hop group: several pools quoting
/// the same pair in the same direction, so a trade may use any mixture of them. This is
/// the opposite of [`chain_quote`], which runs one amount through legs in series.
///
/// # Why splitting beats picking the best pool
///
/// Output is concave in size on every leg, so the marginal rate a pool offers *falls*
/// as you push more through it. Sending the whole trade to the pool with the best
/// headline price therefore rides that pool down its own curve while better marginal
/// prices sit unused next door. The optimum instead **equalises the marginal rate
/// across every pool that receives anything** — the classic water-filling condition,
/// and the same stationarity condition the CFMM-routing literature derives from convex
/// duality. It is strictly better than any single-pool route whenever a second pool
/// quotes the same pair at a comparable price, and never worse: with one pool, or with
/// rivals priced far apart, it degenerates to exactly the single-pool answer.
///
/// This matters here specifically because cycle capacity is set by the *thinnest* leg.
/// Splitting a hop across four pools raises that hop's usable depth toward the sum of
/// theirs, which is the only lever that moves the depth ceiling — and depth, not
/// capital, is what these cycles run out of.
///
/// # Method
///
/// For a constant-product leg with fee factor `γ`, output is `γ·x·Rout / (Rin + γ·x)`,
/// whose derivative is `γ·Rin·Rout / (Rin + γ·x)²`. Setting that equal to a common
/// marginal rate `λ` inverts in closed form:
///
/// ```text
/// x(λ) = (√(γ·Rin·Rout / λ) − Rin) / γ
/// ```
///
/// Each `x(λ)` is decreasing in `λ`, so the total is too, and a bisection on `λ` finds
/// the allocation summing to `amount_in`. No iterative solver and no convex-program
/// dependency — the general algorithms in the literature buy generality across pool
/// types we do not have.
///
/// Returns allocations in the same order as `legs`, summing exactly to `amount_in`.
/// `None` if the legs cannot absorb it — their `max_in` bounds sum to less than asked,
/// which is a refusal to quote rather than an extrapolation past a tick.
#[must_use]
pub fn split_across(legs: &[Leg], amount_in: u128) -> Option<Vec<u128>> {
    if legs.is_empty() {
        return None;
    }
    if amount_in == 0 {
        return Some(vec![0; legs.len()]);
    }
    if legs.len() == 1 {
        return (amount_in <= legs[0].max_in).then(|| vec![amount_in]);
    }

    // Capacity check first: a group that cannot take the trade must refuse it, exactly
    // as a single leg past its tick does.
    let capacity: u128 = legs.iter().map(|l| l.max_in).fold(0u128, u128::saturating_add);
    if capacity < amount_in {
        return None;
    }

    let gamma = |l: &Leg| f64::from(1_000_000 - l.fee_ppm) / 1_000_000.0;
    let usable = |l: &Leg| l.reserve_in > 0 && l.reserve_out > 0;

    // Allocation to one leg at a common marginal rate.
    let alloc_at = |l: &Leg, lambda: f64| -> f64 {
        if !usable(l) || lambda <= 0.0 {
            return 0.0;
        }
        let (g, rin, rout) = (gamma(l), l.reserve_in as f64, l.reserve_out as f64);
        let x = ((g * rin * rout / lambda).sqrt() - rin) / g;
        x.clamp(0.0, l.max_in.min(u128::from(u64::MAX)) as f64)
    };

    // At zero size a leg's marginal rate is γ·Rout/Rin. Above the best of those no leg
    // takes anything, which brackets the search from the top.
    let hi_start = legs
        .iter()
        .filter(|l| usable(l))
        .map(|l| gamma(l) * (l.reserve_out as f64) / (l.reserve_in as f64))
        .fold(0.0f64, f64::max);
    if hi_start <= 0.0 {
        return None;
    }

    let (mut lo, mut hi) = (0.0f64, hi_start);
    let target = amount_in as f64;
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if mid <= 0.0 || !mid.is_finite() {
            break;
        }
        let total: f64 = legs.iter().map(|l| alloc_at(l, mid)).sum();
        // Total is decreasing in lambda: too much means the rate is set too low.
        if total > target {
            lo = mid;
        } else {
            hi = mid;
        }
        if (hi - lo) <= f64::EPSILON * hi.max(1.0) {
            break;
        }
    }

    let lambda = 0.5 * (lo + hi);
    let mut out: Vec<u128> = legs
        .iter()
        .map(|l| {
            let x = alloc_at(l, lambda).floor();
            if x <= 0.0 { 0 } else { (x as u128).min(l.max_in) }
        })
        .collect();

    // Bisection lands near the target but rounding down loses a few base units, and an
    // allocation that does not sum to the input is not a route. Hand the remainder to
    // whichever leg still has both headroom and the best marginal rate.
    let mut placed: u128 = out.iter().fold(0u128, |a, b| a.saturating_add(*b));
    while placed < amount_in {
        let short = amount_in - placed;
        let best = legs
            .iter()
            .enumerate()
            .filter(|(i, l)| usable(l) && out[*i] < l.max_in)
            .max_by(|(i, a), (j, b)| {
                let m = |l: &Leg, x: u128| {
                    let (g, rin, rout) = (gamma(l), l.reserve_in as f64, l.reserve_out as f64);
                    let d = rin + g * x as f64;
                    if d <= 0.0 { 0.0 } else { g * rin * rout / (d * d) }
                };
                m(a, out[*i]).total_cmp(&m(b, out[*j]))
            })
            .map(|(i, _)| i)?;
        let room = legs[best].max_in - out[best];
        let give = short.min(room.max(1));
        out[best] = out[best].saturating_add(give);
        placed = placed.saturating_add(give);
    }

    Some(out)
}

/// Total output from splitting `amount_in` optimally across parallel `legs`.
///
/// Quotes each allocation exactly rather than trusting the float search, so the number
/// returned is one a swap would actually produce.
#[must_use]
pub fn split_output(legs: &[Leg], amount_in: u128) -> Option<u128> {
    let alloc = split_across(legs, amount_in)?;
    let mut total: u128 = 0;
    for (leg, amount) in legs.iter().zip(alloc) {
        if amount == 0 {
            continue;
        }
        total = total.checked_add(leg.quote(amount)?)?;
    }
    Some(total)
}

/// Profit this cycle would pay an account holding exactly `capital` base units.
///
/// # Why this is measured rather than scaled from the optimum
///
/// Profit is concave in size: it climbs, peaks at [`optimal_input`], then falls away as
/// the trade moves the price against itself. So what a given account can take is
/// neither proportional to its capital nor a fixed share of the whole pie. A $100 book
/// facing an opportunity that peaks at $9,000 does not take 1.1% of it — and treating
/// it as though it did is exactly how a run measured at one book size gets extrapolated
/// into a claim about another.
///
/// Depth caps this independently of capital, which is the point worth keeping in view:
/// a cycle whose tightest leg stops quoting at $19 pays a $1,000,000 account the same
/// as a $100 one. Borrowed capital cannot widen a tick.
#[must_use]
pub fn profit_at_capital(legs: &[Leg], capital: u128) -> u128 {
    optimal_input(legs, capital).and_then(|n| cycle_profit(legs, n)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::{optimal_input as closed_form, CycleReserves};

    const FEE: FeePpm = 2500; // 25 bp, Raydium v4

    fn leg(r_in: u128, r_out: u128) -> Leg {
        Leg::cp(r_in, r_out, FEE)
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
            let cr =
                CycleReserves { a_in, a_out, b_in, b_out, fee_a_ppm: FEE, fee_b_ppm: FEE };
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
        let legs =
            [leg(1_000_000, 1_050_000), leg(1_000_000, 1_050_000), leg(1_000_000, 1_050_000)];
        let unconstrained = optimal_input(&legs, u128::MAX).unwrap();
        let capped = optimal_input(&legs, unconstrained / 4).unwrap();
        assert!(capped <= unconstrained / 4, "must respect the cap");
        assert!(cycle_profit(&legs, capped).unwrap() > 0, "a capped trade must still profit");
    }

    #[test]
    fn a_cycle_that_only_loses_returns_none() {
        // Heavy fees against a tiny edge.
        let legs = [
            Leg::cp(1_000_000, 1_001_000, 30_000),
            Leg::cp(1_000_000, 1_001_000, 30_000),
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
        assert!((route_fee_bps(&legs) - 49.94).abs() < 0.1, "fee decomposition must match");
    }

    /// The reason parts-per-million exists in this codebase. Two 1 bp pools cost
    /// 2 bps, not the 0 or 200 that a coarser unit would produce.
    #[test]
    fn one_basis_point_fee_tiers_survive_the_unit() {
        let legs = [Leg::cp(1_000_000, 1_000_000, 100), Leg::cp(1_000_000, 1_000_000, 100)];
        assert!((route_fee_bps(&legs) - 2.0).abs() < 0.001);
        let edge = marginal_edge_bps(&legs).unwrap();
        assert!((edge + 2.0).abs() < 0.01, "expected about -2bps, got {edge}");
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

    // ---- bounded (concentrated-liquidity) legs ----

    #[test]
    fn a_bounded_leg_refuses_sizes_past_its_tick() {
        let l = Leg::bounded(1_000_000, 1_050_000, 400, 1000);
        assert!(l.quote(1000).is_some(), "at the bound must still quote");
        assert_eq!(l.quote(1001), None, "past the bound must refuse, not extrapolate");
    }

    #[test]
    fn sizing_stays_inside_the_tick_that_binds() {
        // The unconstrained optimum is far larger than the second leg can absorb.
        let unbounded =
            [leg(10_000_000, 12_000_000), Leg::cp(10_000_000, 12_000_000, FEE)];
        let want = optimal_input(&unbounded, u128::MAX).unwrap();
        let mid = chain_quote(&unbounded[..1], want).unwrap();

        // Now cap the second leg at a tenth of what the optimum would push through it.
        let bounded = [unbounded[0], Leg::bounded(10_000_000, 12_000_000, FEE, mid / 10)];
        let sized = optimal_input(&bounded, u128::MAX).expect("a smaller trade still profits");

        assert!(sized < want, "the tick bound must reduce the size ({sized} !< {want})");
        let mid_now = chain_quote(&bounded[..1], sized).unwrap();
        assert!(mid_now <= mid / 10, "sizing must respect the downstream bound");
        assert!(cycle_profit(&bounded, sized).unwrap() > 0);
    }

    #[test]
    fn a_cycle_with_no_feasible_size_returns_none() {
        // Profitable on paper, but the first tick holds nothing.
        let legs = [Leg::bounded(1_000_000, 1_500_000, 400, 0), leg(1_000_000, 1_000_000)];
        assert!(is_profitable(&legs), "marginal rates still clear");
        assert_eq!(optimal_input(&legs, u128::MAX), None, "but nothing is tradable");
    }

    #[test]
    fn largest_feasible_finds_the_exact_boundary() {
        let legs = [Leg::bounded(1_000_000, 1_050_000, FEE, 777)];
        assert_eq!(largest_feasible(&legs, 10_000), 777);
        assert_eq!(largest_feasible(&legs, 100), 100, "already feasible means no search");
        assert_eq!(largest_feasible(&legs, 0), 0);
    }

    /// A bounded leg deeper in the chain still binds, because the amount reaching it
    /// grows with the size we put in at the front.
    #[test]
    fn feasibility_is_monotone_so_the_binary_search_is_valid() {
        let legs = [leg(1_000_000, 1_050_000), Leg::bounded(1_000_000, 1_050_000, FEE, 5_000)];
        let boundary = largest_feasible(&legs, 1_000_000);
        assert!(chain_quote(&legs, boundary).is_some(), "the boundary itself must be feasible");
        assert!(chain_quote(&legs, boundary + 1).is_none(), "one past it must not be");
    }

    #[test]
    fn a_bottleneck_downstream_leg_bounds_the_reported_depth() {
        // Enter through an enormous pool, exit through a tick with almost nothing in
        // it. The first leg alone says a billion; the cycle can absorb about a
        // thousand. Reporting the former is how an untradeable route leads a board.
        let legs = [
            leg(1_000_000_000, 1_000_000_000),
            Leg::bounded(1_000_000_000, 1_000_000_000, FEE, 1_000),
        ];
        let depth = cycle_depth_base(&legs);
        assert_eq!(search_ceiling(&legs), 1_000_000_000, "the first leg is not the constraint");
        assert!(
            (990..=1_010).contains(&depth),
            "the downstream tick bounds the cycle, got {depth}"
        );
    }

    #[test]
    fn depth_of_a_single_unbounded_leg_is_its_own_reserve() {
        assert_eq!(cycle_depth_base(&[leg(1_000_000, 2_000_000)]), 1_000_000);
    }

    #[test]
    fn depth_never_promises_more_than_the_sizing_search_will_take() {
        // Depth is a first-order upper bound; the exact search must land at or under
        // it. If it ever came out low, the instrument would understate capacity and
        // discard real opportunities.
        let legs = [
            leg(1_000_000, 1_050_000),
            Leg::bounded(1_000_000, 1_050_000, FEE, 500),
        ];
        let depth = cycle_depth_base(&legs);
        let sized = optimal_input(&legs, u128::MAX).expect("this cycle profits");
        assert!(sized <= depth, "sizing took {sized} against a reported depth of {depth}");
    }

    #[test]
    fn a_cycle_with_no_usable_leg_has_no_depth() {
        assert_eq!(cycle_depth_base(&[]), 0);
        assert_eq!(cycle_depth_base(&[leg(0, 100)]), 0);
        assert_eq!(cycle_depth_base(&[leg(100, 0)]), 0);
        assert_eq!(cycle_depth_base(&[Leg::bounded(1_000, 1_000, FEE, 0)]), 0);
    }

    /// The ladder's whole purpose: more capital pays more, but only up to the optimum,
    /// and never linearly. If this ever came out proportional, the ladder would be a
    /// rescaling of one number rather than a measurement of several.
    #[test]
    fn profit_rises_with_capital_and_then_stops() {
        let legs = [leg(1_000_000_000, 1_050_000_000), leg(1_000_000_000, 1_000_000_000)];
        let optimum = optimal_input(&legs, u128::MAX).expect("this cycle profits");
        let ceiling = profit_at_capital(&legs, u128::MAX);

        let small = profit_at_capital(&legs, optimum / 100);
        let half = profit_at_capital(&legs, optimum / 2);
        assert!(small < half && half < ceiling, "{small} < {half} < {ceiling}");

        // Concave, not linear: a hundredth of the capital takes far more than a
        // hundredth of the pie. Extrapolating one book size to another gets this wrong
        // in the direction that flatters the smaller account.
        assert!(
            small * 100 > ceiling,
            "a 1% account took {small}, which linear scaling would put at {}",
            ceiling / 100
        );
    }

    #[test]
    fn capital_past_the_optimum_buys_nothing_more() {
        let legs = [leg(1_000_000_000, 1_050_000_000), leg(1_000_000_000, 1_000_000_000)];
        let optimum = optimal_input(&legs, u128::MAX).expect("this cycle profits");
        let at_optimum = profit_at_capital(&legs, optimum);

        // Compared with a tolerance rather than exactly. `optimal_input` brackets by
        // ternary search and settles within a base unit or two of the true peak, and
        // which side of it a given cap lands on is an artefact of the bracketing, not a
        // difference in what the capital bought. Asserting equality here would pin the
        // search's rounding instead of the property that matters.
        for cap in [optimum * 1_000, u128::MAX] {
            let more = profit_at_capital(&legs, cap);
            assert!(
                more.abs_diff(at_optimum) <= 2,
                "capital past the optimum changed profit from {at_optimum} to {more}"
            );
        }
    }

    /// The measurement that answers whether borrowed capital helps. A cycle whose
    /// tightest leg stops quoting early pays a huge account exactly what it pays a
    /// small one — depth is not something a flash loan can widen.
    #[test]
    fn a_shallow_cycle_pays_a_large_account_no_more_than_a_small_one() {
        let legs = [
            leg(1_000_000_000, 1_050_000_000),
            Leg::bounded(1_000_000_000, 1_000_000_000, FEE, 500),
        ];
        let small = profit_at_capital(&legs, 1_000);
        assert!(small > 0, "the cycle must profit at all for this to mean anything");
        assert_eq!(small, profit_at_capital(&legs, 1_000_000_000));
    }

    /// The property the whole idea rests on: two comparable pools beat either alone.
    #[test]
    fn splitting_beats_sending_everything_to_the_best_pool() {
        // Same pair, near-identical prices — exactly the case our universe is full of.
        let a = leg(1_000_000_000, 1_000_000_000);
        let b = leg(900_000_000, 900_000_000);
        let size = 50_000_000u128;

        let split = split_output(&[a, b], size).expect("the group can absorb it");
        let best_alone = a.quote(size).unwrap().max(b.quote(size).unwrap());
        assert!(
            split > best_alone,
            "splitting produced {split}, the best single pool {best_alone}"
        );
    }

    #[test]
    fn one_pool_alone_is_unchanged_by_the_split_path() {
        let a = leg(1_000_000_000, 1_050_000_000);
        assert_eq!(split_output(&[a], 1_000_000), a.quote(1_000_000));
    }

    /// Degenerate case that must not silently misroute: when one pool is far better,
    /// splitting should converge on it rather than sprinkling size into a bad price.
    #[test]
    fn a_far_worse_pool_receives_almost_nothing() {
        let good = leg(1_000_000_000, 1_000_000_000);
        let awful = leg(1_000_000_000, 10_000_000); // 100x worse rate
        let alloc = split_across(&[good, awful], 1_000_000).unwrap();
        assert_eq!(alloc.iter().sum::<u128>(), 1_000_000, "allocations must sum to the input");
        assert!(alloc[0] > alloc[1] * 50, "got {alloc:?}");
    }

    #[test]
    fn allocations_always_sum_to_the_input_exactly() {
        // Rounding down per leg loses base units; the remainder has to go somewhere or
        // the "route" quietly trades less than it was given.
        let legs = [leg(1_000_000_000, 1_000_000_000), leg(700_000_000, 690_000_000), leg(3_000_000, 3_100_000)];
        for size in [1u128, 7, 999, 1_000_000, 123_456_789] {
            let alloc = split_across(&legs, size).unwrap();
            assert_eq!(alloc.iter().sum::<u128>(), size, "size {size} gave {alloc:?}");
        }
    }

    #[test]
    fn a_split_never_pushes_a_leg_past_its_tick() {
        let bounded = Leg::bounded(1_000_000_000, 1_000_000_000, FEE, 1_000);
        let open = leg(1_000_000_000, 1_000_000_000);
        let alloc = split_across(&[bounded, open], 500_000).unwrap();
        assert!(alloc[0] <= 1_000, "bounded leg took {}", alloc[0]);
        assert_eq!(alloc.iter().sum::<u128>(), 500_000);
        assert!(split_output(&[bounded, open], 500_000).is_some(), "and it still quotes");
    }

    /// Splitting is what raises a hop's usable depth: a group refuses only what exceeds
    /// the *sum* of its bounds, where any single pool would have refused far sooner.
    #[test]
    fn a_group_absorbs_what_no_single_pool_in_it_could() {
        let a = Leg::bounded(1_000_000_000, 1_000_000_000, FEE, 1_000);
        let b = Leg::bounded(1_000_000_000, 1_000_000_000, FEE, 1_000);
        assert!(a.quote(1_500).is_none(), "neither pool alone can take this");
        assert!(split_output(&[a, b], 1_500).is_some(), "together they can");
        assert!(split_across(&[a, b], 2_001).is_none(), "but not past their combined bound");
    }

    #[test]
    fn splitting_degenerate_input_does_not_panic() {
        assert!(split_across(&[], 100).is_none());
        assert_eq!(split_across(&[leg(1_000, 1_000)], 0).unwrap(), vec![0]);
        assert!(split_output(&[leg(0, 100), leg(100, 0)], 50).is_none());
    }

    #[test]
    fn an_unprofitable_cycle_pays_nothing_at_any_capital() {
        let legs = [leg(1_000_000, 1_000_000), leg(1_000_000, 1_000_000)];
        assert_eq!(profit_at_capital(&legs, u128::MAX), 0);
        assert_eq!(profit_at_capital(&legs, 0), 0);
    }
}
