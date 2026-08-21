//! Concentrated-liquidity (Uniswap-v3-style) pools: Orca Whirlpool and Raydium CLMM.
//!
//! # The whole trick: within one tick, a CLMM *is* a constant-product pool
//!
//! A concentrated-liquidity pool at price `P` with liquidity `L` behaves exactly like
//! a constant-product pool holding **virtual reserves**
//!
//! ```text
//! x = L / √P     (of token A)          y = L · √P     (of token B)
//! ```
//!
//! Proof. The CLMM quote for input `Δx` (after fee) is `out = L·(√P − √P')` where
//! `1/√P' = 1/√P + Δx/L`, so `√P' = L·√P / (L + Δx·√P)` and
//!
//! ```text
//! out = L·√P − L²·√P/(L + Δx·√P) = Δx·L·P / (L + Δx·√P)
//! ```
//!
//! The constant-product quote with those virtual reserves is
//!
//! ```text
//! out = Δx·y / (x + Δx) = Δx·L·√P / (L/√P + Δx) = Δx·L·P / (L + Δx·√P)
//! ```
//!
//! Identical. And `x·y = L²` is constant, as constant-product requires.
//!
//! This is worth more than it looks. It means every piece of money math already in
//! this crate — [`crate::amm::cp_swap_out`], the closed-form two-pool optimum, the
//! N-hop ternary search, the marginal-edge statistic — applies to concentrated
//! liquidity **unchanged**. There is no second quote engine to write, no second set
//! of overflow bugs to find, and no risk of the two engines silently disagreeing.
//!
//! # Where the equivalence stops
//!
//! `L` is only constant while the swap stays inside the current tick interval. Cross
//! a tick where a position starts or ends and `L` jumps, and the constant-product
//! curve is the wrong curve on the other side.
//!
//! Both venues require position boundaries to be multiples of the pool's
//! `tick_spacing`, so `L` is provably constant on the open interval
//! `(n·spacing, (n+1)·spacing)` containing the current tick. That interval is what
//! [`bounds`] computes, and [`capacity_for_input`] reports how much size it can
//! absorb. Past that, we refuse to quote rather than guess — an over-quote is a
//! trade that loses money, and refusing costs nothing when the number is this large:
//! a $5 trade against a $24M pool moves the price by about one part in 10⁷, while
//! one tick at spacing 4 is 400 parts in 10⁷.

use ruint::aliases::U256;

/// Q64.64 scale. `sqrt_price_x64 = √P · 2⁶⁴`, the on-chain representation used by
/// both Orca Whirlpool and Raydium CLMM.
pub const Q64: u128 = 1 << 64;

/// Tick bounds shared by both venues: `1.0001^±443636` spans the representable range.
pub const MIN_TICK: i32 = -443_636;
pub const MAX_TICK: i32 = 443_636;

/// How far we pull tick boundaries inward before using them, in parts per billion.
///
/// [`sqrt_price_at_tick`] is computed in `f64`, whose relative error is about 1e-16.
/// Shrinking the usable interval by 1e-9 puts seven orders of magnitude between the
/// error and the margin, so a boundary we compute is never past the real one. The
/// cost is that we decline to use the last billionth of each tick's depth, which is
/// not a quantity anyone will miss.
const SAFETY_SHRINK_PPB: u128 = 1_000_000_000;

#[inline]
fn u256(x: u128) -> U256 {
    U256::from(x)
}

#[inline]
fn narrow(x: U256) -> Option<u128> {
    x.try_into().ok()
}

/// `√(1.0001^tick) · 2⁶⁴`, the price at a tick index.
///
/// Computed in `f64`. That is a deliberate choice, not a shortcut: this value is
/// never used to compute an amount, only to bound one, and [`bounds`] applies a
/// margin far larger than the error. The exactness that matters — the quote itself —
/// stays in integer arithmetic in [`crate::amm`].
///
/// The invariant this must satisfy, verified against captured mainnet accounts in
/// the tests, is `sqrt_price_at_tick(t) ≤ sqrt_price < sqrt_price_at_tick(t+1)` for
/// the pool's own reported `tick_current`.
#[must_use]
pub fn sqrt_price_at_tick(tick: i32) -> Option<u128> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return None;
    }
    let v = 1.000_1_f64.powf(f64::from(tick) / 2.0) * 18_446_744_073_709_551_616.0;
    if !v.is_finite() || v < 1.0 {
        return None;
    }
    Some(v as u128)
}

/// The tick-spacing-aligned interval containing `tick_current`.
///
/// Uses floor division, so negative ticks round *down* — `floor(-23953 / 4) = -5989`,
/// giving `[-23956, -23952)`, not `[-23952, -23948)`. Getting this wrong on the
/// negative side (which is where every SOL/USDC pool sits, since SOL is token A and
/// its raw price in USDC units is below 1) would put the boundary on the wrong side
/// of the current price and hand out a capacity that is already spent.
#[must_use]
pub fn tick_interval(tick_current: i32, tick_spacing: u16) -> Option<(i32, i32)> {
    let spacing = i32::from(tick_spacing);
    if spacing <= 0 {
        return None;
    }
    let lower = tick_current.div_euclid(spacing).checked_mul(spacing)?;
    let upper = lower.checked_add(spacing)?;
    Some((lower, upper))
}

/// The current tick interval's true `(sqrt_lo, sqrt_hi)`, in Q64.64, unshrunk.
///
/// Use this to check that a reported price belongs to its reported tick. Use
/// [`bounds`] to decide how much size the tick can absorb.
#[must_use]
pub fn raw_bounds(tick_current: i32, tick_spacing: u16) -> Option<(u128, u128)> {
    let (lower, upper) = tick_interval(tick_current, tick_spacing)?;
    Some((sqrt_price_at_tick(lower)?, sqrt_price_at_tick(upper)?))
}

/// Does `sqrt_price_x64` belong to the tick the pool says it is in?
///
/// A pool that fails this is either mid-write or being decoded with the wrong layout;
/// either way its numbers are not to be traded on.
///
/// This is deliberately *not* the same test as "is the price inside [`bounds`]". The
/// safety shrink there points inward, and a price can legitimately sit exactly on a
/// tick boundary: a swap that runs precisely into one stops there, and a freshly
/// created pool sits on tick 0 by construction. Conflating the two rejects such pools
/// outright when the honest answer is "no depth in that direction, full depth in the
/// other" — which [`capacity_for_input`] already reports correctly.
#[must_use]
pub fn price_belongs_to_tick(sqrt_price_x64: u128, tick_current: i32, tick_spacing: u16) -> bool {
    let Some((lo, hi)) = raw_bounds(tick_current, tick_spacing) else { return false };
    // The same 1e-9 tolerance, applied outward: this asks about membership, and both
    // sides of the comparison come from an f64 approximation.
    let lo = lo.saturating_sub(lo / SAFETY_SHRINK_PPB).saturating_sub(1);
    let hi = hi.saturating_add(hi / SAFETY_SHRINK_PPB).saturating_add(1);
    sqrt_price_x64 >= lo && sqrt_price_x64 <= hi
}

/// Conservative `(sqrt_lo, sqrt_hi)` bounds on the current tick interval, in Q64.64.
///
/// Both are pulled inward by [`SAFETY_SHRINK_PPB`]. Returns `None` if the interval is
/// degenerate or the shrink swallows it.
///
/// These can sit on the wrong side of the current price when the pool is parked on a
/// tick boundary. That is intended — it makes capacity zero in the pinned direction.
/// Validate membership with [`price_belongs_to_tick`], never with these.
#[must_use]
pub fn bounds(tick_current: i32, tick_spacing: u16) -> Option<(u128, u128)> {
    let (lo, hi) = raw_bounds(tick_current, tick_spacing)?;
    // Pull each boundary toward the middle.
    let lo = lo.checked_add(lo / SAFETY_SHRINK_PPB)?.checked_add(1)?;
    let hi = hi.checked_sub(hi / SAFETY_SHRINK_PPB)?.checked_sub(1)?;
    if lo >= hi {
        return None;
    }
    Some((lo, hi))
}

/// Virtual constant-product reserves, oriented for a swap direction.
///
/// `a_to_b` means spending token A (index 0) to receive token B (index 1), which
/// pushes the price down.
///
/// Rounding is direction-aware and always against us: the input reserve rounds up and
/// the output reserve rounds down, so the constant-product quote built on these is a
/// **lower bound** on the pool's true output. Rounding a single fixed way would be
/// conservative in one direction and generous in the other, and the generous one is
/// the direction that loses money.
#[must_use]
pub fn virtual_reserves_for_input(
    liquidity: u128,
    sqrt_price_x64: u128,
    a_to_b: bool,
) -> Option<(u128, u128)> {
    if liquidity == 0 || sqrt_price_x64 == 0 {
        return None;
    }
    let l = u256(liquidity);
    let sp = u256(sqrt_price_x64);
    let q = u256(Q64);

    // x = L/√P = L·2⁶⁴ / sqrt_price   (reserve of token A)
    // y = L·√P = L·sqrt_price / 2⁶⁴   (reserve of token B)
    let x_num = l.checked_mul(q)?;
    let y_num = l.checked_mul(sp)?;

    let (r_in, r_out) = if a_to_b {
        (x_num.div_ceil(sp), y_num / q)
    } else {
        (y_num.div_ceil(q), x_num / sp)
    };

    let r_in = narrow(r_in)?;
    let r_out = narrow(r_out)?;
    if r_in == 0 || r_out == 0 {
        None
    } else {
        Some((r_in, r_out))
    }
}

/// Largest input, *before* fee, that keeps the swap inside the current tick interval.
///
/// Returns 0 when the price already sits at or past the boundary in that direction,
/// which is the honest answer: there is no depth left on this side of the tick.
///
/// The fee is added back because the caller passes gross input, while the amount that
/// actually moves the price is net of fee. Dividing by `(1 − fee)` and rounding *down*
/// keeps the result an under-estimate.
#[must_use]
pub fn capacity_for_input(
    liquidity: u128,
    sqrt_price_x64: u128,
    sqrt_lo_x64: u128,
    sqrt_hi_x64: u128,
    a_to_b: bool,
    fee_ppm: u32,
) -> Option<u128> {
    if liquidity == 0 || sqrt_price_x64 == 0 {
        return None;
    }
    let l = u256(liquidity);
    let sp = u256(sqrt_price_x64);
    let q = u256(Q64);

    let net = if a_to_b {
        // Spending A drives √P down to sqrt_lo. Δx = L·(1/√P_lo − 1/√P).
        if sqrt_lo_x64 == 0 || sqrt_lo_x64 >= sqrt_price_x64 {
            return Some(0);
        }
        let lo = u256(sqrt_lo_x64);
        // L·2⁶⁴·(√P − √P_lo) / (√P · √P_lo)
        l.checked_mul(q)?.checked_mul(sp.checked_sub(lo)?)? / (sp.checked_mul(lo)?)
    } else {
        // Spending B drives √P up to sqrt_hi. Δy = L·(√P_hi − √P).
        if sqrt_hi_x64 <= sqrt_price_x64 {
            return Some(0);
        }
        let hi = u256(sqrt_hi_x64);
        l.checked_mul(hi.checked_sub(sp)?)? / q
    };

    // Gross up for the fee: gross · (1 − fee) = net.
    let gamma = 1_000_000u128.checked_sub(u128::from(fee_ppm))?;
    if gamma == 0 {
        return Some(0);
    }
    let gross = net.checked_mul(u256(1_000_000))? / u256(gamma);
    narrow(gross)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::cp_swap_out;

    /// Real Whirlpool state captured from mainnet, used to pin the tick invariant.
    /// `(tick_spacing, fee_ppm, tick_current, liquidity, sqrt_price)`.
    const CAPTURED: [(u16, u32, i32, u128, u128); 6] = [
        (4, 400, -23953, 758_634_162_063_829, 5_569_625_019_338_410_820), // SOL/USDC
        (2, 200, -23948, 1_304_106_584_647, 5_570_956_668_682_291_589),   // SOL/USDT
        (1, 100, -2594, 58_128_566_782_986_801, 16_203_694_563_459_600_686), // SOL/JitoSOL
        (8, 500, -23951, 573_387_112_804, 5_570_049_037_602_215_125),     // SOL/USDC 5bp
        (16, 1600, -90434, 12_145_563_517_051, 200_578_137_769_255_952),  // SOL/cbBTC
        (64, 3000, 24223, 76_658_641_011_345, 61_929_289_274_865_515_714), // SOL/PENGU
    ];

    /// The one property the `f64` tick function must have, checked against six real
    /// pools spanning four orders of magnitude of price and every common spacing:
    /// the pool's own `sqrt_price` must sit inside `[tick, tick+1)`.
    #[test]
    fn tick_math_brackets_real_mainnet_prices() {
        for (_, _, tick, _, sqrt_price) in CAPTURED {
            let at = sqrt_price_at_tick(tick).expect("tick in range");
            let next = sqrt_price_at_tick(tick + 1).expect("tick+1 in range");
            assert!(
                at <= sqrt_price && sqrt_price < next,
                "tick {tick}: {at} <= {sqrt_price} < {next} violated"
            );
        }
    }

    #[test]
    fn tick_intervals_floor_toward_negative_infinity() {
        // The bug this guards: -23953 / 4 truncates to -5988 in Rust, which would put
        // the interval at [-23952, -23948) — entirely above the current tick.
        assert_eq!(tick_interval(-23953, 4), Some((-23956, -23952)));
        assert_eq!(tick_interval(-23952, 4), Some((-23952, -23948)));
        assert_eq!(tick_interval(24223, 64), Some((24192, 24256)));
        assert_eq!(tick_interval(100, 1), Some((100, 101)));
        assert_eq!(tick_interval(0, 0), None);
    }

    #[test]
    fn every_captured_pool_has_its_price_inside_its_own_bounds() {
        for (spacing, _, tick, _, sqrt_price) in CAPTURED {
            let (lo, hi) = bounds(tick, spacing).expect("real pool must have bounds");
            assert!(lo < sqrt_price && sqrt_price < hi, "spacing {spacing}: {lo} !< {sqrt_price} !< {hi}");
            assert!(price_belongs_to_tick(sqrt_price, tick, spacing));
        }
    }

    /// A pool sitting exactly on a tick boundary is normal, not broken.
    ///
    /// Treating the inward safety shrink as a membership test rejected eight live
    /// mainnet pools outright — every pool whose price had just touched a tick, plus
    /// every pool still sitting at its initial tick 0. The right answer for those is
    /// "no depth downward, full depth upward", not "do not decode".
    #[test]
    fn a_price_pinned_to_a_tick_boundary_still_belongs_to_that_tick() {
        let at_zero = 1u128 << 64; // exactly sqrt_price_at_tick(0)
        assert!(price_belongs_to_tick(at_zero, 0, 1), "tick 0's own price belongs to tick 0");
        assert!(price_belongs_to_tick(at_zero, 0, 64));

        // The shrunk bounds do step past it. That is what made this look like an error.
        let (lo, _) = bounds(0, 1).unwrap();
        assert!(lo > at_zero);

        // And capacity correctly reports no room downward, full room upward.
        let (lo, hi) = bounds(0, 64).unwrap();
        assert_eq!(capacity_for_input(1_000_000_000, at_zero, lo, hi, true, 100), Some(0));
        assert!(capacity_for_input(1_000_000_000, at_zero, lo, hi, false, 100).unwrap() > 0);
    }

    #[test]
    fn a_price_from_the_wrong_tick_is_rejected() {
        let at_zero = 1u128 << 64;
        assert!(!price_belongs_to_tick(at_zero, -23_953, 4), "a $91 price is not tick 0");
        assert!(!price_belongs_to_tick(at_zero, 1_000, 1));
        assert!(!price_belongs_to_tick(at_zero, 0, 0), "zero spacing has no interval");
    }

    /// The identity the whole module rests on: the constant-product quote over
    /// virtual reserves must equal the direct concentrated-liquidity quote.
    #[test]
    fn virtual_reserves_reproduce_the_concentrated_liquidity_quote() {
        for (_, fee, _, liquidity, sqrt_price) in CAPTURED {
            for a_to_b in [true, false] {
                let (r_in, r_out) =
                    virtual_reserves_for_input(liquidity, sqrt_price, a_to_b).expect("reserves");

                // A few realistic sizes, from dust to a few thousand dollars.
                for amount in [1_000u128, 1_000_000, 100_000_000, 5_000_000_000] {
                    let Some(via_cp) = cp_swap_out(amount, r_in, r_out, fee) else { continue };

                    // Direct concentrated-liquidity math in f64, as an independent
                    // implementation.
                    //
                    // Written in the algebraically-rearranged form that avoids
                    // subtracting two nearly-equal square-root prices. The textbook
                    // form — `L·(√P − √P')` — cancels catastrophically here: a $0.000001
                    // trade against 5.8e16 of liquidity moves √P by about 1e-14, and
                    // f64 has ~1e-16 of absolute resolution at √P ≈ 0.88, so the
                    // difference retains barely two significant digits. That is not a
                    // rounding nuisance, it is a wrong answer, and it is why the
                    // production path stays in integers over virtual reserves.
                    let l = liquidity as f64;
                    let sp = sqrt_price as f64 / Q64 as f64;
                    let net = amount as f64 * (1_000_000.0 - f64::from(fee)) / 1_000_000.0;
                    let direct = if a_to_b {
                        // out = Δx·L·P / (L + Δx·√P)
                        net * l * sp * sp / (l + net * sp)
                    } else {
                        // out = Δy / (√P · √P'), with √P' = √P + Δy/L
                        net / (sp * (sp + net / l))
                    };

                    // The property that matters is not equality — integer quotes
                    // truncate — but that we are never *above* the true output, and
                    // never more than a base unit below it. Over-quoting by even one
                    // unit is a trade that looked profitable and was not.
                    let slack = 1.0 + direct * 1e-9;
                    assert!(
                        (via_cp as f64) <= direct + slack,
                        "over-quoted: cp={via_cp} direct={direct} (a_to_b={a_to_b}, amount={amount})"
                    );
                    assert!(
                        (via_cp as f64) >= direct - slack,
                        "lost more than rounding: cp={via_cp} direct={direct} (a_to_b={a_to_b}, amount={amount})"
                    );
                }
            }
        }
    }

    #[test]
    fn virtual_reserves_round_against_us_in_both_directions() {
        // Input reserve rounds up, output reserve rounds down — in whichever
        // orientation is asked for. Swapping direction must swap which is which.
        let (l, sp) = (1_000_000_000_000u128, 5_569_625_019_338_410_820u128);
        let (in_ab, out_ab) = virtual_reserves_for_input(l, sp, true).unwrap();
        let (in_ba, out_ba) = virtual_reserves_for_input(l, sp, false).unwrap();
        // a→b spends A: r_in is x (rounded up), r_out is y (rounded down).
        // b→a spends B: r_in is y (rounded up), r_out is x (rounded down).
        assert!(in_ab >= out_ba, "x rounded up must be >= x rounded down");
        assert!(in_ba >= out_ab, "y rounded up must be >= y rounded down");
        assert!(in_ab - out_ba <= 1, "rounding must cost at most one base unit");
        assert!(in_ba - out_ab <= 1, "rounding must cost at most one base unit");
    }

    /// Capacity must be large enough that $5 never touches it, and small enough that
    /// it never lets a swap out of the tick.
    #[test]
    fn capacity_is_far_above_our_capital_but_still_bounded() {
        let (spacing, fee, tick, liquidity, sqrt_price) = CAPTURED[0]; // SOL/USDC 4bp
        let (lo, hi) = bounds(tick, spacing).unwrap();

        let cap_a = capacity_for_input(liquidity, sqrt_price, lo, hi, true, fee).unwrap();
        let cap_b = capacity_for_input(liquidity, sqrt_price, lo, hi, false, fee).unwrap();

        // $5 of SOL is ~0.055 SOL = 5.5e7 lamports; $5 of USDC is 5e6 base units.
        assert!(cap_a > 5_500_000_000, "one tick must hold far more than $5 of SOL, got {cap_a}");
        assert!(cap_b > 500_000_000, "one tick must hold far more than $5 of USDC, got {cap_b}");

        // And it is bounded: a tick is not infinite depth.
        assert!(cap_a < liquidity, "capacity must be a fraction of liquidity");
    }

    #[test]
    fn a_price_pinned_to_a_boundary_reports_no_depth_that_way() {
        let (l, sp) = (1_000_000_000_000u128, 5_569_625_019_338_410_820u128);
        // lo == sp: no room to push the price down.
        assert_eq!(capacity_for_input(l, sp, sp, sp * 2, true, 400), Some(0));
        // hi == sp: no room to push it up.
        assert_eq!(capacity_for_input(l, sp, sp / 2, sp, false, 400), Some(0));
    }

    #[test]
    fn degenerate_inputs_return_none_rather_than_panicking() {
        assert!(virtual_reserves_for_input(0, 1, true).is_none());
        assert!(virtual_reserves_for_input(1, 0, true).is_none());
        assert!(capacity_for_input(0, 1, 1, 2, true, 400).is_none());
        assert!(sqrt_price_at_tick(MIN_TICK - 1).is_none());
        assert!(sqrt_price_at_tick(MAX_TICK + 1).is_none());
        assert!(bounds(0, 0).is_none());
    }

    #[test]
    fn extreme_but_legal_state_does_not_overflow() {
        // Maximum plausible liquidity against the extreme ends of the price range.
        for tick in [MIN_TICK, MIN_TICK + 1, -1, 0, 1, MAX_TICK - 1, MAX_TICK] {
            let sp = sqrt_price_at_tick(tick).unwrap();
            let _ = virtual_reserves_for_input(u128::from(u64::MAX), sp, true);
            let _ = virtual_reserves_for_input(u128::from(u64::MAX), sp, false);
            let _ = capacity_for_input(u128::from(u64::MAX), sp, sp / 2, sp * 2, true, 400);
        }
    }
}
