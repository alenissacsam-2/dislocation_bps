//! Constant-product AMM math, and the closed-form optimal size for a two-pool
//! arbitrage cycle.
//!
//! Everything here is integer arithmetic on `u128`. No floats touch the profit path:
//! a float rounding error in the wrong direction turns a "profitable" trade into a
//! losing one, and the loss is real money.
//!
//! # Derivation of the optimal input size
//!
//! Constant-product swap with a fee, where `γ = 1 − fee`:
//!
//! ```text
//! out = (γ · in · R_out) / (R_in + γ · in)
//! ```
//!
//! Chain two pools. Pool A holds `(A, B)` — we put token X in against reserve `A` and
//! take token Y out of reserve `B`. Pool B holds `(C, D)` — we put that Y in against
//! reserve `C` and take X back out of `D`.
//!
//! ```text
//! y = γa·x·B / (A + γa·x)
//! z = γb·y·D / (C + γb·y)
//! ```
//!
//! Substituting `y` into `z` and simplifying:
//!
//! ```text
//!       γa·γb·B·D·x
//! z = ─────────────────────────
//!     A·C + γa·x·(C + γb·B)
//! ```
//!
//! which is itself a constant-product curve `z = E_out·x / (E_in + x)` with
//!
//! ```text
//! E_in  = A·C / (γa·(C + γb·B))
//! E_out = γb·B·D / (C + γb·B)
//! ```
//!
//! Profit is `P(x) = z − x`. Setting `dP/dx = 0`:
//!
//! ```text
//! E_out·E_in / (E_in + x)² = 1   ⟹   x* = √(E_in·E_out) − E_in
//! ```
//!
//! Expanding gives the form we actually compute:
//!
//! ```text
//!      √(γa·γb·A·B·C·D) − A·C
//! x* = ──────────────────────────
//!         γa·(C + γb·B)
//! ```
//!
//! The cycle is profitable at all iff the numerator is positive, i.e.
//! `γa·γb·B·D > A·C`. That test is cheap and is used to reject candidates before
//! doing any square root.

/// Fee expressed in basis points (1 bp = 0.01%). Raydium v4 is 25 bp, most
/// constant-product pools are 25–30 bp.
pub type FeeBps = u32;

const BPS_DENOM: u128 = 10_000;

/// Output of a constant-product swap, given reserves and a fee.
///
/// `reserve_in` and `reserve_out` are the pool's current balances of the input and
/// output mints respectively, in base units.
///
/// Returns `None` on empty reserves or arithmetic overflow rather than panicking —
/// this runs in the hot path against untrusted on-chain data, some of which is
/// adversarial.
#[must_use]
pub fn cp_swap_out(amount_in: u128, reserve_in: u128, reserve_out: u128, fee_bps: FeeBps) -> Option<u128> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
        return None;
    }
    let gamma_num = BPS_DENOM.checked_sub(u128::from(fee_bps))?;

    // amount_in_after_fee = amount_in · γ
    let in_after_fee = amount_in.checked_mul(gamma_num)?;

    // out = (in·γ · reserve_out) / (reserve_in·BPS + in·γ)
    let numerator = in_after_fee.checked_mul(reserve_out)?;
    let denominator = reserve_in.checked_mul(BPS_DENOM)?.checked_add(in_after_fee)?;

    let out = numerator / denominator;

    // Never allow draining the pool; a quote that claims the whole reserve is a
    // decode error, not an opportunity.
    if out >= reserve_out {
        return None;
    }
    Some(out)
}

/// A two-pool arbitrage cycle: swap X→Y in pool A, then Y→X in pool B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleReserves {
    /// Pool A reserve of the token we spend (X).
    pub a_in: u128,
    /// Pool A reserve of the intermediate token (Y).
    pub a_out: u128,
    /// Pool B reserve of the intermediate token (Y).
    pub b_in: u128,
    /// Pool B reserve of the token we get back (X).
    pub b_out: u128,
    pub fee_a_bps: FeeBps,
    pub fee_b_bps: FeeBps,
}

/// Cheap necessary condition for the cycle to be profitable at *any* size.
///
/// Profitable iff `γa·γb·B·D > A·C`. Scaled by `BPS_DENOM²` to stay in integers.
/// Used to reject the overwhelming majority of candidate cycles before spending a
/// square root on them.
#[must_use]
pub fn is_profitable(r: &CycleReserves) -> bool {
    let (Some(ga), Some(gb)) = (
        BPS_DENOM.checked_sub(u128::from(r.fee_a_bps)),
        BPS_DENOM.checked_sub(u128::from(r.fee_b_bps)),
    ) else {
        return false;
    };

    // lhs = γa·γb·B·D, rhs = A·C·BPS²
    let lhs = ga
        .checked_mul(gb)
        .and_then(|g| g.checked_mul(r.a_out))
        .and_then(|v| v.checked_mul(r.b_out));
    let rhs = r
        .a_in
        .checked_mul(r.b_in)
        .and_then(|v| v.checked_mul(BPS_DENOM))
        .and_then(|v| v.checked_mul(BPS_DENOM));

    match (lhs, rhs) {
        (Some(l), Some(rr)) => l > rr,
        // Overflow means reserves are absurd; treat as not profitable.
        _ => false,
    }
}

/// Closed-form optimal input size for a two-pool constant-product cycle.
///
/// Returns `None` when the cycle is unprofitable at every size, or on overflow.
///
/// ```text
///      √(γa·γb·A·B·C·D) − A·C
/// x* = ──────────────────────────
///         γa·(C + γb·B)
/// ```
#[must_use]
pub fn optimal_input(r: &CycleReserves) -> Option<u128> {
    if !is_profitable(r) {
        return None;
    }
    let ga = BPS_DENOM.checked_sub(u128::from(r.fee_a_bps))?;
    let gb = BPS_DENOM.checked_sub(u128::from(r.fee_b_bps))?;

    // Work in units scaled by BPS_DENOM to keep γ integral.
    //
    // numerator = √(γa·γb·A·B·C·D) − A·C·BPS
    // denominator = γa·(C·BPS + γb·B) / BPS  →  keep the BPS factors explicit.

    // radicand = γa·γb·A·B·C·D. This is the overflow-prone term: six multiplied
    // reserves. u128 holds ~3.4e38; realistic reserves are <1e20 each, so we stage
    // the multiplication and bail on overflow rather than wrapping.
    let radicand = ga
        .checked_mul(gb)?
        .checked_mul(r.a_in)?
        .checked_mul(r.a_out)?
        .checked_mul(r.b_in)?
        .checked_mul(r.b_out)?;

    let root = radicand.isqrt();
    let ac_scaled = r.a_in.checked_mul(r.b_in)?.checked_mul(BPS_DENOM)?;

    // is_profitable guaranteed root > ac_scaled up to isqrt truncation; guard anyway.
    let numerator = root.checked_sub(ac_scaled)?;

    // denominator = γa·(C + γb·B), with the BPS factors kept explicit:
    //   γa·(C + γb·B) = ga·(C·BPS + gb·B) / BPS²
    // and the surrounding expression multiplies by BPS, so we carry
    // ga·(C·BPS + gb·B) here and apply the single remaining BPS below.
    let denominator = ga.checked_mul(r.b_in.checked_mul(BPS_DENOM)?.checked_add(gb.checked_mul(r.a_out)?)?)?;

    if denominator == 0 {
        return None;
    }
    let x = numerator.checked_mul(BPS_DENOM)? / denominator;
    if x == 0 {
        None
    } else {
        Some(x)
    }
}

/// Gross profit of running `amount_in` through the cycle, in input-token base units.
///
/// Returns `None` if either leg fails or the cycle loses money.
#[must_use]
pub fn cycle_profit(r: &CycleReserves, amount_in: u128) -> Option<u128> {
    let mid = cp_swap_out(amount_in, r.a_in, r.a_out, r.fee_a_bps)?;
    let back = cp_swap_out(mid, r.b_in, r.b_out, r.fee_b_bps)?;
    back.checked_sub(amount_in)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cycle(a_in: u128, a_out: u128, b_in: u128, b_out: u128) -> CycleReserves {
        CycleReserves { a_in, a_out, b_in, b_out, fee_a_bps: 25, fee_b_bps: 25 }
    }

    #[test]
    fn swap_matches_hand_computed_value() {
        // 1000 in, reserves 1_000_000 / 1_000_000, 25bp fee.
        // in_after_fee = 1000 * 9975 = 9_975_000
        // num = 9_975_000 * 1_000_000 = 9.975e12
        // den = 1_000_000*10_000 + 9_975_000 = 10_009_975_000
        // out = 9.975e12 / 1.0009975e10 = 996
        assert_eq!(cp_swap_out(1000, 1_000_000, 1_000_000, 25), Some(996));
    }

    #[test]
    fn balanced_pools_are_not_profitable() {
        // Identical pools: any round trip loses two lots of fees.
        assert!(!is_profitable(&cycle(1_000_000, 1_000_000, 1_000_000, 1_000_000)));
        assert_eq!(optimal_input(&cycle(1_000_000, 1_000_000, 1_000_000, 1_000_000)), None);
    }

    #[test]
    fn skewed_pools_are_profitable_and_sized() {
        // Pool B offers Y at a much better rate than pool A charges for it.
        let r = cycle(1_000_000, 1_000_000, 1_000_000, 1_300_000);
        assert!(is_profitable(&r), "20%+ dislocation must be profitable");
        let x = optimal_input(&r).expect("should produce a size");
        assert!(x > 0);
        assert!(cycle_profit(&r, x).is_some(), "optimal size must actually profit");
    }

    /// The real test of the closed form: it must beat every nearby size.
    #[test]
    fn optimal_input_maximises_profit() {
        let r = cycle(5_000_000, 5_000_000, 5_000_000, 6_500_000);
        let x = optimal_input(&r).unwrap();
        let best = cycle_profit(&r, x).unwrap();

        // Sweep a wide neighbourhood; nothing may beat the closed form by more than
        // the rounding slack inherent in integer division.
        for delta_pct in [50u128, 75, 90, 95, 99, 101, 105, 110, 125, 150, 200] {
            let probe = x * delta_pct / 100;
            if probe == 0 {
                continue;
            }
            if let Some(p) = cycle_profit(&r, probe) {
                assert!(
                    p <= best + 2,
                    "size {probe} ({delta_pct}% of optimum) profited {p}, beating optimum {best}"
                );
            }
        }
    }

    #[test]
    fn profit_is_concave_around_the_optimum() {
        // Too small leaves money on the table; too large eats itself in slippage.
        let r = cycle(10_000_000, 10_000_000, 10_000_000, 12_000_000);
        let x = optimal_input(&r).unwrap();
        let best = cycle_profit(&r, x).unwrap();
        assert!(cycle_profit(&r, x / 10).unwrap() < best);
        assert!(cycle_profit(&r, x * 5).map_or(true, |p| p < best));
    }

    #[test]
    fn rejects_degenerate_inputs() {
        assert_eq!(cp_swap_out(0, 100, 100, 25), None);
        assert_eq!(cp_swap_out(100, 0, 100, 25), None);
        assert_eq!(cp_swap_out(100, 100, 0, 25), None);
        // Fee >= 100% is nonsense and must not underflow.
        assert_eq!(cp_swap_out(100, 1000, 1000, 10_001), None);
    }

    #[test]
    fn huge_reserves_do_not_panic() {
        // Overflow must degrade to None, never wrap or panic.
        let r = CycleReserves {
            a_in: u128::MAX / 2,
            a_out: u128::MAX / 2,
            b_in: u128::MAX / 2,
            b_out: u128::MAX / 2,
            fee_a_bps: 25,
            fee_b_bps: 25,
        };
        let _ = is_profitable(&r);
        let _ = optimal_input(&r);
    }

    #[test]
    fn zero_fee_pools_still_need_a_dislocation() {
        let mut r = cycle(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        r.fee_a_bps = 0;
        r.fee_b_bps = 0;
        // With no fees and no dislocation, profit is exactly zero — not positive.
        assert!(!is_profitable(&r));
    }
}
