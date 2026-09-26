//! Core domain types. Deliberately free of Solana SDK dependencies so this crate
//! stays pure and fast to compile; pubkeys are raw 32-byte arrays and are converted
//! at the edges.

use crate::clmm;
use crate::path::Leg;

/// A raw Solana public key. Kept as bytes so `cb-core` needs no solana-sdk dependency.
pub type Pubkey32 = [u8; 32];

/// Identifier for a liquidity pool — its on-chain account address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolId(pub Pubkey32);

/// Which venue a pool belongs to. Determines the decoder used to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dex {
    RaydiumAmmV4,
    PumpSwap,
    OrcaWhirlpool,
    RaydiumClmm,
    RaydiumCpmm,
    MeteoraDammV2,
    MeteoraDlmm,
}

impl Dex {
    /// Human-readable name, used in logs and on the dashboard.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Dex::RaydiumAmmV4 => "Raydium AMM v4",
            Dex::PumpSwap => "PumpSwap",
            Dex::OrcaWhirlpool => "Orca Whirlpool",
            Dex::RaydiumClmm => "Raydium CLMM",
            Dex::RaydiumCpmm => "Raydium CP-Swap",
            Dex::MeteoraDammV2 => "Meteora DAMM v2",
            Dex::MeteoraDlmm => "Meteora DLMM",
        }
    }

    /// Short tag for dense table columns.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Dex::RaydiumAmmV4 => "RAY-V4",
            Dex::PumpSwap => "PUMP",
            Dex::OrcaWhirlpool => "ORCA",
            Dex::RaydiumClmm => "RAY-CL",
            Dex::RaydiumCpmm => "RAY-CP",
            Dex::MeteoraDammV2 => "MET-D2",
            Dex::MeteoraDlmm => "MET-DL",
        }
    }

    /// Whether pool state lives entirely in the pool account.
    ///
    /// Concentrated-liquidity pools carry `liquidity` and `sqrt_price` inline, so one
    /// WebSocket subscription tracks them completely. Constant-product pools keep
    /// their balances in separate SPL token vaults, which cost two more subscriptions
    /// each and can be read torn across slots. That difference decides how many pools
    /// fit inside an RPC provider's subscription budget.
    #[must_use]
    pub fn is_self_contained(self) -> bool {
        matches!(self, Dex::OrcaWhirlpool | Dex::RaydiumClmm | Dex::MeteoraDammV2)
    }
}

/// Reserves oriented for one swap direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserves {
    /// Reserve of the mint being spent.
    pub r_in: u128,
    /// Reserve of the mint being received.
    pub r_out: u128,
}

/// How a pool prices a swap.
///
/// Both variants quote through the same constant-product formula — see
/// [`crate::clmm`] for why a concentrated-liquidity pool inside its tick is exactly a
/// constant-product pool over virtual reserves. What differs is where the reserves
/// come from and whether the quote has a size limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolMath {
    /// Real token balances held in vaults. Exact at any size.
    ConstantProduct { reserve_a: u128, reserve_b: u128 },
    /// Concentrated liquidity, valid inside `[sqrt_lo_x64, sqrt_hi_x64]` — the
    /// tick-spacing-aligned interval around the current price, within which the
    /// pool's `liquidity` provably cannot change.
    Concentrated {
        liquidity: u128,
        sqrt_price_x64: u128,
        sqrt_lo_x64: u128,
        sqrt_hi_x64: u128,
    },
    /// A curve quoted from an explicit rate, exact only up to a bound, and with a
    /// different rate in each direction.
    ///
    /// # What this is for
    ///
    /// A binned venue — Meteora DLMM — holds liquidity at discrete prices. Inside one
    /// bin the price does not move at all: the bin is constant-*sum*, and it stops dead
    /// when its reserve on the output side runs out. Neither other variant describes
    /// that. [`PoolMath::ConstantProduct`] would quote straight past the bin, and
    /// [`PoolMath::Concentrated`] derives its bound from a tick geometry this venue
    /// does not have.
    ///
    /// # Why the two directions cannot share one number
    ///
    /// A bin holds only the token the price has not yet reached, so on a live pool the
    /// bin that fills a sale and the bin that fills a purchase are frequently *not the
    /// same bin*. Their prices then differ by at least one bin step, and a single
    /// reserve ratio cannot sit on the safe side of both — whichever one it flatters is
    /// a quote the pool will not honour. So each direction carries its own rate and its
    /// own depth, and `cb_dex::meteora_dlmm` fills them from the specific bin that
    /// direction will actually be filled by.
    ///
    /// The rates are output-per-input in Q64 raw base units. [`PoolState::leg_for_input`]
    /// turns one into a constant-product leg over reserves large enough that the curve
    /// through them is flat to a rounding error across the whole permitted range, so the
    /// quote lands a hair **under** the constant-sum truth — the only direction an
    /// approximation of a fill is allowed to err in.
    /// Constant product whose fee is taken on the quote side in both directions: out of
    /// what a sale of the base token receives, and out of what a purchase spends.
    /// PumpSwap prices this way.
    ///
    /// The pool's `fee_ppm` is the whole fee. [`PoolState::leg_for_input`] turns each
    /// direction into an ordinary constant-product leg, exactly:
    ///
    /// - **Selling the base token**, the fee comes off the output, and a curve whose
    ///   output is scaled by `1 - f` is the same curve over an output reserve scaled by
    ///   `1 - f` with no fee at all.
    /// - **Buying with the quote token**, `spend / (1 + f)` reaches the curve and the
    ///   rest is fee, which is a fee of `f / (1 + f)` on input.
    ///
    /// Both are shaded by [`QUOTE_SIDE_FEE_MARGIN_PPM`] against us, because the program
    /// rounds each of its fee shares separately and up.
    QuoteSideFee {
        /// Real balance of token A.
        reserve_a: u128,
        /// Real balance of token B.
        reserve_b: u128,
        /// Whether token B is the quote token (it is for every PumpSwap pool: A is the
        /// base mint and B the quote mint).
        quote_is_b: bool,
    },
    Bounded {
        /// Output per unit of input when spending token A, Q64. Zero when that
        /// direction cannot be filled at all.
        rate_a_x64: u128,
        /// Largest input, in A's base units, this rate is exact for.
        max_in_a: u128,
        /// Output per unit of input when spending token B, Q64.
        rate_b_x64: u128,
        /// Largest input, in B's base units, this rate is exact for.
        max_in_b: u128,
    },
}

/// Extra fee assumed on a [`PoolMath::QuoteSideFee`] leg, in ppm, to cover the program
/// rounding each fee share up separately: a handful of base units on any output, which
/// two ppm covers for every output above a few million base units.
pub const QUOTE_SIDE_FEE_MARGIN_PPM: u128 = 2;

/// A decoded pool at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    pub id: PoolId,
    pub dex: Dex,
    /// Token A. For concentrated pools this is token 0, and price is quoted as
    /// B-per-A, so spending A pushes the price down.
    pub mint_a: Pubkey32,
    pub mint_b: Pubkey32,
    pub math: PoolMath,
    /// Swap fee in parts per million.
    pub fee_ppm: u32,
    /// Slot this state was observed at. Used for staleness checks.
    pub slot: u64,
}

impl PoolState {
    /// Build a constant-product pool. Convenience for decoders and tests.
    #[must_use]
    // Eight arguments, and all eight are distinct facts about a pool with no natural
    // grouping between them. Bundling them into a struct to satisfy the lint would
    // just move the same eight fields one level out.
    #[allow(clippy::too_many_arguments)]
    pub fn constant_product(
        id: PoolId,
        dex: Dex,
        mint_a: Pubkey32,
        mint_b: Pubkey32,
        reserve_a: u128,
        reserve_b: u128,
        fee_ppm: u32,
        slot: u64,
    ) -> Self {
        Self {
            id,
            dex,
            mint_a,
            mint_b,
            math: PoolMath::ConstantProduct { reserve_a, reserve_b },
            fee_ppm,
            slot,
        }
    }

    /// A quotable leg for spending `input_mint`, or `None` if this pool doesn't trade it.
    ///
    /// This is the function the scanner uses. It carries the size bound that
    /// [`reserves_for_input`](Self::reserves_for_input) throws away, so a
    /// concentrated-liquidity leg cannot be sized past the tick it is valid in.
    #[must_use]
    pub fn leg_for_input(&self, input_mint: &Pubkey32) -> Option<Leg> {
        let a_to_b = if *input_mint == self.mint_a {
            true
        } else if *input_mint == self.mint_b {
            false
        } else {
            return None;
        };

        match self.math {
            PoolMath::ConstantProduct { reserve_a, reserve_b } => {
                let (r_in, r_out) =
                    if a_to_b { (reserve_a, reserve_b) } else { (reserve_b, reserve_a) };
                if r_in == 0 || r_out == 0 {
                    return None;
                }
                Some(Leg::cp(r_in, r_out, self.fee_ppm))
            }
            PoolMath::QuoteSideFee { reserve_a, reserve_b, quote_is_b } => {
                let (r_in, r_out) =
                    if a_to_b { (reserve_a, reserve_b) } else { (reserve_b, reserve_a) };
                if r_in == 0 || r_out == 0 {
                    return None;
                }
                let f = u128::from(self.fee_ppm) + QUOTE_SIDE_FEE_MARGIN_PPM;
                if f >= 1_000_000 {
                    return None;
                }
                // Spending A while the quote is B (or B while it is A) is a sale.
                if a_to_b == quote_is_b {
                    Some(Leg::cp(r_in, r_out * (1_000_000 - f) / 1_000_000, 0))
                } else {
                    let on_input = (f * 1_000_000).div_ceil(1_000_000 + f);
                    Some(Leg::cp(r_in, r_out, u32::try_from(on_input).ok()?))
                }
            }
            PoolMath::Bounded { rate_a_x64, max_in_a, rate_b_x64, max_in_b } => {
                let (rate, max_in) =
                    if a_to_b { (rate_a_x64, max_in_a) } else { (rate_b_x64, max_in_b) };
                let (r_in, r_out) = flat_reserves(rate, max_in)?;
                Some(Leg::bounded(r_in, r_out, self.fee_ppm, max_in))
            }
            PoolMath::Concentrated { liquidity, sqrt_price_x64, sqrt_lo_x64, sqrt_hi_x64 } => {
                let (r_in, r_out) =
                    clmm::virtual_reserves_for_input(liquidity, sqrt_price_x64, a_to_b)?;
                let max_in = clmm::capacity_for_input(
                    liquidity,
                    sqrt_price_x64,
                    sqrt_lo_x64,
                    sqrt_hi_x64,
                    a_to_b,
                    self.fee_ppm,
                )?;
                if max_in == 0 {
                    return None;
                }
                Some(Leg::bounded(r_in, r_out, self.fee_ppm, max_in))
            }
        }
    }

    /// Reserves oriented so `r_in` is the reserve of `input_mint`, virtual for a
    /// concentrated pool.
    ///
    /// Prefer [`leg_for_input`](Self::leg_for_input): this drops the tick bound, so a
    /// size derived from it can exceed what the pool will actually honour.
    #[must_use]
    pub fn reserves_for_input(&self, input_mint: &Pubkey32) -> Option<Reserves> {
        self.leg_for_input(input_mint).map(|l| Reserves { r_in: l.reserve_in, r_out: l.reserve_out })
    }

    /// The counterparty mint to `mint`, or `None` if this pool doesn't trade it.
    #[must_use]
    pub fn other_mint(&self, mint: &Pubkey32) -> Option<Pubkey32> {
        if *mint == self.mint_a {
            Some(self.mint_b)
        } else if *mint == self.mint_b {
            Some(self.mint_a)
        } else {
            None
        }
    }

    /// Reserve of token A — real for a constant-product pool, virtual for a
    /// concentrated one, and the depth of the fillable bin for a binned one. For
    /// display and depth comparison only.
    #[must_use]
    pub fn reserve_a(&self) -> u128 {
        match self.math {
            PoolMath::ConstantProduct { reserve_a, .. } | PoolMath::QuoteSideFee { reserve_a, .. } => {
                reserve_a
            }
            // Deliberately the *real* depth and not the flat reserve the quote is
            // built from: that number is an arithmetic device chosen to be enormous,
            // and reporting it as a reserve would put a fictional depth on the
            // dashboard and in every depth comparison that ranks pools.
            PoolMath::Bounded { max_in_a, .. } => max_in_a,
            PoolMath::Concentrated { liquidity, sqrt_price_x64, .. } => {
                clmm::virtual_reserves_for_input(liquidity, sqrt_price_x64, true)
                    .map_or(0, |(r_in, _)| r_in)
            }
        }
    }

    /// Reserve of token B. See [`reserve_a`](Self::reserve_a).
    #[must_use]
    pub fn reserve_b(&self) -> u128 {
        match self.math {
            PoolMath::ConstantProduct { reserve_b, .. } | PoolMath::QuoteSideFee { reserve_b, .. } => {
                reserve_b
            }
            PoolMath::Bounded { max_in_b, .. } => max_in_b,
            PoolMath::Concentrated { liquidity, sqrt_price_x64, .. } => {
                clmm::virtual_reserves_for_input(liquidity, sqrt_price_x64, false)
                    .map_or(0, |(r_in, _)| r_in)
            }
        }
    }

    /// Mid price of A in units of B, in raw base units, ignoring fees.
    ///
    /// A reporting number: it feeds the dashboard's price column, never a trade size.
    #[must_use]
    pub fn spot_price(&self) -> Option<f64> {
        // A binned pool's two depths are in different tokens and their ratio is not a
        // price, so the rate is read directly. Either direction will do — they differ
        // by a bin step — and one of them can legitimately be missing, because a bin
        // holds only the token the price has not reached yet.
        if let PoolMath::Bounded { rate_a_x64, rate_b_x64, .. } = self.math {
            let q64 = clmm::Q64 as f64;
            return if rate_a_x64 > 0 {
                Some(rate_a_x64 as f64 / q64)
            } else if rate_b_x64 > 0 {
                Some(q64 / rate_b_x64 as f64)
            } else {
                None
            };
        }
        let (a, b) = (self.reserve_a(), self.reserve_b());
        if a == 0 {
            return None;
        }
        Some(b as f64 / a as f64)
    }

    /// Largest trade this pool can price exactly when spending `input_mint`.
    ///
    /// Unbounded for constant-product pools; the current tick's depth for
    /// concentrated ones. Surfaced so "we declined to quote" shows up as a number on
    /// the dashboard instead of a silently missing route.
    #[must_use]
    pub fn quotable_depth(&self, input_mint: &Pubkey32) -> Option<u128> {
        self.leg_for_input(input_mint).map(|l| l.max_in)
    }
}

/// Narrowest headroom, as a power of two, between a flat curve's permitted size and the
/// reserves it is built from.
///
/// The constant-product quote over those reserves falls short of the flat truth by
/// about `amount_in / reserve_in`, so this bounds the shortfall at `2⁻²⁰` — under a
/// hundredth of a basis point, which is two orders of magnitude below the rounding in
/// the fee itself. Any less headroom and the approximation starts to be visible in the
/// only number that matters.
const MIN_FLAT_HEADROOM_BITS: u32 = 20;
/// Headroom aimed for before settling for less. `2⁻⁴⁰` is beneath integer resolution at
/// every size this trades.
const FLAT_HEADROOM_BITS: u32 = 40;

/// Constant-product reserves whose curve is flat, at rate `rate_x64`, across `max_in`.
///
/// # Why reserves at all
///
/// Everything downstream of [`PoolState`] quotes through one formula — see
/// [`crate::path::Leg`] — and a flat rate is not that formula. Rather than give the
/// router a second kind of leg to reason about, the rate is expressed *as* a
/// constant-product pool so deep that its curvature vanishes over the whole range the
/// quote is allowed to cover. Deep enough is made precise by
/// [`MIN_FLAT_HEADROOM_BITS`], and the residual curvature always bends the quote
/// **down**, so what comes out is a floor on the real fill rather than a hope.
///
/// `None` when no headroom in range avoids overflowing a `u128`, or when the rate is too
/// small to survive the scaling — both of which mean this is not a leg anyone can size,
/// and refusing is the honest answer.
fn flat_reserves(rate_x64: u128, max_in: u128) -> Option<(u128, u128)> {
    if rate_x64 == 0 || max_in == 0 {
        return None;
    }
    for bits in (MIN_FLAT_HEADROOM_BITS..=FLAT_HEADROOM_BITS).rev() {
        let Some(r_in) = max_in.checked_mul(1u128 << bits) else { continue };
        let Some(r_out) = clmm::mul_shr_q64(r_in, rate_x64) else { continue };
        // A rate this small rounds the whole output side away, which would quote zero
        // for every size rather than a small number.
        if r_out == 0 {
            continue;
        }
        return Some((r_in, r_out));
    }
    None
}

/// A detected (not executed) arbitrage opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opportunity {
    pub pool_buy: PoolId,
    pub pool_sell: PoolId,
    /// Mint we start and end with (the token we hold).
    pub base_mint: Pubkey32,
    /// Intermediate mint we route through.
    pub quote_mint: Pubkey32,
    /// Optimal input size in base-mint base units.
    pub amount_in: u128,
    /// Gross profit in base-mint base units, before fees and tip.
    pub gross_profit: u128,
    pub slot: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Pubkey32 = [10u8; 32];
    const B: Pubkey32 = [20u8; 32];
    const OTHER: Pubkey32 = [99u8; 32];

    fn cp_pool(reserve_a: u128, reserve_b: u128) -> PoolState {
        PoolState::constant_product(
            PoolId([1u8; 32]),
            Dex::RaydiumAmmV4,
            A,
            B,
            reserve_a,
            reserve_b,
            2500,
            42,
        )
    }

    /// Real Orca SOL/USDC state: 4 bp tier, tick spacing 4.
    fn clmm_pool() -> PoolState {
        let (lo, hi) = clmm::bounds(-23953, 4).unwrap();
        PoolState {
            id: PoolId([2u8; 32]),
            dex: Dex::OrcaWhirlpool,
            mint_a: A,
            mint_b: B,
            math: PoolMath::Concentrated {
                liquidity: 758_634_162_063_829,
                sqrt_price_x64: 5_569_625_019_338_410_820,
                sqrt_lo_x64: lo,
                sqrt_hi_x64: hi,
            },
            fee_ppm: 400,
            slot: 100,
        }
    }

    #[test]
    fn constant_product_reserves_orient_by_direction() {
        let p = cp_pool(1_000_000, 2_000_000);
        assert_eq!(
            p.reserves_for_input(&A),
            Some(Reserves { r_in: 1_000_000, r_out: 2_000_000 })
        );
        assert_eq!(
            p.reserves_for_input(&B),
            Some(Reserves { r_in: 2_000_000, r_out: 1_000_000 })
        );
        assert_eq!(p.reserves_for_input(&OTHER), None);
    }

    #[test]
    fn constant_product_legs_are_unbounded() {
        let leg = cp_pool(1_000_000, 2_000_000).leg_for_input(&A).unwrap();
        assert_eq!(leg.max_in, u128::MAX, "a real constant-product curve holds at any size");
        assert_eq!(leg.fee_ppm, 2500);
    }

    #[test]
    fn concentrated_legs_carry_the_tick_bound() {
        let p = clmm_pool();
        for mint in [A, B] {
            let leg = p.leg_for_input(&mint).unwrap();
            assert!(leg.max_in < u128::MAX, "a concentrated leg must be bounded");
            assert!(leg.max_in > 0);
            assert_eq!(leg.fee_ppm, 400);
            assert!(leg.reserve_in > 0 && leg.reserve_out > 0);
        }
        assert_eq!(p.leg_for_input(&OTHER), None);
    }

    /// The virtual reserves must reproduce the price the pool actually reports.
    /// SOL is token A at 9 decimals, USDC token B at 6, so the raw price is the UI
    /// price divided by 1000 — about 0.0911 at the time this state was captured.
    #[test]
    fn concentrated_spot_price_matches_the_pools_own_sqrt_price() {
        let p = clmm_pool();
        let expected = (5_569_625_019_338_410_820f64 / clmm::Q64 as f64).powi(2);
        let got = p.spot_price().unwrap();
        assert!((got / expected - 1.0).abs() < 1e-9, "spot {got} vs sqrt-price {expected}");
        assert!((got * 1000.0 - 91.0).abs() < 1.0, "should read as roughly $91 SOL");
    }

    #[test]
    fn other_mint_returns_the_counterparty() {
        let p = cp_pool(1, 1);
        assert_eq!(p.other_mint(&A), Some(B));
        assert_eq!(p.other_mint(&B), Some(A));
        assert_eq!(p.other_mint(&OTHER), None);
    }

    #[test]
    fn an_empty_constant_product_pool_has_no_leg() {
        assert_eq!(cp_pool(0, 1_000_000).leg_for_input(&A), None);
        assert_eq!(cp_pool(1_000_000, 0).leg_for_input(&A), None);
    }

    #[test]
    fn a_concentrated_pool_with_no_liquidity_has_no_leg() {
        let mut p = clmm_pool();
        p.math = PoolMath::Concentrated {
            liquidity: 0,
            sqrt_price_x64: 5_569_625_019_338_410_820,
            sqrt_lo_x64: 1,
            sqrt_hi_x64: u128::MAX,
        };
        assert_eq!(p.leg_for_input(&A), None);
    }

    /// A price sitting exactly on a tick boundary has no depth in that direction. The
    /// pool must drop out of routing rather than quote a trade it cannot honour.
    #[test]
    fn a_pool_pinned_to_its_tick_boundary_drops_out_of_routing() {
        let sp = 5_569_625_019_338_410_820u128;
        let mut p = clmm_pool();
        p.math = PoolMath::Concentrated {
            liquidity: 758_634_162_063_829,
            sqrt_price_x64: sp,
            sqrt_lo_x64: sp,
            sqrt_hi_x64: sp * 2,
        };
        assert_eq!(p.leg_for_input(&A), None, "no room to push the price down");
        assert!(p.leg_for_input(&B).is_some(), "but there is room the other way");
    }
}
