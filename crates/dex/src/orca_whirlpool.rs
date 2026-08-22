//! Orca Whirlpool pool decoding.
//!
//! Program: `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`
//!
//! # Why this venue matters more than any other decoder here
//!
//! Raydium AMM v4 charges 25 bp on every swap. A three-hop triangle across it costs
//! 75 bp before any price dislocation is even considered, and measured dislocation on
//! the majors runs 13–18 bp. That gap is not closable by trading faster.
//!
//! Whirlpool quotes the same pairs at 1, 2, 4 and 5 bp. SOL/USDC is 4 bp, SOL/USDT is
//! 2 bp, SOL/JitoSOL is 1 bp. Moving the same route onto these tiers cuts the fee wall
//! by most of an order of magnitude, which is the only lever that acts on the term
//! that was actually binding.
//!
//! # Layout
//!
//! Anchor-serialised, fixed 653 bytes, no variable-length fields. Unusually for a
//! concentrated-liquidity venue, `liquidity` and `sqrt_price` live *in the pool
//! account*, so one subscription tracks a pool completely — no vault accounts, no
//! torn reads across slots.
//!
//! Offsets verified against six live mainnet pools spanning every common tick spacing
//! on 2026-08-21, cross-checked against the values Orca's own API reports for the same
//! pools.

use anyhow::{ensure, Result};
use cb_core::clmm;
use cb_core::types::{Dex, PoolId, PoolMath, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/// Exact serialised length of the `Whirlpool` account.
pub const WHIRLPOOL_LEN: usize = 653;

// Verified byte offsets.
const OFF_TICK_SPACING: usize = 41;
const OFF_TICK_SPACING_SEED: usize = 43;
const OFF_FEE_RATE: usize = 45;
const OFF_LIQUIDITY: usize = 49;
const OFF_SQRT_PRICE: usize = 65;
const OFF_TICK_CURRENT: usize = 81;
const OFF_MINT_A: usize = 101;
const OFF_MINT_B: usize = 181;

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn i32_at(d: &[u8], o: usize) -> i32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&d[o..o + 4]);
    i32::from_le_bytes(b)
}

fn u128_at(d: &[u8], o: usize) -> u128 {
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[o..o + 16]);
    u128::from_le_bytes(b)
}

fn pubkey_at(d: &[u8], o: usize) -> Pubkey32 {
    let mut k = [0u8; 32];
    k.copy_from_slice(&d[o..o + 32]);
    k
}

/// A decoded Whirlpool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Whirlpool {
    pub mint_a: Pubkey32,
    pub mint_b: Pubkey32,
    pub tick_spacing: u16,
    /// Fee in parts per million — the account's native unit. 400 is 4 bp.
    pub fee_rate_ppm: u32,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
}

/// Decode a Whirlpool account.
///
/// # Rejections
///
/// Rejects **adaptive-fee pools**. Orca overloads the `tick_spacing_seed` field on
/// those to hold a fee-tier index instead of a copy of the tick spacing, and their
/// effective fee carries a volatility surcharge held in a separate oracle account.
/// Quoting one at its base `fee_rate` would understate the fee, which overstates
/// profit — exactly the direction that loses money. Since the disagreement between
/// the two fields is the documented marker, we use it and decline to decode.
pub fn decode(data: &[u8]) -> Result<Whirlpool> {
    ensure!(
        data.len() >= WHIRLPOOL_LEN,
        "whirlpool account too short: {} bytes, need {WHIRLPOOL_LEN}",
        data.len()
    );

    let tick_spacing = u16_at(data, OFF_TICK_SPACING);
    let seed = u16_at(data, OFF_TICK_SPACING_SEED);
    ensure!(tick_spacing > 0, "whirlpool with zero tick spacing");
    ensure!(
        seed == tick_spacing,
        "adaptive-fee whirlpool (fee tier index {seed} != spacing {tick_spacing}): its true fee \
         lives in an oracle account we do not read, so quoting it would understate cost"
    );

    let fee_rate_ppm = u32::from(u16_at(data, OFF_FEE_RATE));
    ensure!(fee_rate_ppm < 1_000_000, "whirlpool fee rate {fee_rate_ppm}ppm is not a fee");

    let tick_current = i32_at(data, OFF_TICK_CURRENT);
    ensure!(
        (clmm::MIN_TICK..=clmm::MAX_TICK).contains(&tick_current),
        "whirlpool tick {tick_current} outside the representable range — wrong layout"
    );

    Ok(Whirlpool {
        mint_a: pubkey_at(data, OFF_MINT_A),
        mint_b: pubkey_at(data, OFF_MINT_B),
        tick_spacing,
        fee_rate_ppm,
        liquidity: u128_at(data, OFF_LIQUIDITY),
        sqrt_price_x64: u128_at(data, OFF_SQRT_PRICE),
        tick_current,
    })
}

/// Decode straight into a [`PoolState`], resolving the tick interval the quote is
/// valid inside.
///
/// Returns an error when the pool has no liquidity at the current price, or when the
/// price sits outside its own tick's bounds — both mean there is nothing quotable
/// here, and both are normal for thin pools rather than signs of a decode error.
pub fn to_pool_state(address: Pubkey32, data: &[u8], slot: u64) -> Result<PoolState> {
    let w = decode(data)?;
    ensure!(w.liquidity > 0, "whirlpool has no liquidity at the current price");

    ensure!(
        clmm::price_belongs_to_tick(w.sqrt_price_x64, w.tick_current, w.tick_spacing),
        "sqrt price {} does not belong to tick {} at spacing {} — the account is \
         mid-update or the layout drifted",
        w.sqrt_price_x64,
        w.tick_current,
        w.tick_spacing
    );
    // These are the *shrunk* bounds and may sit on the wrong side of the current price
    // when the pool is parked on a tick boundary. Deliberate: capacity then reads zero
    // in the pinned direction while the other direction keeps quoting.
    let (sqrt_lo_x64, sqrt_hi_x64) = clmm::bounds(w.tick_current, w.tick_spacing)
        .ok_or_else(|| anyhow::anyhow!("could not bound tick {}", w.tick_current))?;

    Ok(PoolState {
        id: PoolId(address),
        dex: Dex::OrcaWhirlpool,
        mint_a: w.mint_a,
        mint_b: w.mint_b,
        math: PoolMath::Concentrated {
            liquidity: w.liquidity,
            sqrt_price_x64: w.sqrt_price_x64,
            sqrt_lo_x64,
            sqrt_hi_x64,
        },
        fee_ppm: w.fee_rate_ppm,
        slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic account with the verified layout. Everything not named is
    /// zero, which is exactly what a decoder should be indifferent to.
    // One argument per field the layout actually has; grouping them would only hide
    // which byte each test is varying.
    #[allow(clippy::too_many_arguments)]
    fn account(
        tick_spacing: u16,
        seed: u16,
        fee: u16,
        liquidity: u128,
        sqrt_price: u128,
        tick: i32,
        mint_a: u8,
        mint_b: u8,
    ) -> Vec<u8> {
        let mut d = vec![0u8; WHIRLPOOL_LEN];
        d[OFF_TICK_SPACING..OFF_TICK_SPACING + 2].copy_from_slice(&tick_spacing.to_le_bytes());
        d[OFF_TICK_SPACING_SEED..OFF_TICK_SPACING_SEED + 2].copy_from_slice(&seed.to_le_bytes());
        d[OFF_FEE_RATE..OFF_FEE_RATE + 2].copy_from_slice(&fee.to_le_bytes());
        d[OFF_LIQUIDITY..OFF_LIQUIDITY + 16].copy_from_slice(&liquidity.to_le_bytes());
        d[OFF_SQRT_PRICE..OFF_SQRT_PRICE + 16].copy_from_slice(&sqrt_price.to_le_bytes());
        d[OFF_TICK_CURRENT..OFF_TICK_CURRENT + 4].copy_from_slice(&tick.to_le_bytes());
        d[OFF_MINT_A..OFF_MINT_A + 32].copy_from_slice(&[mint_a; 32]);
        d[OFF_MINT_B..OFF_MINT_B + 32].copy_from_slice(&[mint_b; 32]);
        d
    }

    /// Real SOL/USDC 4bp state, captured 2026-08-21.
    fn sol_usdc() -> Vec<u8> {
        account(4, 4, 400, 758_634_162_063_829, 5_569_625_019_338_410_820, -23953, 1, 2)
    }

    #[test]
    fn decodes_real_sol_usdc_state() {
        let w = decode(&sol_usdc()).unwrap();
        assert_eq!(w.tick_spacing, 4);
        assert_eq!(w.fee_rate_ppm, 400, "4 bp, stored as parts per million");
        assert_eq!(w.tick_current, -23953);
        assert_eq!(w.liquidity, 758_634_162_063_829);
        assert_eq!(w.mint_a, [1u8; 32]);
        assert_eq!(w.mint_b, [2u8; 32]);
    }

    #[test]
    fn pool_state_prices_sol_at_about_ninety_one_dollars() {
        let p = to_pool_state([9u8; 32], &sol_usdc(), 500).unwrap();
        assert_eq!(p.dex, Dex::OrcaWhirlpool);
        assert_eq!(p.fee_ppm, 400);
        assert_eq!(p.slot, 500);
        // SOL is token A at 9 decimals, USDC token B at 6, so multiply by 10^3.
        let ui = p.spot_price().unwrap() * 1000.0;
        assert!((ui - 91.0).abs() < 1.5, "expected roughly $91, got {ui}");
    }

    #[test]
    fn both_directions_quote_and_are_bounded() {
        let p = to_pool_state([9u8; 32], &sol_usdc(), 1).unwrap();
        let sol_in = p.leg_for_input(&[1u8; 32]).expect("must quote SOL in");
        let usdc_in = p.leg_for_input(&[2u8; 32]).expect("must quote USDC in");
        assert!(sol_in.max_in < u128::MAX && usdc_in.max_in < u128::MAX);
        // One tick at spacing 4 on a $24M pool holds far more than our whole account.
        assert!(sol_in.max_in > 5_500_000_000, "should hold well over $5 of SOL");
        assert!(usdc_in.max_in > 500_000_000, "should hold well over $5 of USDC");
    }

    /// The rejection that protects the money: an adaptive-fee pool's real fee is not
    /// the one in this account.
    #[test]
    fn adaptive_fee_pools_are_refused() {
        // Same pool, but the seed field holds a fee-tier index instead of the spacing.
        let d = account(4, 1024, 400, 758_634_162_063_829, 5_569_625_019_338_410_820, -23953, 1, 2);
        let err = decode(&d).unwrap_err().to_string();
        assert!(err.contains("adaptive-fee"), "unexpected error: {err}");
    }

    #[test]
    fn a_pool_with_no_liquidity_is_not_a_pool() {
        let d = account(4, 4, 400, 0, 5_569_625_019_338_410_820, -23953, 1, 2);
        assert!(to_pool_state([9u8; 32], &d, 1).is_err());
    }

    /// A sqrt price that does not belong to the reported tick means we are looking at
    /// a half-written account or the layout moved. Either way, do not quote it.
    #[test]
    fn price_inconsistent_with_its_own_tick_is_rejected() {
        let d = account(4, 4, 400, 758_634_162_063_829, 5_569_625_019_338_410_820, 0, 1, 2);
        let err = to_pool_state([9u8; 32], &d, 1).unwrap_err().to_string();
        assert!(err.contains("does not belong to tick"), "unexpected error: {err}");
    }

    #[test]
    fn short_accounts_are_rejected_rather_than_read_out_of_bounds() {
        assert!(decode(&[]).is_err());
        assert!(decode(&vec![0u8; WHIRLPOOL_LEN - 1]).is_err());
    }

    #[test]
    fn nonsense_field_values_are_rejected() {
        // Zero tick spacing would divide by zero downstream.
        assert!(decode(&account(0, 0, 400, 1, 1, 0, 1, 2)).is_err());
        // A tick far outside the representable range means we read the wrong bytes.
        assert!(decode(&account(4, 4, 400, 1, 1, i32::MAX, 1, 2)).is_err());
    }

    /// Every common Orca fee tier must survive decoding at full precision. Rounding
    /// 100 ppm into "0 bp" or "1 bp plus change" is the failure this venue exists to
    /// avoid.
    #[test]
    fn every_live_fee_tier_round_trips_exactly() {
        for (spacing, fee) in [(1u16, 100u16), (2, 200), (4, 400), (8, 500), (16, 1600), (64, 3000), (96, 6500)] {
            let d = account(spacing, spacing, fee, 1_000_000_000, 5_569_625_019_338_410_820, -23953, 1, 2);
            assert_eq!(decode(&d).unwrap().fee_rate_ppm, u32::from(fee));
        }
    }
}
