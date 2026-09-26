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
//!
//! # Adaptive-fee pools
//!
//! Some pools charge a volatility surcharge on top of their static `fee_rate`, held in a
//! per-pool `Oracle` account (PDA `["oracle", whirlpool]`). The program applies it
//! whenever that account is initialised; the pool's `tick_spacing_seed` holding a
//! fee-tier index instead of the spacing is only the marker such pools are created
//! with. The surcharge is `ceil(control · (accumulator · tick_group_size)² / 10¹³)`
//! ppm, where the accumulator counts tick groups moved away from a reference that decays
//! with time — so the fee a swap pays depends on the block time it lands at.
//! [`Oracle::max_fee_rate`] prices the worst the program could charge anywhere in a
//! window of block times, and [`to_pool_state_adaptive`] bounds the quote to the current
//! tick group, the range over which the program holds that fee fixed. Ported from
//! `programs/whirlpool/src/state/oracle.rs` and `manager/fee_rate_manager.rs` of
//! orca-so/whirlpools, and checked against the program by simulation.

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
/// `token_vault_a` sits immediately after `token_mint_a`, and `token_vault_b` after
/// `token_mint_b`. Needed to build a swap, not to price one.
const OFF_VAULT_A: usize = 133;
const OFF_MINT_B: usize = 181;
const OFF_VAULT_B: usize = 213;

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
    /// The pool's own token accounts. Not used for pricing — a swap instruction needs
    /// them, and re-fetching the account at signing time would be a second round trip
    /// against state that has already been read once.
    pub vault_a: Pubkey32,
    pub vault_b: Pubkey32,
    /// The fee-tier index in `tick_spacing_seed` differs from the spacing: the pool was
    /// created with an adaptive fee, and its oracle holds the rest of it.
    pub adaptive: bool,
}

/// Decode a Whirlpool account.
///
/// An adaptive-fee pool decodes, flagged [`Whirlpool::adaptive`]: building a swap needs
/// its accounts like any other. Pricing one needs its oracle — see [`to_pool_state`].
pub fn decode(data: &[u8]) -> Result<Whirlpool> {
    ensure!(
        data.len() >= WHIRLPOOL_LEN,
        "whirlpool account too short: {} bytes, need {WHIRLPOOL_LEN}",
        data.len()
    );

    let tick_spacing = u16_at(data, OFF_TICK_SPACING);
    let seed = u16_at(data, OFF_TICK_SPACING_SEED);
    ensure!(tick_spacing > 0, "whirlpool with zero tick spacing");

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
        vault_a: pubkey_at(data, OFF_VAULT_A),
        vault_b: pubkey_at(data, OFF_VAULT_B),
        adaptive: seed != tick_spacing,
    })
}

/// Decode straight into a [`PoolState`], resolving the tick interval the quote is
/// valid inside.
///
/// Returns an error when the pool has no liquidity at the current price, or when the
/// price sits outside its own tick's bounds — both mean there is nothing quotable
/// here, and both are normal for thin pools rather than signs of a decode error.
///
/// Refuses a pool marked adaptive: its static `fee_rate` is only part of what it charges,
/// and quoting it alone would understate cost. Such a pool is priced by
/// [`to_pool_state_adaptive`] with its oracle.
pub fn to_pool_state(address: Pubkey32, data: &[u8], slot: u64) -> Result<PoolState> {
    let w = decode(data)?;
    ensure!(
        !w.adaptive,
        "adaptive-fee whirlpool: its true fee needs its oracle account, which this price was \
         not given, and quoting the static part alone would understate cost"
    );
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

/// Serialised length of the `Oracle` account: discriminator, whirlpool, trade-enable
/// timestamp, 34 bytes of constants, 44 of variables, 128 reserved.
pub const ORACLE_LEN: usize = 254;

const OFF_ORACLE_WHIRLPOOL: usize = 8;
const OFF_TRADE_ENABLE: usize = 40;
const OFF_FILTER_PERIOD: usize = 48;
const OFF_DECAY_PERIOD: usize = 50;
const OFF_REDUCTION_FACTOR: usize = 52;
const OFF_CONTROL_FACTOR: usize = 54;
const OFF_MAX_VOLATILITY: usize = 58;
const OFF_TICK_GROUP_SIZE: usize = 62;
const OFF_LAST_REFERENCE_UPDATE: usize = 82;
const OFF_LAST_MAJOR_SWAP: usize = 90;
const OFF_VOLATILITY_REFERENCE: usize = 98;
const OFF_TICK_GROUP_REFERENCE: usize = 102;
const OFF_VOLATILITY_ACCUMULATOR: usize = 106;

/// The program's constants, as `oracle.rs` and `fee_rate_manager.rs` define them.
const VOLATILITY_ACCUMULATOR_SCALE_FACTOR: u64 = 10_000;
const REDUCTION_FACTOR_DENOMINATOR: u64 = 10_000;
const ADAPTIVE_FEE_CONTROL_FACTOR_DENOMINATOR: u128 = 100_000;
/// A reference older than this, in seconds, is reset outright.
const MAX_REFERENCE_AGE: u64 = 3_600;
/// No total fee exceeds 10%.
pub const FEE_RATE_HARD_LIMIT: u32 = 100_000;

fn u32_at(d: &[u8], o: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&d[o..o + 4]);
    u32::from_le_bytes(b)
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[o..o + 8]);
    u64::from_le_bytes(b)
}

/// An adaptive-fee pool's oracle: the fee's constants and its moving state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Oracle {
    pub whirlpool: Pubkey32,
    /// No swap is accepted before this block time.
    pub trade_enable_timestamp: u64,
    /// Seconds after the last reference update within which a swap leaves the reference
    /// alone ("high frequency").
    pub filter_period: u16,
    /// Seconds after which the reference resets to zero instead of decaying.
    pub decay_period: u16,
    /// Share of the accumulator kept as the new reference, over 10,000.
    pub reduction_factor: u16,
    /// Scales the squared accumulator into a fee, over 100,000.
    pub adaptive_fee_control_factor: u32,
    pub max_volatility_accumulator: u32,
    /// A tick group is `floor(tick / tick_group_size)`.
    pub tick_group_size: u16,
    pub last_reference_update_timestamp: u64,
    pub last_major_swap_timestamp: u64,
    pub volatility_reference: u32,
    pub tick_group_index_reference: i32,
    pub volatility_accumulator: u32,
}

/// Decode an `Oracle` account.
///
/// # Errors
/// If it is too short, or its constants are ones the program would never have accepted
/// — which means the bytes are not an oracle.
pub fn decode_oracle(data: &[u8]) -> Result<Oracle> {
    ensure!(data.len() >= ORACLE_LEN, "oracle account too short: {} bytes, need {ORACLE_LEN}", data.len());
    let o = Oracle {
        whirlpool: pubkey_at(data, OFF_ORACLE_WHIRLPOOL),
        trade_enable_timestamp: u64_at(data, OFF_TRADE_ENABLE),
        filter_period: u16_at(data, OFF_FILTER_PERIOD),
        decay_period: u16_at(data, OFF_DECAY_PERIOD),
        reduction_factor: u16_at(data, OFF_REDUCTION_FACTOR),
        adaptive_fee_control_factor: u32_at(data, OFF_CONTROL_FACTOR),
        max_volatility_accumulator: u32_at(data, OFF_MAX_VOLATILITY),
        tick_group_size: u16_at(data, OFF_TICK_GROUP_SIZE),
        last_reference_update_timestamp: u64_at(data, OFF_LAST_REFERENCE_UPDATE),
        last_major_swap_timestamp: u64_at(data, OFF_LAST_MAJOR_SWAP),
        volatility_reference: u32_at(data, OFF_VOLATILITY_REFERENCE),
        tick_group_index_reference: i32_at(data, OFF_TICK_GROUP_REFERENCE),
        volatility_accumulator: u32_at(data, OFF_VOLATILITY_ACCUMULATOR),
    };
    // The program's own validation of the constants: anything else is not an oracle.
    ensure!(
        o.filter_period > 0 && o.decay_period > o.filter_period,
        "oracle periods {} / {} are not ones the program accepts",
        o.filter_period,
        o.decay_period
    );
    ensure!(
        u64::from(o.reduction_factor) < REDUCTION_FACTOR_DENOMINATOR
            && u128::from(o.adaptive_fee_control_factor) < ADAPTIVE_FEE_CONTROL_FACTOR_DENOMINATOR
            && o.tick_group_size > 0
            && u64::from(o.max_volatility_accumulator) * u64::from(o.tick_group_size) <= u64::from(u32::MAX),
        "oracle constants are not ones the program accepts"
    );
    Ok(o)
}

impl Oracle {
    /// The tick group a tick is in.
    #[must_use]
    pub fn tick_group(&self, tick: i32) -> i32 {
        tick.div_euclid(i32::from(self.tick_group_size))
    }

    /// The volatility accumulator the first step of a swap starting in `tick_group` at
    /// block time `t` is priced with: the program's `update_reference` then
    /// `update_volatility_accumulator`, exactly. `None` where the program refuses the
    /// swap, for a block time before the oracle's own last update.
    #[must_use]
    pub fn accumulator_at(&self, tick_group: i32, t: u64) -> Option<u32> {
        let max_ts = self.last_reference_update_timestamp.max(self.last_major_swap_timestamp);
        if t < max_ts {
            return None;
        }
        let (mut reference, mut index) = (self.volatility_reference, self.tick_group_index_reference);
        if t - self.last_reference_update_timestamp > MAX_REFERENCE_AGE {
            reference = 0;
            index = tick_group;
        } else {
            let elapsed = t - max_ts;
            if elapsed < u64::from(self.filter_period) {
                // A high-frequency swap: the reference stands.
            } else if elapsed < u64::from(self.decay_period) {
                index = tick_group;
                reference = (u64::from(self.volatility_accumulator) * u64::from(self.reduction_factor)
                    / REDUCTION_FACTOR_DENOMINATOR) as u32;
            } else {
                index = tick_group;
                reference = 0;
            }
        }
        let delta = (i64::from(index) - i64::from(tick_group)).unsigned_abs();
        let acc = u64::from(reference).saturating_add(delta.saturating_mul(VOLATILITY_ACCUMULATOR_SCALE_FACTOR));
        Some(acc.min(u64::from(self.max_volatility_accumulator)) as u32)
    }

    /// The surcharge, in ppm, an accumulator of `acc` adds to the static fee.
    #[must_use]
    pub fn adaptive_fee_rate(&self, acc: u32) -> u32 {
        let crossed = u128::from(acc) * u128::from(self.tick_group_size);
        let squared = crossed * crossed;
        let denominator = ADAPTIVE_FEE_CONTROL_FACTOR_DENOMINATOR
            * u128::from(VOLATILITY_ACCUMULATOR_SCALE_FACTOR)
            * u128::from(VOLATILITY_ACCUMULATOR_SCALE_FACTOR);
        let fee = (u128::from(self.adaptive_fee_control_factor) * squared).div_ceil(denominator);
        u32::try_from(fee.min(u128::from(FEE_RATE_HARD_LIMIT))).unwrap_or(FEE_RATE_HARD_LIMIT)
    }

    /// The most the program could charge, static fee included, on the first tick group
    /// of a swap starting at `tick_current` and landing at any block time in
    /// `[t_lo, t_hi]`. `None` if no time in that window can swap at all.
    ///
    /// The accumulator is constant between the instants its branch changes — the last
    /// update, the end of the filter and decay windows, the reference's maximum age — so
    /// it is evaluated at the window's start and at each of those inside it, and the
    /// largest taken. Largest, not latest: decaying can *raise* it, when the price has
    /// come back to the reference group since the last swap.
    #[must_use]
    pub fn max_fee_rate(&self, static_fee_ppm: u32, tick_current: i32, t_lo: u64, t_hi: u64) -> Option<u32> {
        let group = self.tick_group(tick_current);
        let max_ts = self.last_reference_update_timestamp.max(self.last_major_swap_timestamp);
        let breaks = [
            max_ts,
            max_ts.saturating_add(u64::from(self.filter_period)),
            max_ts.saturating_add(u64::from(self.decay_period)),
            self.last_reference_update_timestamp.saturating_add(MAX_REFERENCE_AGE + 1),
        ];
        let acc = std::iter::once(t_lo)
            .chain(breaks.into_iter().filter(|b| (t_lo..=t_hi).contains(b)))
            .filter_map(|t| self.accumulator_at(group, t))
            .max()?;
        Some(static_fee_ppm.saturating_add(self.adaptive_fee_rate(acc)).min(FEE_RATE_HARD_LIMIT))
    }
}

/// Price an adaptive-fee pool with its oracle, for a swap landing at a block time in
/// `[t_lo, t_hi]` (unix seconds).
///
/// The fee is [`Oracle::max_fee_rate`] over that window, and the quote is bounded to the
/// current tick group rather than the tick-spacing interval: the program re-prices its
/// fee at every group boundary it crosses, upward as it moves away from the reference,
/// so only the first group's fee is known here. A group never spans more than the
/// spacing interval, whose liquidity is constant.
///
/// # Errors
/// If the oracle belongs to another pool, trading is not yet enabled, the pool has no
/// liquidity, or its price and tick disagree.
pub fn to_pool_state_adaptive(
    address: Pubkey32,
    data: &[u8],
    oracle: &Oracle,
    t_lo: u64,
    t_hi: u64,
    slot: u64,
) -> Result<PoolState> {
    let w = decode(data)?;
    ensure!(oracle.whirlpool == address, "this oracle belongs to another whirlpool");
    ensure!(
        oracle.trade_enable_timestamp <= t_lo,
        "whirlpool trading opens at {}, after this swap could land",
        oracle.trade_enable_timestamp
    );
    ensure!(w.liquidity > 0, "whirlpool has no liquidity at the current price");
    ensure!(
        clmm::price_belongs_to_tick(w.sqrt_price_x64, w.tick_current, w.tick_spacing),
        "sqrt price {} does not belong to tick {} at spacing {}",
        w.sqrt_price_x64,
        w.tick_current,
        w.tick_spacing
    );
    ensure!(
        w.tick_spacing % oracle.tick_group_size == 0,
        "tick group size {} does not divide spacing {}",
        oracle.tick_group_size,
        w.tick_spacing
    );
    let fee_ppm = oracle
        .max_fee_rate(w.fee_rate_ppm, w.tick_current, t_lo, t_hi)
        .ok_or_else(|| anyhow::anyhow!("no block time in the window can swap this pool"))?;
    let (sqrt_lo_x64, sqrt_hi_x64) = clmm::bounds(w.tick_current, oracle.tick_group_size)
        .ok_or_else(|| anyhow::anyhow!("could not bound tick group of {}", w.tick_current))?;
    Ok(PoolState {
        id: PoolId(address),
        dex: Dex::OrcaWhirlpool,
        mint_a: w.mint_a,
        mint_b: w.mint_b,
        math: PoolMath::Concentrated { liquidity: w.liquidity, sqrt_price_x64: w.sqrt_price_x64, sqrt_lo_x64, sqrt_hi_x64 },
        fee_ppm,
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
    /// the one in this account, so it is never priced without its oracle.
    #[test]
    fn adaptive_fee_pools_decode_but_are_not_priced_without_their_oracle() {
        // Same pool, but the seed field holds a fee-tier index instead of the spacing.
        let d = account(4, 1024, 400, 758_634_162_063_829, 5_569_625_019_338_410_820, -23953, 1, 2);
        assert!(decode(&d).unwrap().adaptive, "building a swap still needs its accounts");
        let err = to_pool_state([9u8; 32], &d, 1).unwrap_err().to_string();
        assert!(err.contains("adaptive-fee"), "unexpected error: {err}");
        assert!(!decode(&sol_usdc()).unwrap().adaptive);
    }

    fn oracle() -> Oracle {
        Oracle {
            whirlpool: [9u8; 32],
            trade_enable_timestamp: 0,
            filter_period: 30,
            decay_period: 600,
            reduction_factor: 5_000,
            adaptive_fee_control_factor: 4_000,
            max_volatility_accumulator: 350_000,
            tick_group_size: 4,
            last_reference_update_timestamp: 1_000,
            last_major_swap_timestamp: 1_000,
            volatility_reference: 20_000,
            tick_group_index_reference: -5_990,
            volatility_accumulator: 60_000,
        }
    }

    #[test]
    fn the_oracle_layout_round_trips() {
        let o = oracle();
        let mut d = vec![0u8; ORACLE_LEN];
        d[OFF_ORACLE_WHIRLPOOL..OFF_ORACLE_WHIRLPOOL + 32].copy_from_slice(&o.whirlpool);
        d[OFF_FILTER_PERIOD..OFF_FILTER_PERIOD + 2].copy_from_slice(&o.filter_period.to_le_bytes());
        d[OFF_DECAY_PERIOD..OFF_DECAY_PERIOD + 2].copy_from_slice(&o.decay_period.to_le_bytes());
        d[OFF_REDUCTION_FACTOR..OFF_REDUCTION_FACTOR + 2].copy_from_slice(&o.reduction_factor.to_le_bytes());
        d[OFF_CONTROL_FACTOR..OFF_CONTROL_FACTOR + 4].copy_from_slice(&o.adaptive_fee_control_factor.to_le_bytes());
        d[OFF_MAX_VOLATILITY..OFF_MAX_VOLATILITY + 4].copy_from_slice(&o.max_volatility_accumulator.to_le_bytes());
        d[OFF_TICK_GROUP_SIZE..OFF_TICK_GROUP_SIZE + 2].copy_from_slice(&o.tick_group_size.to_le_bytes());
        d[OFF_LAST_REFERENCE_UPDATE..OFF_LAST_REFERENCE_UPDATE + 8]
            .copy_from_slice(&o.last_reference_update_timestamp.to_le_bytes());
        d[OFF_LAST_MAJOR_SWAP..OFF_LAST_MAJOR_SWAP + 8].copy_from_slice(&o.last_major_swap_timestamp.to_le_bytes());
        d[OFF_VOLATILITY_REFERENCE..OFF_VOLATILITY_REFERENCE + 4]
            .copy_from_slice(&o.volatility_reference.to_le_bytes());
        d[OFF_TICK_GROUP_REFERENCE..OFF_TICK_GROUP_REFERENCE + 4]
            .copy_from_slice(&o.tick_group_index_reference.to_le_bytes());
        d[OFF_VOLATILITY_ACCUMULATOR..OFF_VOLATILITY_ACCUMULATOR + 4]
            .copy_from_slice(&o.volatility_accumulator.to_le_bytes());
        assert_eq!(decode_oracle(&d).unwrap(), o);
        assert!(decode_oracle(&d[..ORACLE_LEN - 1]).is_err());
        // Constants the program would never have accepted are not an oracle.
        d[OFF_DECAY_PERIOD..OFF_DECAY_PERIOD + 2].copy_from_slice(&10u16.to_le_bytes());
        assert!(decode_oracle(&d).is_err(), "decay no longer than the filter");
    }

    /// Each branch of the program's reference update, at the tick group two away from
    /// the reference.
    #[test]
    fn the_accumulator_follows_every_branch_of_the_reference_update() {
        let o = oracle();
        let g = -5_988;
        // Inside the filter window the reference stands: 20,000 + 2 groups.
        assert_eq!(o.accumulator_at(g, 1_010), Some(40_000));
        // Past it, the reference becomes half the last accumulator, centred here.
        assert_eq!(o.accumulator_at(g, 1_100), Some(30_000));
        // Past the decay window it resets.
        assert_eq!(o.accumulator_at(g, 1_700), Some(0));
        // And a block time before the last update is one the program refuses.
        assert_eq!(o.accumulator_at(g, 999), None);
        // Far from the reference it is capped.
        assert_eq!(o.accumulator_at(g + 100, 1_010), Some(350_000));
    }

    /// ceil(4,000 · (40,000 · 4)² / 10¹³) = ceil(10.24) = 11 ppm, and the hard limit holds.
    #[test]
    fn the_surcharge_matches_the_programs_formula() {
        let mut o = oracle();
        assert_eq!(o.adaptive_fee_rate(40_000), 11);
        assert_eq!(o.adaptive_fee_rate(350_000), 784, "4,000 · 1,400,000² / 10¹³");
        assert_eq!(o.adaptive_fee_rate(0), 0);
        assert_eq!(o.adaptive_fee_rate(1), 1, "any volatility rounds up to a ppm");
        o.adaptive_fee_control_factor = 99_999;
        o.tick_group_size = 64;
        assert_eq!(o.adaptive_fee_rate(1_000_000), FEE_RATE_HARD_LIMIT);
    }

    /// Back in the reference group, the high-frequency branch charges only the old
    /// reference while the decay branch charges half the last accumulator: the window's
    /// worst is the later, larger one.
    #[test]
    fn the_worst_fee_in_a_window_can_come_from_decay() {
        let o = oracle();
        let tick = -5_990 * 4; // the reference group itself
        let now_only = o.max_fee_rate(400, tick, 1_010, 1_010).unwrap();
        assert_eq!(now_only, 400 + o.adaptive_fee_rate(20_000));
        let spanning = o.max_fee_rate(400, tick, 1_010, 1_040).unwrap();
        assert_eq!(spanning, 400 + o.adaptive_fee_rate(30_000), "decay starts at 1,030");
        assert!(spanning > now_only);
    }

    #[test]
    fn an_adaptive_pool_is_bounded_to_its_tick_group_and_pays_the_surcharge() {
        let mut o = oracle();
        o.tick_group_size = 1;
        let d = account(4, 1024, 400, 758_634_162_063_829, 5_569_625_019_338_410_820, -23953, 1, 2);
        let plain = to_pool_state([9u8; 32], &account(4, 4, 400, 758_634_162_063_829, 5_569_625_019_338_410_820, -23953, 1, 2), 1).unwrap();
        let p = to_pool_state_adaptive([9u8; 32], &d, &o, 1_010, 1_020, 1).unwrap();
        assert!(p.fee_ppm > 400, "the surcharge is charged: {}", p.fee_ppm);
        let (a, b) = (p.leg_for_input(&[1u8; 32]).unwrap(), plain.leg_for_input(&[1u8; 32]).unwrap());
        assert!(a.max_in < b.max_in, "one tick of room, not the spacing's four");
        // Another pool's oracle, or trading not yet open, prices nothing.
        assert!(to_pool_state_adaptive([8u8; 32], &d, &o, 1_010, 1_020, 1).is_err());
        o.trade_enable_timestamp = 2_000;
        assert!(to_pool_state_adaptive([9u8; 32], &d, &o, 1_010, 1_020, 1).is_err());
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

    /// The vault offsets cannot be proved here, and this test does not pretend to.
    ///
    /// Every other field in this decoder is corroborated from outside: `--verify`
    /// prices the pool against an independent router, and a wrong `mint`, `liquidity`,
    /// `sqrt_price` or `tick` would not survive that. The vaults are not priced, so
    /// nothing in the quote path would notice them being wrong — the outside opinion
    /// that catches them is a failed `simulateTransaction`, which happens before
    /// anything is signed.
    ///
    /// What is checkable here is that they do not collide with the fields that *are*
    /// corroborated. An off-by-one-field slip is the realistic mistake, and it would
    /// land a vault squarely on a mint.
    #[test]
    fn the_vault_offsets_do_not_overlap_any_verified_field() {
        const PUBKEY: usize = 32;
        let spans = [
            ("mint_a", OFF_MINT_A, PUBKEY),
            ("vault_a", OFF_VAULT_A, PUBKEY),
            ("mint_b", OFF_MINT_B, PUBKEY),
            ("vault_b", OFF_VAULT_B, PUBKEY),
            ("liquidity", OFF_LIQUIDITY, 16),
            ("sqrt_price", OFF_SQRT_PRICE, 16),
            ("tick_current", OFF_TICK_CURRENT, 4),
        ];
        for (i, (an, ao, al)) in spans.iter().enumerate() {
            assert!(ao + al <= WHIRLPOOL_LEN, "{an} runs past the account");
            for (bn, bo, bl) in spans.iter().skip(i + 1) {
                let overlap = *ao < bo + bl && *bo < ao + al;
                assert!(!overlap, "{an} at {ao} overlaps {bn} at {bo}");
            }
        }
        // And the two vaults must be the two distinct 32-byte spans that follow each
        // mint, which is the layout's actual shape.
        assert_eq!(OFF_VAULT_A, OFF_MINT_A + PUBKEY);
        assert_eq!(OFF_VAULT_B, OFF_MINT_B + PUBKEY);
    }

    #[test]
    fn a_decoded_pool_reports_the_two_vaults_it_was_given() {
        let mut d = account(64, 64, 400, 1_000_000_000, 5_569_625_019_338_410_820, -23953, 1, 2);
        d[OFF_VAULT_A] = 7;
        d[OFF_VAULT_B] = 9;
        let w = decode(&d).unwrap();
        assert_eq!(w.vault_a[0], 7);
        assert_eq!(w.vault_b[0], 9);
        assert_ne!(w.vault_a, w.vault_b);
    }
}
