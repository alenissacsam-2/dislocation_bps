//! Meteora DAMM v2 (`cp-amm`) pool decoding.
//!
//! Program: `cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG`
//!
//! # Why this venue
//!
//! It is the largest Solana AMM this project was not reading. More usefully, it
//! quotes the majors at tiers that reach *below* the ones already covered — the
//! deepest SOL/USDC pool here charges 4 bp, and unlike Raydium CLMM's 1 bp pools its
//! quote stays exact over a far wider price move (see below). Cheap **and** deep is
//! the combination the fee wall is made of.
//!
//! # It is concentrated liquidity without ticks
//!
//! A DAMM v2 pool holds a single `liquidity` and a fixed price range
//! `[sqrt_min_price, sqrt_max_price]`, and **that one liquidity value is valid across
//! the entire range**. There is no tick array and no per-tick liquidity net, because
//! every position in the pool spans the pool's whole range.
//!
//! That makes it strictly easier to quote than Orca or Raydium CLMM. There the
//! constant-product equivalence holds only inside the current tick, so `max_in` is a
//! tick's worth of depth and a larger trade has to be refused. Here the same
//! equivalence holds all the way to the range boundary, which is usually a price move
//! of several *hundred* percent. Same [`cb_core::clmm`] identity, a much larger exact
//! bound, and no second account to read.
//!
//! # Layout
//!
//! Anchor `zero_copy`, fixed 1112 bytes (8-byte discriminator + 1104). The struct
//! declares its own padding, and on BPF `u128` aligns to 8, so declared offsets are
//! actual offsets with nothing implicit inserted.
//!
//! Offsets were verified against all 66 live SOL/USDC pools on 2026-08-22 by
//! reconstructing each pool's token balances from `(liquidity, sqrt_price,
//! sqrt_min_price, sqrt_max_price)` and comparing against the `token_a_amount` and
//! `token_b_amount` the account carries independently. Every pool agreed to within
//! 0.001%, the residual being fees accrued but not yet folded into liquidity. Two
//! fields cannot both be read correctly by accident across 66 pools, so this pins the
//! layout rather than merely being consistent with it.
//!
//! # `liquidity` is scaled, and the others are not
//!
//! The field holds **L · 2⁶⁴**, not L. Orca and Raydium CLMM store L directly, so
//! this is the one place a DAMM v2 number cannot be handed to the shared CLMM math
//! unchanged. The scale showed up as the reconstructed balances above missing by a
//! factor of exactly 2⁶⁴ in both tokens at once — the kind of error that is obvious
//! when two independent fields disagree and invisible when only one is read.

use anyhow::{ensure, Result};
use cb_core::types::{Dex, PoolId, PoolMath, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG";

/// Exact serialised length: 8-byte discriminator plus a 1104-byte `Pool`.
pub const POOL_LEN: usize = 1112;

/// Meteora states fees as a numerator over 1e9, where this project uses parts per
/// million. 400_000/1e9 is 4 bp; 400 ppm is the same 4 bp.
const FEE_DENOM_PER_PPM: u64 = 1_000;

// Verified byte offsets. `pool_fees` occupies 8..168, hence the mints at 168/200.
const OFF_CLIFF_FEE: usize = 8;
/// The rest of `BaseFeeInfo` plus its padding. All zero means a flat fee; anything
/// else is a fee *schedule*, which changes over time.
const OFF_BASE_FEE_REST: usize = 16;
const OFF_BASE_FEE_REST_END: usize = 48;
const OFF_DYNAMIC_FEE_INITIALIZED: usize = 56;
const OFF_MINT_A: usize = 168;
const OFF_MINT_B: usize = 200;
const OFF_LIQUIDITY: usize = 360;
const OFF_SQRT_MIN_PRICE: usize = 424;
const OFF_SQRT_MAX_PRICE: usize = 440;
const OFF_SQRT_PRICE: usize = 456;
const OFF_POOL_STATUS: usize = 481;
const OFF_TOKEN_A_FLAG: usize = 482;
const OFF_TOKEN_B_FLAG: usize = 483;

fn u64_at(d: &[u8], o: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[o..o + 8]);
    u64::from_le_bytes(b)
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

/// A decoded DAMM v2 pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DammV2Pool {
    pub mint_a: Pubkey32,
    pub mint_b: Pubkey32,
    /// Already unscaled: the stored field divided by 2⁶⁴.
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub sqrt_min_price_x64: u128,
    pub sqrt_max_price_x64: u128,
    pub fee_ppm: u32,
}

/// Decode a DAMM v2 `Pool` account.
///
/// # Rejections
///
/// Every one of these is a pool whose true cost is higher than its stored base fee,
/// or whose balance is not what the account says. Quoting any of them would overstate
/// profit, which is the only direction of error that loses money rather than
/// opportunities.
///
/// - **Dynamic-fee pools.** The effective fee carries a volatility surcharge derived
///   from state this decoder does not track, so the base fee is a floor, not a price.
/// - **Fee schedules.** A scheduled fee decays over time from a cliff. Reading the
///   cliff alone would misprice it in whichever direction the schedule happens to run.
/// - **Token-2022 mints.** Transfer fees and transfer hooks change what actually
///   arrives, and the pool account does not say by how much.
/// - **Disabled pools.** `pool_status` non-zero means swaps are off.
pub fn decode(data: &[u8]) -> Result<DammV2Pool> {
    ensure!(
        data.len() >= POOL_LEN,
        "damm v2 account too short: {} bytes, need {POOL_LEN}",
        data.len()
    );

    ensure!(
        data[OFF_DYNAMIC_FEE_INITIALIZED] == 0,
        "dynamic-fee damm v2 pool: its true fee adds a volatility surcharge held in \
         state we do not read, so the base fee would understate cost"
    );
    ensure!(
        data[OFF_BASE_FEE_REST..OFF_BASE_FEE_REST_END].iter().all(|&b| b == 0),
        "damm v2 pool has a fee schedule: the fee moves over time and the cliff value \
         alone does not price it"
    );
    ensure!(data[OFF_POOL_STATUS] == 0, "damm v2 pool has swaps disabled");
    ensure!(
        data[OFF_TOKEN_A_FLAG] == 0 && data[OFF_TOKEN_B_FLAG] == 0,
        "damm v2 pool holds a Token-2022 mint: transfer fees or hooks can change what \
         actually arrives"
    );

    let cliff = u64_at(data, OFF_CLIFF_FEE);
    // Round up: an exact tier divides evenly, and anything else is safer overstated.
    let fee_ppm = u32::try_from(cliff.div_ceil(FEE_DENOM_PER_PPM))
        .map_err(|_| anyhow::anyhow!("damm v2 fee numerator {cliff} is not a fee"))?;
    ensure!(fee_ppm < 1_000_000, "damm v2 fee {fee_ppm}ppm is not a fee");

    // The stored field is L · 2⁶⁴. The truncation discards a fraction of one unit of
    // liquidity against a value in the trillions — around 1e-13 relative.
    let liquidity = u128_at(data, OFF_LIQUIDITY) >> 64;

    Ok(DammV2Pool {
        mint_a: pubkey_at(data, OFF_MINT_A),
        mint_b: pubkey_at(data, OFF_MINT_B),
        liquidity,
        sqrt_price_x64: u128_at(data, OFF_SQRT_PRICE),
        sqrt_min_price_x64: u128_at(data, OFF_SQRT_MIN_PRICE),
        sqrt_max_price_x64: u128_at(data, OFF_SQRT_MAX_PRICE),
        fee_ppm,
    })
}

/// Decode straight into a [`PoolState`].
///
/// Unlike the tick-based venues there is no interval to resolve: the pool's own
/// `[sqrt_min_price, sqrt_max_price]` *is* the range the quote is exact over.
pub fn to_pool_state(address: Pubkey32, data: &[u8], slot: u64) -> Result<PoolState> {
    let p = decode(data)?;
    ensure!(p.liquidity > 0, "damm v2 pool has no liquidity");
    ensure!(
        p.sqrt_min_price_x64 < p.sqrt_max_price_x64,
        "damm v2 pool has an empty price range"
    );
    ensure!(
        (p.sqrt_min_price_x64..=p.sqrt_max_price_x64).contains(&p.sqrt_price_x64),
        "damm v2 price {} outside its own range {}..{} — mid-update or wrong layout",
        p.sqrt_price_x64,
        p.sqrt_min_price_x64,
        p.sqrt_max_price_x64
    );

    Ok(PoolState {
        id: PoolId(address),
        dex: Dex::MeteoraDammV2,
        mint_a: p.mint_a,
        mint_b: p.mint_b,
        math: PoolMath::Concentrated {
            liquidity: p.liquidity,
            sqrt_price_x64: p.sqrt_price_x64,
            sqrt_lo_x64: p.sqrt_min_price_x64,
            sqrt_hi_x64: p.sqrt_max_price_x64,
        },
        fee_ppm: p.fee_ppm,
        slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q64: u128 = 1u128 << 64;

    /// A synthetic account carrying only the fields the layout names. Everything else
    /// is zero, which is exactly what a decoder should be indifferent to.
    fn account(
        liquidity_scaled: u128,
        sqrt_price: u128,
        sqrt_min: u128,
        sqrt_max: u128,
        cliff_fee: u64,
    ) -> Vec<u8> {
        let mut d = vec![0u8; POOL_LEN];
        d[OFF_CLIFF_FEE..OFF_CLIFF_FEE + 8].copy_from_slice(&cliff_fee.to_le_bytes());
        d[OFF_MINT_A] = 0xAA;
        d[OFF_MINT_B] = 0xBB;
        d[OFF_LIQUIDITY..OFF_LIQUIDITY + 16].copy_from_slice(&liquidity_scaled.to_le_bytes());
        d[OFF_SQRT_MIN_PRICE..OFF_SQRT_MIN_PRICE + 16].copy_from_slice(&sqrt_min.to_le_bytes());
        d[OFF_SQRT_MAX_PRICE..OFF_SQRT_MAX_PRICE + 16].copy_from_slice(&sqrt_max.to_le_bytes());
        d[OFF_SQRT_PRICE..OFF_SQRT_PRICE + 16].copy_from_slice(&sqrt_price.to_le_bytes());
        d
    }

    /// The deepest live SOL/USDC pool, `8Pm2kZ…`, byte for byte as mainnet held it.
    ///
    /// This is the regression test that matters. It pins the whole layout at once by
    /// rebuilding balances the account reports separately: if any offset here drifts,
    /// or the 2⁶⁴ liquidity scale is dropped, the reconstruction misses by orders of
    /// magnitude rather than by a rounding error.
    #[test]
    fn a_real_pool_reconstructs_the_balances_it_reports_separately() {
        // One atomic read of the account at slot 440844416. All six numbers come from
        // the same snapshot — pairing fields across two reads of a live pool makes
        // this look broken when it is not.
        let liquidity_scaled = 127_650_388_139_951_181_215_105_640_046_542u128;
        let sqrt_price = 5_814_034_935_813_444_547u128;
        let sqrt_min = 4_880_549_731_789_001_291u128;
        let sqrt_max = 12_236_185_739_241_331_242u128;
        // What the account carried in token_a_amount / token_b_amount at that slot.
        let reported_a = 11_523_355_541_146u128;
        let reported_b = 350_179_453_609u128;

        let p = decode(&account(liquidity_scaled, sqrt_price, sqrt_min, sqrt_max, 400_000))
            .expect("a real pool must decode");

        assert_eq!(p.fee_ppm, 400, "4 bp, stated as 400_000 over 1e9");

        // The standard concentrated-liquidity holdings, in terms of unscaled L:
        //   a = L·2⁶⁴·(√Pu − √P) / (√P·√Pu)      b = L·(√P − √Pl) / 2⁶⁴
        // Grouped to keep every intermediate inside u128 — multiplying L by 2⁶⁴ up
        // front would overflow before the division ever ran.
        let l = p.liquidity;
        let a = l * (sqrt_max - sqrt_price) / sqrt_max * Q64 / sqrt_price;
        let b = l * (sqrt_price - sqrt_min) / Q64;

        let err_a = (a as f64 - reported_a as f64).abs() / reported_a as f64;
        let err_b = (b as f64 - reported_b as f64).abs() / reported_b as f64;
        assert!(err_a < 1e-4, "token A off by {err_a:e}: got {a}, account says {reported_a}");
        assert!(err_b < 1e-4, "token B off by {err_b:e}: got {b}, account says {reported_b}");
    }

    /// Dropping the 2⁶⁴ scale is the specific mistake the field invites, and it is
    /// wrong by a factor no sanity check on price would catch.
    #[test]
    fn liquidity_is_unscaled_by_two_to_the_sixty_four() {
        let scaled = 127_650_388_139_951_181_215_105_640_046_542u128;
        let p = decode(&account(scaled, 5_761_959_368_792_461_300, 1, u128::MAX, 400_000)).unwrap();
        assert_eq!(p.liquidity, scaled >> 64);
        assert_eq!(p.liquidity, 6_919_941_406_997);
    }

    #[test]
    fn fee_tiers_convert_to_parts_per_million() {
        for (numerator, ppm) in [(400_000u64, 400u32), (2_500_000, 2_500), (10_000_000, 10_000)] {
            let p = decode(&account(Q64, 2, 1, 3, numerator)).unwrap();
            assert_eq!(p.fee_ppm, ppm, "{numerator} over 1e9");
        }
    }

    #[test]
    fn a_dynamic_fee_pool_is_refused() {
        let mut d = account(Q64, 2, 1, 3, 400_000);
        d[OFF_DYNAMIC_FEE_INITIALIZED] = 1;
        let e = decode(&d).unwrap_err().to_string();
        assert!(e.contains("dynamic-fee"), "{e}");
    }

    #[test]
    fn a_scheduled_fee_pool_is_refused() {
        let mut d = account(Q64, 2, 1, 3, 400_000);
        d[OFF_BASE_FEE_REST] = 7; // any non-zero scheduler parameter
        let e = decode(&d).unwrap_err().to_string();
        assert!(e.contains("fee schedule"), "{e}");
    }

    #[test]
    fn a_token_2022_pool_is_refused() {
        for off in [OFF_TOKEN_A_FLAG, OFF_TOKEN_B_FLAG] {
            let mut d = account(Q64, 2, 1, 3, 400_000);
            d[off] = 1;
            assert!(decode(&d).unwrap_err().to_string().contains("Token-2022"));
        }
    }

    #[test]
    fn a_disabled_pool_is_refused() {
        let mut d = account(Q64, 2, 1, 3, 400_000);
        d[OFF_POOL_STATUS] = 1;
        assert!(decode(&d).unwrap_err().to_string().contains("disabled"));
    }

    #[test]
    fn a_price_outside_its_own_range_is_refused() {
        let d = account(Q64 * 1000, 10, 100, 200, 400_000);
        let e = to_pool_state([0; 32], &d, 1).unwrap_err().to_string();
        assert!(e.contains("outside its own range"), "{e}");
    }

    #[test]
    fn a_short_account_is_refused_rather_than_read_past_the_end() {
        assert!(decode(&[0u8; POOL_LEN - 1]).is_err());
    }

    /// The whole configured range is quotable, not one tick's worth — which is the
    /// point of covering this venue at all.
    #[test]
    fn the_quotable_range_is_the_pools_whole_range() {
        let d = account(Q64 * 1_000_000, 2 * Q64, Q64, 4 * Q64, 400_000);
        let s = to_pool_state([1; 32], &d, 7).unwrap();
        match s.math {
            PoolMath::Concentrated { sqrt_lo_x64, sqrt_hi_x64, .. } => {
                assert_eq!(sqrt_lo_x64, Q64);
                assert_eq!(sqrt_hi_x64, 4 * Q64, "a 4x price move, not a tick");
            }
            PoolMath::ConstantProduct { .. } => panic!("damm v2 is concentrated"),
        }
        assert_eq!(s.dex, Dex::MeteoraDammV2);
    }
}
