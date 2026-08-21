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

/// Fee expressed in **parts per million** (1 ppm = 0.0001%; 100 ppm = 1 bp).
///
/// Parts per million, not basis points, because that is the native unit on chain:
/// Orca Whirlpool's `fee_rate` and Raydium CLMM's `trade_fee_rate` are both stored
/// this way. Rounding them into basis points would turn a 1 bp pool and a 1.5 bp pool
/// into the same number, and at these fee tiers the difference between 1 and 2 bp is
/// most of the edge we are hunting.
pub type FeePpm = u32;

const PPM_DENOM: u128 = 1_000_000;

use ruint::aliases::U256;

/// Widen to 256 bits. Every product of more than two pool-scale quantities must go
/// through this: `u128` overflows at pools above roughly 43 USDC, which is every
/// pool that actually exists.
#[inline]
fn u256(x: u128) -> U256 {
    U256::from(x)
}

/// Output of a constant-product swap, given reserves and a fee.
///
/// `reserve_in` and `reserve_out` are the pool's current balances of the input and
/// output mints respectively, in base units.
///
/// Returns `None` on empty reserves or arithmetic overflow rather than panicking —
/// this runs in the hot path against untrusted on-chain data, some of which is
/// adversarial.
#[must_use]
pub fn cp_swap_out(amount_in: u128, reserve_in: u128, reserve_out: u128, fee_ppm: FeePpm) -> Option<u128> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
        return None;
    }
    let gamma_num = PPM_DENOM.checked_sub(u128::from(fee_ppm))?;

    // amount_in_after_fee = amount_in · γ
    // 256-bit throughout. `in_after_fee` carries a factor of 1e6, and multiplying
    // that by an output reserve overflows u128 at concentrated-liquidity scale: a
    // pool's virtual reserve can exceed 2^88, and 2^88 · 2^46 is already past u128.
    // This is the same overflow that once made `optimal_input` return None for the
    // entire live market, one function further down the call chain.
    let in_after_fee = u256(amount_in).checked_mul(u256(gamma_num))?;

    // out = (in·γ · reserve_out) / (reserve_in·PPM + in·γ)
    let numerator = in_after_fee.checked_mul(u256(reserve_out))?;
    let denominator = u256(reserve_in).checked_mul(u256(PPM_DENOM))?.checked_add(in_after_fee)?;
    if denominator == U256::ZERO {
        return None;
    }

    let out: u128 = (numerator / denominator).try_into().ok()?;

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
    pub fee_a_ppm: FeePpm,
    pub fee_b_ppm: FeePpm,
}

/// Cheap necessary condition for the cycle to be profitable at *any* size.
///
/// Profitable iff `γa·γb·B·D > A·C`. Scaled by `BPS_DENOM²` to stay in integers.
/// Used to reject the overwhelming majority of candidate cycles before spending a
/// square root on them.
///
/// Computed in 256 bits: `γa·γb·B·D` is a product of four pool-scale quantities and
/// overflows `u128` for pools of realistic size.
#[must_use]
pub fn is_profitable(r: &CycleReserves) -> bool {
    let (Some(ga), Some(gb)) = (
        PPM_DENOM.checked_sub(u128::from(r.fee_a_ppm)),
        PPM_DENOM.checked_sub(u128::from(r.fee_b_ppm)),
    ) else {
        return false;
    };

    // lhs = γa·γb·B·D, rhs = A·C·BPS²
    let lhs = u256(ga) * u256(gb) * u256(r.a_out) * u256(r.b_out);
    let rhs = u256(r.a_in) * u256(r.b_in) * u256(PPM_DENOM) * u256(PPM_DENOM);
    lhs > rhs
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
    let ga = PPM_DENOM.checked_sub(u128::from(r.fee_a_ppm))?;
    let gb = PPM_DENOM.checked_sub(u128::from(r.fee_b_ppm))?;

    // All intermediates are 256-bit. The radicand is a product of SIX pool-scale
    // quantities, which overflows u128 once reserves exceed roughly 4.3e7 base units
    // — i.e. about 43 USDC. Every real pool is far larger than that, so computing
    // this in u128 silently returns None for the entire live market.
    let radicand =
        u256(ga) * u256(gb) * u256(r.a_in) * u256(r.a_out) * u256(r.b_in) * u256(r.b_out);

    // Integer square root, truncating downward.
    let root = radicand.root(2);

    let ac_scaled = u256(r.a_in) * u256(r.b_in) * u256(PPM_DENOM);

    // is_profitable implies root > ac_scaled up to sqrt truncation; guard anyway.
    if root <= ac_scaled {
        return None;
    }
    let numerator = root - ac_scaled;

    // denominator = γa·(C + γb·B), with the BPS factors kept explicit:
    //   γa·(C + γb·B) = ga·(C·BPS + gb·B) / BPS²
    // and the surrounding expression multiplies by BPS, so we carry
    // ga·(C·BPS + gb·B) here and apply the single remaining BPS below.
    let denominator = u256(ga) * (u256(r.b_in) * u256(PPM_DENOM) + u256(gb) * u256(r.a_out));

    if denominator == U256::ZERO {
        return None;
    }
    let x = numerator * u256(PPM_DENOM) / denominator;

    // A size that does not fit u128 is not a size we can put in a transaction.
    let x: u128 = x.try_into().ok()?;
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
    let mid = cp_swap_out(amount_in, r.a_in, r.a_out, r.fee_a_ppm)?;
    let back = cp_swap_out(mid, r.b_in, r.b_out, r.fee_b_ppm)?;
    back.checked_sub(amount_in)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cycle(a_in: u128, a_out: u128, b_in: u128, b_out: u128) -> CycleReserves {
        CycleReserves { a_in, a_out, b_in, b_out, fee_a_ppm: 2500, fee_b_ppm: 2500 }
    }

    #[test]
    fn swap_matches_hand_computed_value() {
        // 1000 in, reserves 1_000_000 / 1_000_000, 25 bp fee = 2500 ppm.
        // in_after_fee = 1000 * 997_500 = 997_500_000
        // num = 997_500_000 * 1_000_000 = 9.975e14
        // den = 1_000_000*1_000_000 + 997_500_000 = 1_000_997_500_000
        // out = 9.975e14 / 1.0009975e12 = 996
        assert_eq!(cp_swap_out(1000, 1_000_000, 1_000_000, 2500), Some(996));
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
        assert!(cycle_profit(&r, x * 5).is_none_or(|p| p < best));
    }

    #[test]
    fn rejects_degenerate_inputs() {
        assert_eq!(cp_swap_out(0, 100, 100, 2500), None);
        assert_eq!(cp_swap_out(100, 0, 100, 2500), None);
        assert_eq!(cp_swap_out(100, 100, 0, 2500), None);
        // Fee >= 100% is nonsense and must not underflow.
        assert_eq!(cp_swap_out(100, 1000, 1000, 1_000_001), None);
    }

    /// Regression: `optimal_input` used to compute the six-way radicand in `u128`,
    /// which overflows once reserves exceed roughly 4.3e7 base units — about 43 USDC.
    /// Every pool on mainnet is orders of magnitude bigger, so the function returned
    /// `None` for the entire live market while every small-number unit test passed.
    /// These reserves are SOL/USDC at realistic depth.
    #[test]
    fn finds_opportunities_at_mainnet_pool_scale() {
        // Venue 1: 1,800 SOL / 138,186 USDC  → SOL at 76.77
        // Venue 2: 4,200 SOL / 335,412 USDC  → SOL at 79.86  (a ~4% dislocation)
        let r = CycleReserves {
            a_in: 138_186_000_000,     // USDC in (6 dp)
            a_out: 1_800_000_000_000,  // SOL out (9 dp)
            b_in: 4_200_000_000_000,   // SOL in
            b_out: 335_412_000_000,    // USDC out
            fee_a_ppm: 2500,
            fee_b_ppm: 2500,
        };
        assert!(is_profitable(&r), "a 4% dislocation at real depth must be profitable");

        let x = optimal_input(&r).expect("must size a mainnet-scale opportunity");
        let profit = cycle_profit(&r, x).expect("optimal size must profit");
        assert!(profit > 0);

        // Nothing nearby may beat it.
        for pct in [50u128, 80, 95, 105, 120, 200] {
            if let Some(p) = cycle_profit(&r, x * pct / 100) {
                assert!(p <= profit + 2, "size at {pct}% beat the optimum");
            }
        }
    }

    /// The capital-constrained case, which is the whole $5 question: taking less than
    /// the optimal size must still profit, just proportionally less.
    #[test]
    fn undersized_trades_still_profit_proportionally() {
        let r = CycleReserves {
            a_in: 138_186_000_000,
            a_out: 1_800_000_000_000,
            b_in: 4_200_000_000_000,
            b_out: 335_412_000_000,
            fee_a_ppm: 2500,
            fee_b_ppm: 2500,
        };
        let optimal = optimal_input(&r).unwrap();
        let five_dollars = 4_800_000u128; // $4.80 in USDC base units

        assert!(five_dollars < optimal, "at real depth $5 must be below the optimum");
        let small = cycle_profit(&r, five_dollars).expect("a $4.80 trade must still profit");
        let big = cycle_profit(&r, optimal).unwrap();
        assert!(small < big, "capital cap must cost us profit");
        assert!(small > 0);
    }

    #[test]
    fn huge_reserves_do_not_panic() {
        // Overflow must degrade to None, never wrap or panic.
        let r = CycleReserves {
            a_in: u128::MAX / 2,
            a_out: u128::MAX / 2,
            b_in: u128::MAX / 2,
            b_out: u128::MAX / 2,
            fee_a_ppm: 2500,
            fee_b_ppm: 2500,
        };
        let _ = is_profitable(&r);
        let _ = optimal_input(&r);
    }

    #[test]
    fn zero_fee_pools_still_need_a_dislocation() {
        let mut r = cycle(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        r.fee_a_ppm = 0;
        r.fee_b_ppm = 0;
        // With no fees and no dislocation, profit is exactly zero — not positive.
        assert!(!is_profitable(&r));
    }
}
