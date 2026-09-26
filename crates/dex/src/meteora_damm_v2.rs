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

/// DAMM v2's volatility fee over 10^9, at the accumulator the pool last recorded plus two
/// steps of headroom for the swap's own movement.
///
/// `(volatility x bin_step)^2 x variable_fee_control / 10^11`, rounded up — measured on
/// pool 4MhsDW... on 2026-09-26, where the state gave 257.93 ppm and the program charged
/// 257.95. The accumulator is the one stored at the pool's last swap and so already
/// includes the distance from its reference at today's price; decay can only lower it,
/// which is why it is not modelled. The two extra steps (20,000) cover the price moving
/// during our own swap.
#[must_use]
pub fn variable_fee_numerator(d: &DammV2DynamicFee) -> u64 {
    let vol = d
        .volatility_accumulator
        .saturating_add(20_000)
        .min(u128::from(d.max_volatility_accumulator).max(d.volatility_accumulator));
    let x = vol.saturating_mul(u128::from(d.bin_step));
    let num = x.saturating_mul(x).saturating_mul(u128::from(d.variable_fee_control));
    u64::try_from(num.div_ceil(100_000_000_000)).unwrap_or(u64::MAX)
}

/// Decode straight into a [`PoolState`].
///
/// Unlike the tick-based venues there is no interval to resolve: the pool's own
/// `[sqrt_min_price, sqrt_max_price]` *is* the range the quote is exact over.
///
/// # The fee, as the program charges it
///
/// Measured on live pools on 2026-09-26 with `cb-check-damm`, against the program's own
/// `EvtSwap2`:
///
/// - The rate is the base fee (the cliff numerator over 10^9) plus the volatility fee
///   ([`variable_fee_numerator`]); pools with a fee schedule are still refused.
/// - Collection mode 0 takes it from the **output** in both directions (4 bp and 10 bp
///   pools: the curve matched to the unit). Modes 1 and 2 take it in token B: from the
///   output spending A, from the input spending B. Mode 2 compounds part of the fee
///   back into the pool, which pays the swapper slightly *more* than this model — the
///   safe side.
/// - Token-2022 mints are accepted here; a mint with a transfer fee is screened out by
///   the caller, which has the mint account and this function does not.
pub fn to_pool_state(address: Pubkey32, data: &[u8], slot: u64) -> Result<PoolState> {
    let p = decode_layout(data)?;
    ensure!(p.pool_status == 0, "damm v2 pool has swaps disabled");
    ensure!(
        !p.has_fee_schedule,
        "damm v2 pool has a fee schedule: the fee moves over time and the cliff value \
         alone does not price it"
    );
    ensure!(p.collect_fee_mode <= 2, "damm v2 fee collection mode {} is not one measured", p.collect_fee_mode);
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

    let numerator = p
        .cliff_fee_numerator
        .saturating_add(p.dynamic_fee.as_ref().map_or(0, variable_fee_numerator));
    ensure!(numerator < 1_000_000_000, "damm v2 fee {numerator} over 1e9 is not a fee");
    let fee_ppm = u32::try_from(numerator.div_ceil(FEE_DENOM_PER_PPM)).expect("under a million");
    let fee_on_output_b_to_a = p.collect_fee_mode == 0;

    Ok(PoolState {
        id: PoolId(address),
        dex: Dex::MeteoraDammV2,
        mint_a: p.mint_a,
        mint_b: p.mint_b,
        math: PoolMath::ConcentratedFeeSide {
            liquidity: p.liquidity,
            sqrt_price_x64: p.sqrt_price_x64,
            sqrt_lo_x64: p.sqrt_min_price_x64,
            sqrt_hi_x64: p.sqrt_max_price_x64,
            fee_on_output_a_to_b: true,
            fee_on_output_b_to_a,
        },
        fee_ppm,
        slot,
    })
}

/// Every field of a DAMM v2 pool that pricing or a swap needs, decoded without judging it.
///
/// [`decode`] refuses any pool whose cost it cannot state; this reads them all, so the
/// swap encoder can name any pool's accounts and a verification tool can see the fee
/// features a pool actually uses. Offsets follow the program's IDL (read from chain on
/// 2026-09-26): `pool_fees` spans 8..168 and holds the base fee blob, the protocol,
/// referral and compounding shares, and the whole dynamic-fee state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DammV2Layout {
    pub mint_a: Pubkey32,
    pub mint_b: Pubkey32,
    pub vault_a: Pubkey32,
    pub vault_b: Pubkey32,
    /// Already unscaled: the stored field divided by 2^64.
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub sqrt_min_price_x64: u128,
    pub sqrt_max_price_x64: u128,
    /// The base fee's first word: the cliff numerator over 10^9.
    pub cliff_fee_numerator: u64,
    /// Whether the base fee blob holds anything past the cliff (a schedule).
    pub has_fee_schedule: bool,
    pub protocol_fee_percent: u8,
    pub referral_fee_percent: u8,
    pub compounding_fee_bps: u16,
    pub dynamic_fee: Option<DammV2DynamicFee>,
    pub activation_point: u64,
    pub activation_type: u8,
    pub pool_status: u8,
    pub token_a_flag: u8,
    pub token_b_flag: u8,
    pub collect_fee_mode: u8,
    pub fee_version: u8,
}

/// DAMM v2's volatility fee state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DammV2DynamicFee {
    pub max_volatility_accumulator: u32,
    pub variable_fee_control: u32,
    pub bin_step: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub last_update_timestamp: u64,
    pub bin_step_u128: u128,
    pub sqrt_price_reference: u128,
    pub volatility_accumulator: u128,
    pub volatility_reference: u128,
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().expect("4 bytes"))
}

/// Decode every field without refusing any pool. See [`DammV2Layout`].
///
/// # Errors
/// If the account is too short to be a pool.
pub fn decode_layout(data: &[u8]) -> Result<DammV2Layout> {
    ensure!(data.len() >= POOL_LEN, "damm v2 account too short: {} bytes, need {POOL_LEN}", data.len());
    let dynamic_fee = (data[OFF_DYNAMIC_FEE_INITIALIZED] != 0).then(|| DammV2DynamicFee {
        max_volatility_accumulator: u32_at(data, 64),
        variable_fee_control: u32_at(data, 68),
        bin_step: u16_at(data, 72),
        filter_period: u16_at(data, 74),
        decay_period: u16_at(data, 76),
        reduction_factor: u16_at(data, 78),
        last_update_timestamp: u64_at(data, 80),
        bin_step_u128: u128_at(data, 88),
        sqrt_price_reference: u128_at(data, 104),
        volatility_accumulator: u128_at(data, 120),
        volatility_reference: u128_at(data, 136),
    });
    Ok(DammV2Layout {
        mint_a: pubkey_at(data, OFF_MINT_A),
        mint_b: pubkey_at(data, OFF_MINT_B),
        vault_a: pubkey_at(data, 232),
        vault_b: pubkey_at(data, 264),
        liquidity: u128_at(data, OFF_LIQUIDITY) >> 64,
        sqrt_price_x64: u128_at(data, OFF_SQRT_PRICE),
        sqrt_min_price_x64: u128_at(data, OFF_SQRT_MIN_PRICE),
        sqrt_max_price_x64: u128_at(data, OFF_SQRT_MAX_PRICE),
        cliff_fee_numerator: u64_at(data, OFF_CLIFF_FEE),
        has_fee_schedule: data[OFF_BASE_FEE_REST..OFF_BASE_FEE_REST_END].iter().any(|&b| b != 0),
        protocol_fee_percent: data[48],
        referral_fee_percent: data[50],
        compounding_fee_bps: u16_at(data, 54),
        dynamic_fee,
        activation_point: u64_at(data, 472),
        activation_type: data[480],
        pool_status: data[OFF_POOL_STATUS],
        token_a_flag: data[OFF_TOKEN_A_FLAG],
        token_b_flag: data[OFF_TOKEN_B_FLAG],
        collect_fee_mode: data[484],
        fee_version: data[486],
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

    /// Mode 0 takes the fee from the output both ways; modes 1 and 2 from the output
    /// spending A and from the input spending B.
    #[test]
    fn the_fee_side_follows_the_collection_mode() {
        for (mode, b_to_a_on_output) in [(0u8, true), (1, false), (2, false)] {
            let mut d = account(Q64 * 1_000_000, 2 * Q64, Q64, 4 * Q64, 400_000);
            d[484] = mode;
            let s = to_pool_state([1; 32], &d, 7).unwrap();
            match s.math {
                PoolMath::ConcentratedFeeSide { fee_on_output_a_to_b, fee_on_output_b_to_a, .. } => {
                    assert!(fee_on_output_a_to_b, "mode {mode}: spending A always pays from the output");
                    assert_eq!(fee_on_output_b_to_a, b_to_a_on_output, "mode {mode}");
                }
                _ => panic!("wrong variant"),
            }
        }
        let mut d = account(Q64 * 1_000_000, 2 * Q64, Q64, 4 * Q64, 400_000);
        d[484] = 3;
        assert!(to_pool_state([1; 32], &d, 7).is_err(), "an unmeasured mode is refused");
    }

    /// Pool 4MhsDW... on 2026-09-26: bin step 1, control 5,739, accumulator 2,120,000.
    /// The program charged 257.95 ppm over its 6% base; the formula gives 257.93 at the
    /// stored accumulator, and the headroom adds a little.
    #[test]
    fn the_volatility_fee_is_the_one_the_program_charged() {
        let d = DammV2DynamicFee {
            max_volatility_accumulator: 14_460_000,
            variable_fee_control: 5_739,
            bin_step: 1,
            filter_period: 10,
            decay_period: 120,
            reduction_factor: 5_000,
            last_update_timestamp: 0,
            bin_step_u128: 0,
            sqrt_price_reference: 0,
            volatility_accumulator: 2_120_000,
            volatility_reference: 580_000,
        };
        let ppm = variable_fee_numerator(&d) as f64 / 1_000.0;
        assert!((257.9..263.0).contains(&ppm), "got {ppm} ppm");
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
            PoolMath::ConcentratedFeeSide { sqrt_lo_x64, sqrt_hi_x64, .. } => {
                assert_eq!(sqrt_lo_x64, Q64);
                assert_eq!(sqrt_hi_x64, 4 * Q64, "a 4x price move, not a tick");
            }
            _ => panic!("damm v2 is concentrated, with its fee on a measured side"),
        }
        assert_eq!(s.dex, Dex::MeteoraDammV2);
    }
}
