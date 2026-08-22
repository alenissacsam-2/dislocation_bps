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
}

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
    /// concentrated one. For display and depth comparison only.
    #[must_use]
    pub fn reserve_a(&self) -> u128 {
        match self.math {
            PoolMath::ConstantProduct { reserve_a, .. } => reserve_a,
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
            PoolMath::ConstantProduct { reserve_b, .. } => reserve_b,
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
