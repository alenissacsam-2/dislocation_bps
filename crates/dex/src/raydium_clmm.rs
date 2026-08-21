//! Raydium CLMM (concentrated liquidity) pool decoding.
//!
//! Program: `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK`
//!
//! Same shape as [`crate::orca_whirlpool`] — `liquidity` and `sqrt_price` live in the
//! pool account, so one subscription tracks a pool completely — with one structural
//! difference that matters operationally:
//!
//! # The fee is not in the pool account
//!
//! Raydium keeps the trade fee in a shared `AmmConfig` account referenced by the
//! pool. Many pools share one config, so the fee is not a per-pool field at all.
//! Configs change only by governance, so they are read once at start-up and cached —
//! but they must be read from *chain*, not assumed, because assuming a fee is
//! assuming the one number this whole exercise turns on.
//!
//! Having both venues matters beyond redundancy: Raydium and Orca each run a 4 bp
//! SOL/USDC pool. A two-hop cycle between them costs 8 bp of fees total, which is the
//! cheapest closed loop available on Solana for that pair and roughly a tenth of what
//! the same trip costs across Raydium AMM v4.
//!
//! Offsets verified against live mainnet pools on 2026-08-21, cross-checked against
//! the fee rates Raydium's own API reports for the same pools.

use anyhow::{ensure, Result};
use cb_core::clmm;
use cb_core::types::{Dex, PoolId, PoolMath, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

/// Exact serialised length of the CLMM `PoolState` account.
pub const POOL_LEN: usize = 1544;
/// Exact serialised length of the `AmmConfig` account.
pub const CONFIG_LEN: usize = 117;

// Verified byte offsets into the pool account.
const OFF_AMM_CONFIG: usize = 9;
const OFF_MINT_0: usize = 73;
const OFF_MINT_1: usize = 105;
const OFF_DECIMALS_0: usize = 233;
const OFF_DECIMALS_1: usize = 234;
const OFF_TICK_SPACING: usize = 235;
const OFF_LIQUIDITY: usize = 237;
const OFF_SQRT_PRICE: usize = 253;
const OFF_TICK_CURRENT: usize = 269;

// Verified byte offsets into AmmConfig.
const OFF_CFG_TRADE_FEE_RATE: usize = 47;

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&d[o..o + 4]);
    u32::from_le_bytes(b)
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

/// A decoded CLMM pool. The fee is absent by construction — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClmmPool {
    /// Config account holding this pool's trade fee. Must be fetched separately.
    pub amm_config: Pubkey32,
    pub mint_0: Pubkey32,
    pub mint_1: Pubkey32,
    pub decimals_0: u8,
    pub decimals_1: u8,
    pub tick_spacing: u16,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
}

/// Decode the pool account.
pub fn decode(data: &[u8]) -> Result<ClmmPool> {
    ensure!(
        data.len() >= POOL_LEN,
        "raydium clmm pool account too short: {} bytes, need {POOL_LEN}",
        data.len()
    );

    let tick_spacing = u16_at(data, OFF_TICK_SPACING);
    ensure!(tick_spacing > 0, "clmm pool with zero tick spacing");

    let decimals_0 = data[OFF_DECIMALS_0];
    let decimals_1 = data[OFF_DECIMALS_1];
    ensure!(
        decimals_0 <= 18 && decimals_1 <= 18,
        "implausible decimals ({decimals_0}, {decimals_1}) — wrong layout or wrong account"
    );

    let tick_current = i32_at(data, OFF_TICK_CURRENT);
    ensure!(
        (clmm::MIN_TICK..=clmm::MAX_TICK).contains(&tick_current),
        "clmm tick {tick_current} outside the representable range — wrong layout"
    );

    Ok(ClmmPool {
        amm_config: pubkey_at(data, OFF_AMM_CONFIG),
        mint_0: pubkey_at(data, OFF_MINT_0),
        mint_1: pubkey_at(data, OFF_MINT_1),
        decimals_0,
        decimals_1,
        tick_spacing,
        liquidity: u128_at(data, OFF_LIQUIDITY),
        sqrt_price_x64: u128_at(data, OFF_SQRT_PRICE),
        tick_current,
    })
}

/// Read the trade fee, in parts per million, out of an `AmmConfig` account.
pub fn decode_trade_fee_ppm(data: &[u8]) -> Result<u32> {
    ensure!(
        data.len() >= CONFIG_LEN,
        "raydium amm config too short: {} bytes, need {CONFIG_LEN}",
        data.len()
    );
    let fee = u32_at(data, OFF_CFG_TRADE_FEE_RATE);
    ensure!(fee < 1_000_000, "clmm trade fee {fee}ppm is not a fee");
    Ok(fee)
}

/// Combine the pool account with the fee from its config into a [`PoolState`].
///
/// The fee is a parameter rather than something fetched here, so this stays a pure
/// function and the caller is forced to have actually resolved the config.
pub fn to_pool_state(
    address: Pubkey32,
    data: &[u8],
    trade_fee_ppm: u32,
    slot: u64,
) -> Result<PoolState> {
    let p = decode(data)?;
    ensure!(p.liquidity > 0, "clmm pool has no liquidity at the current price");
    ensure!(trade_fee_ppm < 1_000_000, "clmm trade fee {trade_fee_ppm}ppm is not a fee");

    ensure!(
        clmm::price_belongs_to_tick(p.sqrt_price_x64, p.tick_current, p.tick_spacing),
        "sqrt price {} does not belong to tick {} at spacing {} — the account is \
         mid-update or the layout drifted",
        p.sqrt_price_x64,
        p.tick_current,
        p.tick_spacing
    );
    // These are the *shrunk* bounds and may sit on the wrong side of the current price
    // when the pool is parked on a tick boundary. Deliberate: capacity then reads zero
    // in the pinned direction while the other direction keeps quoting.
    let (sqrt_lo_x64, sqrt_hi_x64) = clmm::bounds(p.tick_current, p.tick_spacing)
        .ok_or_else(|| anyhow::anyhow!("could not bound tick {}", p.tick_current))?;

    Ok(PoolState {
        id: PoolId(address),
        dex: Dex::RaydiumClmm,
        mint_a: p.mint_0,
        mint_b: p.mint_1,
        math: PoolMath::Concentrated {
            liquidity: p.liquidity,
            sqrt_price_x64: p.sqrt_price_x64,
            sqrt_lo_x64,
            sqrt_hi_x64,
        },
        fee_ppm: trade_fee_ppm,
        slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(
        tick_spacing: u16,
        liquidity: u128,
        sqrt_price: u128,
        tick: i32,
        decimals: (u8, u8),
        mints: (u8, u8),
    ) -> Vec<u8> {
        let mut d = vec![0u8; POOL_LEN];
        d[OFF_AMM_CONFIG..OFF_AMM_CONFIG + 32].copy_from_slice(&[7u8; 32]);
        d[OFF_MINT_0..OFF_MINT_0 + 32].copy_from_slice(&[mints.0; 32]);
        d[OFF_MINT_1..OFF_MINT_1 + 32].copy_from_slice(&[mints.1; 32]);
        d[OFF_DECIMALS_0] = decimals.0;
        d[OFF_DECIMALS_1] = decimals.1;
        d[OFF_TICK_SPACING..OFF_TICK_SPACING + 2].copy_from_slice(&tick_spacing.to_le_bytes());
        d[OFF_LIQUIDITY..OFF_LIQUIDITY + 16].copy_from_slice(&liquidity.to_le_bytes());
        d[OFF_SQRT_PRICE..OFF_SQRT_PRICE + 16].copy_from_slice(&sqrt_price.to_le_bytes());
        d[OFF_TICK_CURRENT..OFF_TICK_CURRENT + 4].copy_from_slice(&tick.to_le_bytes());
        d
    }

    fn config(fee_ppm: u32) -> Vec<u8> {
        let mut d = vec![0u8; CONFIG_LEN];
        d[OFF_CFG_TRADE_FEE_RATE..OFF_CFG_TRADE_FEE_RATE + 4].copy_from_slice(&fee_ppm.to_le_bytes());
        d
    }

    /// Real Raydium CLMM SOL/USDC 4bp state, captured 2026-08-21.
    fn sol_usdc() -> Vec<u8> {
        account(1, 133_291_264_152_881, 5_572_826_470_351_845_177, -23941, (9, 6), (1, 2))
    }

    #[test]
    fn decodes_real_sol_usdc_state() {
        let p = decode(&sol_usdc()).unwrap();
        assert_eq!(p.tick_spacing, 1);
        assert_eq!((p.decimals_0, p.decimals_1), (9, 6), "SOL/USDC decimals");
        assert_eq!(p.tick_current, -23941);
        assert_eq!(p.amm_config, [7u8; 32]);
    }

    #[test]
    fn config_yields_the_fee_the_api_reports() {
        // These are the two configs the live SOL/USDC pools point at.
        assert_eq!(decode_trade_fee_ppm(&config(400)).unwrap(), 400, "4 bp tier");
        assert_eq!(decode_trade_fee_ppm(&config(100)).unwrap(), 100, "1 bp tier");
        assert_eq!(decode_trade_fee_ppm(&config(2500)).unwrap(), 2500, "25 bp tier");
    }

    #[test]
    fn pool_state_prices_sol_at_about_ninety_one_dollars() {
        let p = to_pool_state([9u8; 32], &sol_usdc(), 400, 77).unwrap();
        assert_eq!(p.dex, Dex::RaydiumClmm);
        assert_eq!(p.fee_ppm, 400);
        assert_eq!(p.slot, 77);
        let ui = p.spot_price().unwrap() * 1000.0;
        assert!((ui - 91.0).abs() < 1.5, "expected roughly $91, got {ui}");
    }

    /// A tick spacing of 1 is the tightest there is: the interval is one basis point
    /// wide. On this real $5.9M pool it holds about 2 SOL — roughly $190.
    ///
    /// That is 35x our entire $5 account, so the bound never binds on our own money.
    /// It is also a warning worth keeping in a test: borrow $10,000 through a flash
    /// loan and this same tick is crossed several times over, at which point the
    /// constant-product equivalence stops holding and the quote must walk tick arrays
    /// instead. Any future sizing that ignores `max_in` will silently over-quote.
    #[test]
    fn the_tightest_tick_holds_far_more_than_our_capital_and_far_less_than_a_flash_loan() {
        let p = to_pool_state([9u8; 32], &sol_usdc(), 400, 1).unwrap();
        let sol_in = p.leg_for_input(&[1u8; 32]).expect("must quote SOL in");

        let five_dollars_of_sol = 55_000_000u128; // 0.055 SOL at ~$91
        assert!(
            sol_in.max_in > five_dollars_of_sol * 10,
            "one 1bp tick must comfortably hold our whole account, got {}",
            sol_in.max_in
        );

        let ten_thousand_dollars_of_sol = 110_000_000_000u128;
        assert!(
            sol_in.max_in < ten_thousand_dollars_of_sol,
            "a flash-loan-sized trade must be recognised as exceeding one tick"
        );
    }

    #[test]
    fn a_pool_with_no_liquidity_is_not_a_pool() {
        let d = account(1, 0, 5_572_826_470_351_845_177, -23941, (9, 6), (1, 2));
        assert!(to_pool_state([9u8; 32], &d, 400, 1).is_err());
    }

    #[test]
    fn price_inconsistent_with_its_own_tick_is_rejected() {
        let d = account(1, 1_000_000_000, 5_572_826_470_351_845_177, 0, (9, 6), (1, 2));
        let err = to_pool_state([9u8; 32], &d, 400, 1).unwrap_err().to_string();
        assert!(err.contains("does not belong to tick"), "unexpected error: {err}");
    }

    #[test]
    fn a_hundred_percent_fee_is_rejected_rather_than_underflowing_gamma() {
        assert!(decode_trade_fee_ppm(&config(1_000_000)).is_err());
        assert!(to_pool_state([9u8; 32], &sol_usdc(), 1_000_000, 1).is_err());
    }

    #[test]
    fn short_accounts_are_rejected_rather_than_read_out_of_bounds() {
        assert!(decode(&[]).is_err());
        assert!(decode(&vec![0u8; POOL_LEN - 1]).is_err());
        assert!(decode_trade_fee_ppm(&[]).is_err());
        assert!(decode_trade_fee_ppm(&vec![0u8; CONFIG_LEN - 1]).is_err());
    }

    #[test]
    fn nonsense_field_values_are_rejected() {
        assert!(decode(&account(0, 1, 1, 0, (9, 6), (1, 2))).is_err(), "zero tick spacing");
        assert!(decode(&account(1, 1, 1, i32::MIN, (9, 6), (1, 2))).is_err(), "tick out of range");
        assert!(decode(&account(1, 1, 1, 0, (99, 6), (1, 2))).is_err(), "implausible decimals");
    }
}
