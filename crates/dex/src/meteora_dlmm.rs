//! Meteora DLMM (`LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo`).
//!
//! # Why this venue is worth the extra machinery
//!
//! Two independent censuses put it at the front of the queue. Fifteen consecutive
//! mainnet blocks, filtered to closed-loop arbitrage, produced thirteen winners and
//! Meteora DLMM appears in four of them. Asking an outside router for the best round
//! trip on twenty-two tokens, it holds the mispriced side more often than any other
//! venue this codebase can see.
//!
//! It is also the venue where being wrong is most expensive, because a DLMM's price
//! lives in the pool account and its **depth does not**. Liquidity sits in discrete
//! bins, seventy to an account, and a swap walks outward from the active bin until it
//! is filled. A quote that reads only the pool account knows the price at the margin
//! and nothing about how much is available there — and the error is one-directional: it
//! always reports more fill at a better rate than the pool will give.
//!
//! So nothing here quotes from the pool account alone. Every price this module produces
//! is backed by a bin account read in the same breath, and the two quoting paths are
//! deliberately different shapes:
//!
//! * [`quote_exact_in`] is the **reference**: it walks bins as the program does,
//!   charging the fee per bin and growing the volatility accumulator as it crosses.
//! * [`to_pool_state`] is what the router gets, and it is a *linearisation* — a rate and
//!   a size bound, each direction separately, constructed so the quote can only come out
//!   under the truth. See [`MAX_QUOTE_SPAN_BPS`] for the one judgement call in it, with
//!   the measured cost of that call written down.
//!
//! # How the layout was established
//!
//! Not from an IDL. Every `swap2` instruction on this program was dumped with its
//! accounts resolved, which names the pool's reserves, mints and oracle from the
//! program's own point of view; decoding the pool account then has to reproduce those
//! exact addresses at fixed offsets. For the bin arrays the corroboration is stronger
//! still: summing every bin of all 277 arrays belonging to the SOL/USDC pool gives
//! 2,139,761,987,043 of token X against a vault holding 2,173,686,834,020, and
//! 126,294,948,222 of token Y against a vault holding 130,266,697,861 — 98.4% and 96.9%,
//! the remainder being fees the vaults hold and the bins do not. A wrong offset does not
//! land within two percent of a vault balance twice.
//!
//! That census also settles the direction convention, which is the thing a swap gets
//! backwards: bins **above** the active id hold only token X, bins **below** hold only
//! token Y, and the active bin holds both. Spending X therefore consumes Y from the
//! active bin downwards.
//!
//! Two numeric fields cannot be checked that way, so they were checked against the
//! market: `bin_step` and `active_id` give a price of `(1 + bin_step/10_000)^active_id`,
//! and on the pool captured here that is $102.50 against $102.73 quoted by an outside
//! router at the same moment — a fifth of a percent apart, which is this pool's own bin
//! granularity.
//!
//! # The `swap2` account list, for the encoder
//!
//! Identical across every call, with positions 1 and 9 carrying the program's own id
//! when the optional account is absent:
//!
//! ```text
//!  [0] lb_pair                        [9] host_fee_in (or program id)
//!  [1] bin_array_bitmap_extension    [10] user (signer)
//!      (or program id)               [11] token_x_program
//!  [2] reserve_x                     [12] token_y_program
//!  [3] reserve_y                     [13] memo_program
//!  [4] user_token_in                 [14] event_authority
//!  [5] user_token_out                [15] program
//!  [6] token_x_mint                  [16..] bin arrays
//!  [7] token_y_mint
//!  [8] oracle
//! ```
//!
//! Positions 11 and 12 are the token programs **in the pool's own x/y order**, not
//! classic-then-2022 the way Raydium's `swap_v2` orders them. That is measured, not
//! read: a call was found whose token X is classic and whose token Y is Token-2022, and
//! it passes classic at 11 and Token-2022 at 12.
//!
//! Arguments are `amount_in: u64`, `min_amount_out: u64`, then a `RemainingAccountsInfo`
//! whose empty form is four zero bytes.

use anyhow::{ensure, Result};
use cb_core::clmm::{mul_shr_q64, shl_q64_div};
use cb_core::types::{Dex, PoolId, PoolMath, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

/// `LbPair` is a fixed-size account. A different length is a different account.
pub const LB_PAIR_LEN: usize = 904;

/// Bins in one `BinArray` account.
pub const BINS_PER_ARRAY: i32 = 70;
/// Serialised length of one bin.
pub const BIN_LEN: usize = 144;
/// Serialised length of a `BinArray` account: header plus seventy bins.
pub const BIN_ARRAY_LEN: usize = 10_136;

// --- LbPair offsets. See the module docs for what each was checked against. ---
const OFF_BASE_FACTOR: usize = 8;
const OFF_FILTER_PERIOD: usize = 10;
const OFF_DECAY_PERIOD: usize = 12;
const OFF_REDUCTION_FACTOR: usize = 14;
const OFF_VARIABLE_FEE_CONTROL: usize = 16;
const OFF_MAX_VOLATILITY_ACCUMULATOR: usize = 20;
const OFF_MIN_BIN_ID: usize = 24;
const OFF_MAX_BIN_ID: usize = 28;
const OFF_PROTOCOL_SHARE: usize = 32;
const OFF_BASE_FEE_POWER_FACTOR: usize = 34;
/// `VariableParameters` begins here. The accumulator is the **first** field of it, which
/// is worth stating because an earlier version of this file had it eight bytes late and
/// read `index_reference` instead — a number that looks like nothing at all once it is
/// reinterpreted as unsigned, which is how it was caught.
const OFF_VOLATILITY_ACCUMULATOR: usize = 40;
const OFF_VOLATILITY_REFERENCE: usize = 44;
const OFF_INDEX_REFERENCE: usize = 48;
const OFF_LAST_UPDATE_TIMESTAMP: usize = 56;
const OFF_ACTIVE_ID: usize = 76;
const OFF_BIN_STEP: usize = 80;
const OFF_STATUS: usize = 82;
const OFF_TOKEN_X_MINT: usize = 88;
const OFF_TOKEN_Y_MINT: usize = 120;
const OFF_RESERVE_X: usize = 152;
const OFF_RESERVE_Y: usize = 184;
const OFF_ORACLE: usize = 552;

// --- BinArray offsets. ---
const OFF_BA_INDEX: usize = 8;
const OFF_BA_LB_PAIR: usize = 24;
const OFF_BA_BINS: usize = 56;
// Within one bin.
const OFF_BIN_AMOUNT_X: usize = 0;
const OFF_BIN_AMOUNT_Y: usize = 8;
const OFF_BIN_PRICE: usize = 16;
const OFF_BIN_LIQUIDITY_SUPPLY: usize = 32;

/// Denominator of every fee rate in this program. A rate of 100,000 is one basis point.
const FEE_PRECISION: u128 = 1_000_000_000;
/// The program's own ceiling on a total fee: ten percent.
const MAX_FEE_RATE: u128 = 100_000_000;
/// Scale the variable fee is divided by. Meteora's `VARIABLE_FEE_PRECISION`.
const VARIABLE_FEE_SCALE: u128 = 100_000_000_000;
/// One whole unit of the accumulator, which counts bins crossed in basis-point units.
const BASIS_POINT_MAX: u32 = 10_000;

/// How far below the best available price a linearised quote may reach, in basis points.
///
/// # The judgement call, and what it costs
///
/// A bin's depth is whatever one price level happens to hold, and that is often small.
/// On the SOL/USDC pool captured in this crate's fixtures the active bin held $404 of
/// SOL and **$2.09** of USDC; the next bin down held $374. So a quote confined to one
/// bin can offer a rate that is exactly right and a size that cannot pay a transaction
/// fee, while a quote spanning a few bins has real depth at a rate that must be
/// pessimistic by roughly the span to stay honest.
///
/// The pessimism is not the whole span: the rate is the *capacity-weighted average* over
/// the window, and for a trade far smaller than the window the real average is better
/// than the quote by the difference between them. Measured at the size this book
/// trades — around $12 against a window of a few hundred — the quote sits about a basis
/// point under the truth, part of which is not pessimism at all, because the trade
/// genuinely does cross more than one bin.
///
/// Two basis points rather than more, because the residual pessimism is paid on every
/// quote while the extra depth is only used by trades this account cannot fund. It is
/// expressed in basis points rather than in bins so that it self-scales: a pool with a
/// ten-basis-point bin step gets one bin, which is right, because its single bin holds
/// proportionally more.
pub const MAX_QUOTE_SPAN_BPS: i32 = 2;

/// Hard ceiling on the bins one linearised quote may span, whatever the bin step.
pub const MAX_QUOTE_BINS: i32 = 16;

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn i16_at(d: &[u8], o: usize) -> i16 {
    i16::from_le_bytes([d[o], d[o + 1]])
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

fn u64_at(d: &[u8], o: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[o..o + 8]);
    u64::from_le_bytes(b)
}

fn i64_at(d: &[u8], o: usize) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[o..o + 8]);
    i64::from_le_bytes(b)
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

fn div_ceil(a: u128, b: u128) -> Option<u128> {
    if b == 0 {
        return None;
    }
    Some(a / b + u128::from(a % b != 0))
}

/// The pool account, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LbPair {
    pub token_x_mint: Pubkey32,
    pub token_y_mint: Pubkey32,
    pub reserve_x: Pubkey32,
    pub reserve_y: Pubkey32,
    pub oracle: Pubkey32,
    /// The bin the price currently sits in. Bin `i` prices at
    /// `(1 + bin_step/10_000)^i`, in token-y units per token-x unit before decimals.
    pub active_id: i32,
    /// Lowest and highest bin the pool will ever use.
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    /// Width of one bin, in basis points.
    pub bin_step: u16,
    /// Scales the base fee. See [`LbPair::base_fee_rate`].
    pub base_factor: u16,
    /// A power of ten applied to the base fee. Zero on every pool seen so far, and
    /// ignoring it on a pool that sets it would understate the fee.
    pub base_fee_power_factor: u8,
    /// Scales the volatility-driven part of the fee.
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    /// How volatile the pool currently considers itself. State, not configuration.
    pub volatility_accumulator: u32,
    /// The accumulator's floor until the decay window elapses.
    pub volatility_reference: u32,
    /// The bin the accumulator measures movement away from.
    pub index_reference: i32,
    pub last_update_timestamp: i64,
    /// Seconds of quiet before the references are re-anchored.
    pub filter_period: i16,
    /// Seconds of quiet after which the volatility reference drops to zero.
    pub decay_period: i16,
    /// What fraction of the accumulator survives re-anchoring, in hundredths of a
    /// percent.
    pub reduction_factor: u16,
    /// The protocol's cut of the fee, in hundredths of a percent.
    pub protocol_share: u16,
    /// Non-zero means the pool is not open for swapping.
    pub status: u8,
}

impl LbPair {
    /// The fee charged on every bin crossed, before the volatility surcharge, in units
    /// of 1e-9.
    ///
    /// Meteora's formula is `base_factor · bin_step · 10 · 10^base_fee_power_factor`. On
    /// the captured SOL/USDC pool that is `10_000 · 1 · 10` = 100,000, which is the one
    /// basis point the pool advertises.
    #[must_use]
    pub fn base_fee_rate(&self) -> u128 {
        let scale = 10u128.checked_pow(u32::from(self.base_fee_power_factor)).unwrap_or(u128::MAX);
        u128::from(self.base_factor)
            .saturating_mul(u128::from(self.bin_step))
            .saturating_mul(10)
            .saturating_mul(scale)
    }

    /// The fee charged on every bin crossed, before the surcharge, in basis points.
    #[must_use]
    pub fn base_fee_bps(&self) -> f64 {
        self.base_fee_rate() as f64 / FEE_PRECISION as f64 * 10_000.0
    }

    /// The volatility surcharge at a given accumulator value, in units of 1e-9.
    ///
    /// Rounded up, as the program rounds it — a fee the program rounds up and we round
    /// down is a fill we quote and do not get.
    #[must_use]
    pub fn variable_fee_rate(&self, accumulator: u32) -> u128 {
        let vfa = u128::from(accumulator).saturating_mul(u128::from(self.bin_step));
        let squared = vfa.saturating_mul(vfa);
        let num = squared.saturating_mul(u128::from(self.variable_fee_control));
        num.saturating_add(VARIABLE_FEE_SCALE - 1) / VARIABLE_FEE_SCALE
    }

    /// Total fee rate at a given accumulator, in units of 1e-9, capped where the program
    /// caps it.
    #[must_use]
    pub fn total_fee_rate(&self, accumulator: u32) -> u128 {
        self.base_fee_rate().saturating_add(self.variable_fee_rate(accumulator)).min(MAX_FEE_RATE)
    }

    /// The accumulator a swap reaching `bin_id` would be charged at, assuming no decay.
    ///
    /// # Why no decay is the safe assumption
    ///
    /// The program re-anchors its references once `filter_period` seconds of quiet have
    /// passed, and re-anchoring can only *reduce* the accumulator — the reference index
    /// becomes the active bin, so the distance term vanishes, and the surviving
    /// volatility is scaled down by `reduction_factor`. Assuming the anchor still stands
    /// therefore over-estimates the fee, never under-estimates it, and an over-estimated
    /// fee costs an opportunity while an under-estimated one costs money.
    #[must_use]
    pub fn accumulator_at(&self, bin_id: i32) -> u32 {
        let distance = self.index_reference.abs_diff(bin_id);
        self.volatility_reference
            .saturating_add(distance.saturating_mul(BASIS_POINT_MAX))
            .min(self.max_volatility_accumulator)
    }

    /// Total fee as parts per million, rounded **up** to the next whole ppm.
    ///
    /// The router's leg carries an integer ppm, so the conversion has to round somewhere
    /// and up is the only safe direction. One ppm is a hundredth of a basis point.
    #[must_use]
    pub fn fee_ppm_at(&self, bin_id: i32) -> u32 {
        let rate = self.total_fee_rate(self.accumulator_at(bin_id));
        let ppm = div_ceil(rate, 1_000).unwrap_or(u128::from(u32::MAX));
        u32::try_from(ppm).unwrap_or(u32::MAX)
    }

    /// Price of the active bin, in token-y units per token-x unit, ignoring decimals.
    ///
    /// The **marginal** price, and a reporting number only. What a trade of any size
    /// receives depends on how much liquidity sits in this bin and the ones beyond it,
    /// which is what [`quote_exact_in`] is for.
    #[must_use]
    pub fn marginal_price(&self) -> f64 {
        (1.0 + f64::from(self.bin_step) / 10_000.0).powi(self.active_id)
    }

    /// How many bins one linearised quote may span on this pool. At least one.
    #[must_use]
    pub fn quote_span_bins(&self) -> i32 {
        let step = i32::from(self.bin_step).max(1);
        (MAX_QUOTE_SPAN_BPS / step + 1).clamp(1, MAX_QUOTE_BINS)
    }

    /// The bin the price currently sits in, from arrays already decoded.
    #[must_use]
    pub fn active_bin<'a>(&self, arrays: &'a [BinArray]) -> Option<&'a Bin> {
        find_bin(arrays, self.active_id)
    }
}

/// Decode an `LbPair` account.
///
/// # Errors
/// If the account is not the right length, carries a bin step of zero — which would make
/// every bin the same price and is not a pool anyone can swap through — reports a status
/// other than open, or places its own active bin outside its own range.
pub fn decode(data: &[u8]) -> Result<LbPair> {
    ensure!(
        data.len() == LB_PAIR_LEN,
        "lb_pair account is {} bytes, not {LB_PAIR_LEN} — this is not the account it claims",
        data.len()
    );
    let bin_step = u16_at(data, OFF_BIN_STEP);
    ensure!(bin_step > 0, "a bin step of zero prices every bin the same");
    let status = data[OFF_STATUS];
    ensure!(status == 0, "lb_pair status {status} is not open for swapping");
    let min_bin_id = i32_at(data, OFF_MIN_BIN_ID);
    let max_bin_id = i32_at(data, OFF_MAX_BIN_ID);
    let active_id = i32_at(data, OFF_ACTIVE_ID);
    ensure!(
        min_bin_id <= active_id && active_id <= max_bin_id,
        "active bin {active_id} sits outside the pool's own range {min_bin_id}..={max_bin_id} \
         — the layout drifted"
    );
    Ok(LbPair {
        token_x_mint: pubkey_at(data, OFF_TOKEN_X_MINT),
        token_y_mint: pubkey_at(data, OFF_TOKEN_Y_MINT),
        reserve_x: pubkey_at(data, OFF_RESERVE_X),
        reserve_y: pubkey_at(data, OFF_RESERVE_Y),
        oracle: pubkey_at(data, OFF_ORACLE),
        active_id,
        min_bin_id,
        max_bin_id,
        bin_step,
        base_factor: u16_at(data, OFF_BASE_FACTOR),
        base_fee_power_factor: data[OFF_BASE_FEE_POWER_FACTOR],
        variable_fee_control: u32_at(data, OFF_VARIABLE_FEE_CONTROL),
        max_volatility_accumulator: u32_at(data, OFF_MAX_VOLATILITY_ACCUMULATOR),
        volatility_accumulator: u32_at(data, OFF_VOLATILITY_ACCUMULATOR),
        volatility_reference: u32_at(data, OFF_VOLATILITY_REFERENCE),
        index_reference: i32_at(data, OFF_INDEX_REFERENCE),
        last_update_timestamp: i64_at(data, OFF_LAST_UPDATE_TIMESTAMP),
        filter_period: i16_at(data, OFF_FILTER_PERIOD),
        decay_period: i16_at(data, OFF_DECAY_PERIOD),
        reduction_factor: u16_at(data, OFF_REDUCTION_FACTOR),
        protocol_share: u16_at(data, OFF_PROTOCOL_SHARE),
        status,
    })
}

/// One price level's holdings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bin {
    pub amount_x: u64,
    pub amount_y: u64,
    /// Price of this bin in Q64, token-y base units per token-x base unit.
    ///
    /// Stored by the program rather than recomputed here, which matters: the program
    /// fills against *this* number, so deriving our own from `bin_step` and the id would
    /// be quoting a price the pool does not use.
    pub price_x64: u128,
    pub liquidity_supply: u128,
}

impl Bin {
    /// What this bin can give when the swap wants token Y, i.e. is spending token X.
    #[must_use]
    pub fn out_for(&self, swap_for_y: bool) -> u64 {
        if swap_for_y {
            self.amount_y
        } else {
            self.amount_x
        }
    }

    /// Input needed to take `out` units out of this bin, ignoring the fee.
    ///
    /// Rounded **up**: a fraction of a base unit we do not hand over is a fraction of the
    /// output the program will not give.
    #[must_use]
    pub fn input_for_output(&self, out: u128, swap_for_y: bool) -> Option<u128> {
        if self.price_x64 == 0 {
            return None;
        }
        // Both branches floor, then add one back when the product does not reach `out`,
        // which is the ceiling without needing a wider integer.
        if swap_for_y {
            let scaled = shl_q64_div(out, self.price_x64)?;
            let back = mul_shr_q64(scaled, self.price_x64)?;
            Some(scaled + u128::from(back < out))
        } else {
            let scaled = mul_shr_q64(out, self.price_x64)?;
            let back = shl_q64_div(scaled, self.price_x64)?;
            Some(scaled + u128::from(back < out))
        }
    }

    /// Output for `amount_in` at this bin's price, ignoring the fee and the bin's own
    /// depth. Floored, because the program floors.
    #[must_use]
    pub fn output_for_input(&self, amount_in: u128, swap_for_y: bool) -> Option<u128> {
        if self.price_x64 == 0 {
            return None;
        }
        if swap_for_y {
            mul_shr_q64(amount_in, self.price_x64)
        } else {
            shl_q64_div(amount_in, self.price_x64)
        }
    }
}

/// One `BinArray` account: seventy consecutive bins, addressed by array index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinArray {
    pub index: i64,
    pub lb_pair: Pubkey32,
    pub bins: Vec<Bin>,
}

/// Which array holds a bin.
///
/// Floor division, so it is correct on both sides of zero — and every pool priced below
/// 1.0 in its own units sits at negative bin ids, which is most of them. Truncating
/// division picks the array one span too high for exactly those pools, and does it
/// silently: the address it derives is a real account, it just holds other bins.
#[must_use]
pub fn bin_array_index(bin_id: i32) -> i64 {
    i64::from(bin_id.div_euclid(BINS_PER_ARRAY))
}

/// The first bin id an array holds.
#[must_use]
pub fn bin_array_first_id(index: i64) -> i64 {
    index * i64::from(BINS_PER_ARRAY)
}

impl BinArray {
    /// The bin with this id, or `None` if this array does not hold it.
    #[must_use]
    pub fn bin(&self, bin_id: i32) -> Option<&Bin> {
        if bin_array_index(bin_id) != self.index {
            return None;
        }
        let offset = i64::from(bin_id) - bin_array_first_id(self.index);
        self.bins.get(usize::try_from(offset).ok()?)
    }
}

/// Decode a `BinArray` account.
///
/// # Errors
/// If the account is the wrong length.
pub fn decode_bin_array(data: &[u8]) -> Result<BinArray> {
    ensure!(
        data.len() == BIN_ARRAY_LEN,
        "bin_array account is {} bytes, not {BIN_ARRAY_LEN}",
        data.len()
    );
    let bins = (0..BINS_PER_ARRAY)
        .map(|i| {
            let o = OFF_BA_BINS + usize::try_from(i).unwrap_or(0) * BIN_LEN;
            Bin {
                amount_x: u64_at(data, o + OFF_BIN_AMOUNT_X),
                amount_y: u64_at(data, o + OFF_BIN_AMOUNT_Y),
                price_x64: u128_at(data, o + OFF_BIN_PRICE),
                liquidity_supply: u128_at(data, o + OFF_BIN_LIQUIDITY_SUPPLY),
            }
        })
        .collect();
    Ok(BinArray {
        index: i64_at(data, OFF_BA_INDEX),
        lb_pair: pubkey_at(data, OFF_BA_LB_PAIR),
        bins,
    })
}

/// Find the bin with this id among several arrays.
#[must_use]
pub fn find_bin(arrays: &[BinArray], bin_id: i32) -> Option<&Bin> {
    arrays.iter().find_map(|a| a.bin(bin_id))
}

/// The result of walking bins until an exact-input swap is filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// What the swap receives.
    pub amount_out: u128,
    /// What it actually spends. Less than the request when liquidity ran out first.
    pub amount_in_used: u128,
    /// Fee taken out of the input, in input units.
    pub fee: u128,
    /// Bins the price moved through. Zero for a swap contained in one bin.
    pub bins_crossed: u32,
    /// The bin the price ends in.
    pub end_bin_id: i32,
}

/// Walk bins as the program does, for an exact-input swap.
///
/// `swap_for_y` means spending token X to receive token Y, which moves the price **down**
/// and consumes bins from the active id downwards.
///
/// This is the reference implementation. It is not what the router quotes from — see
/// [`to_pool_state`] for that, and for why — but it is what that quote is checked
/// against, and it is the honest answer to "what would this trade actually get".
///
/// # Returns
/// `None` when the arrays supplied do not cover a bin the walk needs, which is a fact
/// about the fetch rather than about the pool and must never be silently read as "no
/// liquidity": a swap priced against a missing bin array is priced against nothing.
#[must_use]
pub fn quote_exact_in(
    pair: &LbPair,
    arrays: &[BinArray],
    amount_in: u128,
    swap_for_y: bool,
) -> Option<Fill> {
    let mut remaining = amount_in;
    let mut out_total: u128 = 0;
    let mut fee_total: u128 = 0;
    let mut bin_id = pair.active_id;
    let mut crossed = 0u32;

    while remaining > 0 {
        if bin_id < pair.min_bin_id || bin_id > pair.max_bin_id {
            break;
        }
        let bin = find_bin(arrays, bin_id)?;
        let available = u128::from(bin.out_for(swap_for_y));
        if available > 0 && bin.price_x64 > 0 {
            let fee_rate = pair.total_fee_rate(pair.accumulator_at(bin_id));
            let net_to_drain = bin.input_for_output(available, swap_for_y)?;
            // The program takes its fee out of the input, so the gross needed to drain a
            // bin is the net grossed up by the fee.
            let gross_to_drain =
                div_ceil(net_to_drain.checked_mul(FEE_PRECISION)?, FEE_PRECISION - fee_rate)?;

            let gross = remaining.min(gross_to_drain);
            let fee = div_ceil(gross.checked_mul(fee_rate)?, FEE_PRECISION)?;
            let net = gross.checked_sub(fee)?;
            let out = bin.output_for_input(net, swap_for_y)?.min(available);

            out_total = out_total.checked_add(out)?;
            fee_total = fee_total.checked_add(fee)?;
            remaining = remaining.checked_sub(gross)?;
        }
        if remaining == 0 {
            break;
        }
        let next = if swap_for_y { bin_id.checked_sub(1)? } else { bin_id.checked_add(1)? };
        if next < pair.min_bin_id || next > pair.max_bin_id {
            break;
        }
        bin_id = next;
        crossed = crossed.saturating_add(1);
    }

    Some(Fill {
        amount_out: out_total,
        amount_in_used: amount_in - remaining,
        fee: fee_total,
        bins_crossed: crossed,
        end_bin_id: bin_id,
    })
}

/// One direction's linearisation: a rate, a bound, and the bin the fee was priced at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Linear {
    rate_x64: u128,
    max_in: u128,
    worst_bin_id: i32,
}

/// Linearise one direction over a window of bins, starting at the first one that can
/// actually fill.
///
/// # Why an average rate at the bound is safe at every smaller size
///
/// A swap fills from the best price outward, so the average rate it receives *falls* as
/// it grows. Quoting every size at the average rate the window's full capacity would get
/// is therefore a floor on what any smaller size gets, and the bound stops the quote
/// where the window's liquidity stops. Both halves matter: the rate alone would happily
/// quote a size the bins cannot fill.
///
/// The rate excludes the fee, which the leg applies separately, so the two do not
/// double-charge.
fn linearise(pair: &LbPair, arrays: &[BinArray], swap_for_y: bool) -> Linear {
    let span = pair.quote_span_bins();
    let step: i32 = if swap_for_y { -1 } else { 1 };

    // Leading bins holding nothing on the output side cost nothing to skip: the program
    // walks straight through them, so the best price actually available is the first bin
    // holding something rather than the active one. Skipping them is what keeps a pool
    // quotable when the price has come to rest against an empty bin — which is the normal
    // state of the active bin much of the time.
    let mut first = pair.active_id;
    let mut hops = 0;
    loop {
        if first < pair.min_bin_id || first > pair.max_bin_id || hops > MAX_QUOTE_BINS {
            return Linear::default();
        }
        match find_bin(arrays, first) {
            Some(b) if b.out_for(swap_for_y) > 0 && b.price_x64 > 0 => break,
            Some(_) => {}
            // Off the end of what was fetched. Whatever is out there is unknown, and an
            // unknown bin is not one we may quote through.
            None => return Linear::default(),
        }
        first += step;
        hops += 1;
    }

    let mut net_in: u128 = 0;
    let mut out: u128 = 0;
    let mut worst = first;
    for k in 0..span {
        let id = first + step * k;
        if id < pair.min_bin_id || id > pair.max_bin_id {
            break;
        }
        let Some(bin) = find_bin(arrays, id) else { break };
        let available = u128::from(bin.out_for(swap_for_y));
        if available == 0 || bin.price_x64 == 0 {
            continue;
        }
        let Some(need) = bin.input_for_output(available, swap_for_y) else { break };
        let (Some(n), Some(o)) = (net_in.checked_add(need), out.checked_add(available)) else {
            break;
        };
        net_in = n;
        out = o;
        worst = id;
    }

    if net_in == 0 || out == 0 {
        return Linear::default();
    }
    // Floor, so the rate is never rounded in our own favour.
    let Some(rate_x64) = shl_q64_div(out, net_in) else { return Linear::default() };
    Linear { rate_x64, max_in: net_in, worst_bin_id: worst }
}

/// Decode a pool and the bin arrays around its active price into a quotable
/// [`PoolState`].
///
/// `arrays` must include the array containing `active_id`; supplying its neighbours as
/// well widens the window the quote can draw depth from, and supplying none of them is
/// refused rather than quoted.
///
/// # Errors
/// If either account fails to decode, if an array belongs to a different pool, or if
/// neither direction can be filled from what was supplied — which is a real state for a
/// pool whose price has walked off the end of the arrays that were fetched, and is a
/// reason to fetch again rather than to quote.
pub fn to_pool_state(
    address: Pubkey32,
    pool_data: &[u8],
    arrays: &[&[u8]],
    slot: u64,
) -> Result<PoolState> {
    let pair = decode(pool_data)?;
    let decoded: Vec<BinArray> =
        arrays.iter().map(|d| decode_bin_array(d)).collect::<Result<Vec<_>>>()?;
    state_from(address, &pair, &decoded, slot)
}

/// The same thing from already-decoded accounts.
///
/// The live watcher holds both decoded, because a bin array is ten kilobytes and an
/// account update arrives many times a second — re-parsing seventy bins on every one of
/// them to answer a question the previous parse already answered is work nobody asked
/// for.
///
/// # Errors
/// As [`to_pool_state`], minus the decoding.
pub fn state_from(
    address: Pubkey32,
    pair: &LbPair,
    decoded: &[BinArray],
    slot: u64,
) -> Result<PoolState> {
    for a in decoded {
        ensure!(
            a.lb_pair == address,
            "bin array {} belongs to another pool — it would price this one from somebody \
             else's liquidity",
            a.index
        );
    }
    ensure!(
        decoded.iter().any(|a| a.index == bin_array_index(pair.active_id)),
        "none of the {} arrays supplied holds the active bin {} — this pool cannot be priced \
         from them",
        decoded.len(),
        pair.active_id
    );

    let sell_x = linearise(pair, decoded, true);
    let sell_y = linearise(pair, decoded, false);
    ensure!(
        sell_x.max_in > 0 || sell_y.max_in > 0,
        "the bins around {} hold nothing on either side",
        pair.active_id
    );

    // One fee for the leg, and it has to be the worse of the two directions: a leg
    // carries a single `fee_ppm`, and the surcharge grows with distance from the
    // volatility reference, so the direction that walks further pays more.
    let fee_ppm = [sell_x, sell_y]
        .iter()
        .filter(|l| l.max_in > 0)
        .map(|l| pair.fee_ppm_at(l.worst_bin_id))
        .max()
        .unwrap_or_else(|| pair.fee_ppm_at(pair.active_id));

    Ok(PoolState {
        id: PoolId(address),
        dex: Dex::MeteoraDlmm,
        mint_a: pair.token_x_mint,
        mint_b: pair.token_y_mint,
        math: PoolMath::Bounded {
            rate_a_x64: sell_x.rate_x64,
            max_in_a: sell_x.max_in,
            rate_b_x64: sell_y.rate_x64,
            max_in_b: sell_y.max_in,
        },
        fee_ppm,
        slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::clmm::Q64;

    fn b64(s: &str) -> Vec<u8> {
        let table = |c: u8| -> i32 {
            match c {
                b'A'..=b'Z' => i32::from(c - b'A'),
                b'a'..=b'z' => i32::from(c - b'a') + 26,
                b'0'..=b'9' => i32::from(c - b'0') + 52,
                b'+' => 62,
                b'/' => 63,
                _ => -1,
            }
        };
        let (mut acc, mut bits, mut out) = (0i32, 0, Vec::new());
        for &c in s.trim().as_bytes() {
            let v = table(c);
            if v < 0 {
                continue;
            }
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from((acc >> bits) & 0xFF).unwrap_or(0));
            }
        }
        out
    }

    /// Real mainnet bytes for the Meteora DLMM SOL/USDC pool
    /// `HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR`, captured 2026-09-12.
    fn real_pool_account() -> Vec<u8> {
        b64(include_str!("../tests/fixtures/meteora_dlmm_sol_usdc.b64"))
    }

    /// The same pool and the bin array holding its active bin, read in **one**
    /// `getMultipleAccounts` so the two belong to the same moment. A pool account from
    /// one slot and a bin array from another is exactly the torn read this venue's
    /// machinery exists to avoid, and a fixture that tore would let a test pass on state
    /// that never existed.
    fn coherent() -> (Vec<u8>, Vec<u8>) {
        (
            b64(include_str!("../tests/fixtures/meteora_dlmm_pair.b64")),
            b64(include_str!("../tests/fixtures/meteora_dlmm_bin_array.b64")),
        )
    }

    const POOL_ADDRESS: &str = "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR";

    fn pool_key() -> Pubkey32 {
        let mut k = [0u8; 32];
        k.copy_from_slice(&bs58::decode(POOL_ADDRESS).into_vec().unwrap());
        k
    }

    fn to_b58(k: &Pubkey32) -> String {
        bs58::encode(k).into_string()
    }

    #[test]
    fn the_fixtures_are_the_expected_sizes() {
        assert_eq!(real_pool_account().len(), LB_PAIR_LEN);
        let (p, a) = coherent();
        assert_eq!(p.len(), LB_PAIR_LEN);
        assert_eq!(a.len(), BIN_ARRAY_LEN);
    }

    /// The four-way agreement the offsets were established by: the program named these
    /// accounts itself, in its own `swap2` instruction, in the same slot this account was
    /// captured. If an offset ever drifts, one of these stops matching.
    #[test]
    fn the_decoded_accounts_are_the_ones_the_program_named() {
        let p = decode(&real_pool_account()).expect("a real pool decodes");
        assert_eq!(to_b58(&p.token_x_mint), "So11111111111111111111111111111111111111112");
        assert_eq!(to_b58(&p.token_y_mint), "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_eq!(to_b58(&p.reserve_x), "H7j5NPopj3tQvDg4N8CxwtYciTn3e8AEV6wSVrxpyDUc");
        assert_eq!(to_b58(&p.reserve_y), "HbYjRzx7teCxqW3unpXBEcNHhfVZvW2vW9MQ99TkizWt");
        assert_eq!(to_b58(&p.oracle), "EgEYXef2FCoEYLHJJW74dMbom1atLXo6KwPuA6mSATYA");
    }

    /// The two numbers no account list can check, checked against the market instead.
    #[test]
    fn the_bin_price_reproduces_the_market_price_of_sol() {
        let p = decode(&real_pool_account()).unwrap();
        assert_eq!(p.bin_step, 1, "this is the one-basis-point pool");
        assert_eq!(p.active_id, -22780);

        let usd = p.marginal_price() * 1_000.0;
        assert!(
            (usd - 102.73).abs() < 1.0,
            "decoded price {usd:.2} should sit within a dollar of the $102.73 quoted at \
             capture; this is the check that the offsets are the right ones"
        );
    }

    /// A one-basis-point pool charges one basis point. The formula has three scaling
    /// factors and getting any of them wrong lands somewhere absurd rather than slightly
    /// off, which is what makes this worth asserting exactly.
    #[test]
    fn the_base_fee_is_the_advertised_one() {
        let p = decode(&real_pool_account()).unwrap();
        assert!((p.base_fee_bps() - 1.0).abs() < 1e-9, "got {} bps", p.base_fee_bps());
        assert_eq!(p.base_fee_rate(), 100_000);
    }

    /// The bug this file shipped with once: `volatility_accumulator` is the first field of
    /// `VariableParameters`, not the third. Read eight bytes late it lands on
    /// `index_reference`, whose value is a negative bin id — which reinterpreted as
    /// unsigned is about 4.29 billion, far past this pool's own ceiling of 100,000. That
    /// impossibility is the assertion.
    #[test]
    fn the_volatility_fields_are_read_as_state_and_not_as_a_bin_id() {
        let (pool, _) = coherent();
        let p = decode(&pool).unwrap();
        assert!(
            p.volatility_accumulator <= p.max_volatility_accumulator,
            "accumulator {} exceeds the pool's own ceiling {} — the offset is wrong",
            p.volatility_accumulator,
            p.max_volatility_accumulator
        );
        assert!(p.volatility_reference <= p.volatility_accumulator);
        assert!(
            p.min_bin_id <= p.index_reference && p.index_reference <= p.max_bin_id,
            "index_reference {} is not a bin id",
            p.index_reference
        );
        assert_eq!(p.max_volatility_accumulator, 100_000);
        assert_eq!(p.variable_fee_control, 2_000_000);
        assert_eq!(p.protocol_share, 1_000);
        assert_eq!(p.filter_period, 10);
        assert_eq!(p.decay_period, 120);
        assert_eq!(p.reduction_factor, 5_000);
        assert!(p.last_update_timestamp > 1_700_000_000, "not a plausible unix time");
    }

    /// The surcharge at this pool's own ceiling is two basis points, so a wrong scaling
    /// factor shows up as a fee that is either invisible or enormous, never as one that
    /// is plausibly off.
    #[test]
    fn the_volatility_surcharge_is_bounded_and_small() {
        let (pool, _) = coherent();
        let p = decode(&pool).unwrap();
        assert_eq!(
            p.variable_fee_rate(p.max_volatility_accumulator),
            200_000,
            "two basis points at the pool's own ceiling"
        );
        assert_eq!(p.variable_fee_rate(0), 0, "no volatility, no surcharge");
        assert_eq!(p.total_fee_rate(u32::MAX), MAX_FEE_RATE, "capped where the program caps it");
    }

    /// The accumulator a swap is charged at grows with distance from the reference and
    /// stops at the pool's ceiling. Ten thousand per bin is the program's unit.
    #[test]
    fn the_accumulator_grows_one_unit_per_bin_and_then_stops() {
        let (pool, _) = coherent();
        let p = decode(&pool).unwrap();
        assert_eq!(p.accumulator_at(p.index_reference), p.volatility_reference);
        assert_eq!(p.accumulator_at(p.index_reference - 1), p.volatility_reference + 10_000);
        assert_eq!(
            p.accumulator_at(p.index_reference - 1_000),
            p.max_volatility_accumulator,
            "distance must saturate at the ceiling, not overflow past it"
        );
    }

    /// The census that established the bin layout, reproduced from the fixture: the array
    /// holds the active bin, bins above it hold token X, bins below hold token Y. Getting
    /// this backwards is the mistake that reverses a swap.
    #[test]
    fn bins_above_the_active_id_hold_x_and_bins_below_hold_y() {
        let (pool, array) = coherent();
        let p = decode(&pool).unwrap();
        let a = decode_bin_array(&array).unwrap();
        assert_eq!(a.lb_pair, pool_key(), "the array must name the pool it belongs to");
        assert_eq!(a.index, bin_array_index(p.active_id));
        assert_eq!(a.bins.len(), usize::try_from(BINS_PER_ARRAY).unwrap());

        let first = i32::try_from(bin_array_first_id(a.index)).unwrap();
        let mut above_with_y = 0;
        let mut below_with_x = 0;
        for i in 0..BINS_PER_ARRAY {
            let id = first + i;
            let bin = a.bin(id).expect("every id in range resolves");
            if id > p.active_id && bin.amount_y > 0 {
                above_with_y += 1;
            }
            if id < p.active_id && bin.amount_x > 0 {
                below_with_x += 1;
            }
        }
        assert_eq!(above_with_y, 0, "a bin above the price must hold no token Y");
        assert_eq!(below_with_x, 0, "a bin below the price must hold no token X");
    }

    /// Each bin's stored price must be the previous one scaled by exactly the bin step,
    /// and the active bin's must be the price the pool's own fields imply. This is the
    /// check on `OFF_BIN_PRICE`, independent of any amount.
    #[test]
    fn consecutive_bin_prices_differ_by_exactly_one_bin_step() {
        let (pool, array) = coherent();
        let p = decode(&pool).unwrap();
        let a = decode_bin_array(&array).unwrap();
        let step = 1.0 + f64::from(p.bin_step) / 10_000.0;
        let first = i32::try_from(bin_array_first_id(a.index)).unwrap();
        for i in 1..BINS_PER_ARRAY {
            let lo = a.bin(first + i - 1).unwrap().price_x64 as f64;
            let hi = a.bin(first + i).unwrap().price_x64 as f64;
            assert!(
                (hi / lo / step - 1.0).abs() < 1e-9,
                "bins {} and {} are {}x apart, not {step}x",
                first + i - 1,
                first + i,
                hi / lo
            );
        }
        let active = a.bin(p.active_id).unwrap().price_x64 as f64 / Q64 as f64;
        assert!(
            (active / p.marginal_price() - 1.0).abs() < 1e-6,
            "the active bin prices at {active} while the pool's own fields say {}",
            p.marginal_price()
        );
    }

    #[test]
    fn the_array_index_floors_on_both_sides_of_zero() {
        assert_eq!(bin_array_index(0), 0);
        assert_eq!(bin_array_index(69), 0);
        assert_eq!(bin_array_index(70), 1);
        assert_eq!(bin_array_index(-1), -1, "truncating division would say 0 here");
        assert_eq!(bin_array_index(-70), -1);
        assert_eq!(bin_array_index(-71), -2);
        for id in [-22849, -22850, -70, -1, 0, 1, 70, 12_345] {
            let start = bin_array_first_id(bin_array_index(id));
            assert!(start <= i64::from(id) && i64::from(id) < start + i64::from(BINS_PER_ARRAY));
        }
    }

    /// A swap small enough to sit inside one bin gets that bin's price less the fee, and
    /// crosses nothing. The numbers come from the fixture, so this pins the arithmetic
    /// against a real price rather than a round one.
    #[test]
    fn a_swap_inside_one_bin_crosses_nothing() {
        let (pool, array) = coherent();
        let p = decode(&pool).unwrap();
        let arrays = vec![decode_bin_array(&array).unwrap()];
        let active = *p.active_bin(&arrays).expect("the fixture holds the active bin");

        // A fifth of what the active bin holds on the Y side, bought with X.
        let want_out = u128::from(active.amount_y) / 5;
        let spend = active.input_for_output(want_out, true).unwrap();
        assert!(spend > 0, "the fixture's active bin must hold something on the Y side");
        let fill = quote_exact_in(&p, &arrays, spend, true).expect("the array covers this");
        assert_eq!(fill.bins_crossed, 0, "this size fits in the active bin");
        assert_eq!(fill.amount_in_used, spend, "and is filled completely");
        assert!(fill.fee > 0, "a one-basis-point pool charges something");

        let net = fill.amount_in_used - fill.fee;
        let ideal = active.output_for_input(net, true).unwrap();
        assert_eq!(fill.amount_out, ideal, "inside one bin the price is exactly the bin's");
    }

    /// A swap bigger than the nearest bin must walk, and the walk must be monotone: more
    /// input, more output, never less.
    #[test]
    fn a_swap_larger_than_one_bin_walks_and_stays_monotone() {
        let (pool, array) = coherent();
        let p = decode(&pool).unwrap();
        let arrays = vec![decode_bin_array(&array).unwrap()];

        // A whole SOL at nine decimals, which on this pool crosses bins.
        let unit = 1_000_000_000u128;
        let big = quote_exact_in(&p, &arrays, unit, true).unwrap();
        assert!(big.bins_crossed > 0, "a whole SOL must cross at least one bin here");

        let mut last = 0u128;
        for m in 1..=40u128 {
            let f = quote_exact_in(&p, &arrays, unit * m / 40, true).unwrap();
            assert!(f.amount_out >= last, "output fell from {last} to {}", f.amount_out);
            last = f.amount_out;
        }
    }

    /// The walk must never hand back more of the output token than the bins it walked
    /// actually held. This is the failure that would cost money on chain, so it is checked
    /// against the fixture's own totals rather than against a model.
    #[test]
    fn the_walk_never_promises_more_than_the_bins_hold() {
        let (pool, array) = coherent();
        let p = decode(&pool).unwrap();
        let a = decode_bin_array(&array).unwrap();
        let first = i32::try_from(bin_array_first_id(a.index)).unwrap();
        let total_y: u128 = (0..BINS_PER_ARRAY)
            .filter(|i| first + i <= p.active_id)
            .filter_map(|i| a.bin(first + i))
            .map(|b| u128::from(b.amount_y))
            .sum();
        assert!(total_y > 0, "the fixture must have something below the active bin");

        // Everything the array holds below the active bin, in input terms, less a
        // margin: asking for the full amount would walk off the end of the one array
        // supplied, which is correctly a refusal rather than a partial fill and is what
        // `a_walk_that_leaves_the_fetched_arrays_refuses_rather_than_stopping` covers.
        let drain: u128 = (0..BINS_PER_ARRAY)
            .filter(|i| first + i <= p.active_id)
            .filter_map(|i| a.bin(first + i))
            .filter(|b| b.amount_y > 0)
            .filter_map(|b| b.input_for_output(u128::from(b.amount_y), true))
            .sum();
        let arrays = vec![a];
        let fill = quote_exact_in(&p, &arrays, drain * 9 / 10, true).unwrap();
        assert!(
            fill.amount_out <= total_y,
            "promised {} of token Y from bins holding {total_y}",
            fill.amount_out
        );
        assert!(fill.bins_crossed > 1, "nine tenths of a whole array must cross many bins");
        assert_eq!(fill.amount_in_used, drain * 9 / 10, "and it is all spendable");
    }

    /// Missing bin arrays are not thin liquidity. A quote priced against a bin nobody
    /// fetched is priced against nothing, and must come back as `None` rather than as a
    /// small number.
    #[test]
    fn a_walk_that_leaves_the_fetched_arrays_refuses_rather_than_stopping() {
        let (pool, _) = coherent();
        let p = decode(&pool).unwrap();
        assert!(
            quote_exact_in(&p, &[], 1_000, true).is_none(),
            "with no arrays at all there is nothing to price against"
        );
    }

    /// The property the router's safety rests on: the leg it is handed never quotes more
    /// than an exact bin-by-bin walk would give, at any size it accepts.
    #[test]
    fn the_linearised_leg_never_out_quotes_the_exact_walk() {
        let (pool, array) = coherent();
        let p = decode(&pool).unwrap();
        let arrays = vec![decode_bin_array(&array).unwrap()];
        let state = to_pool_state(pool_key(), &pool, &[&array], 99).unwrap();

        for (mint, swap_for_y) in [(p.token_x_mint, true), (p.token_y_mint, false)] {
            let Some(leg) = state.leg_for_input(&mint) else { continue };
            assert!(leg.max_in > 0);
            for d in [10_000u128, 1_000, 100, 10, 4, 2, 1] {
                let size = leg.max_in / d;
                if size == 0 {
                    continue;
                }
                let quoted = leg.quote(size).expect("inside the bound");
                let exact = quote_exact_in(&p, &arrays, size, swap_for_y).expect("covered");
                assert!(
                    quoted <= exact.amount_out,
                    "swap_for_y={swap_for_y} at {size}: leg quoted {quoted} but the walk gives \
                     only {}",
                    exact.amount_out
                );
            }
            assert!(leg.quote(leg.max_in + 1).is_none(), "past the bound must refuse");
        }
    }

    /// And it must not be *needlessly* under: a quote that gives away basis points to be
    /// safe would never find an edge. At the size this account trades the gap has to stay
    /// inside a few basis points of the exact walk.
    #[test]
    fn the_linearised_leg_stays_within_a_few_basis_points_of_the_walk() {
        let (pool, array) = coherent();
        let p = decode(&pool).unwrap();
        let arrays = vec![decode_bin_array(&array).unwrap()];
        let state = to_pool_state(pool_key(), &pool, &[&array], 99).unwrap();

        // About twelve dollars, which is what this wallet holds: 0.117 SOL at nine
        // decimals, and 12 USDC at six.
        for (mint, swap_for_y, want) in
            [(p.token_x_mint, true, 117_000_000u128), (p.token_y_mint, false, 12_000_000u128)]
        {
            let leg = state.leg_for_input(&mint).expect("both directions quote on this pool");
            let size = want.min(leg.max_in);
            let quoted = leg.quote(size).unwrap();
            let exact = quote_exact_in(&p, &arrays, size, swap_for_y).unwrap().amount_out;
            assert!(exact > 0);
            let shortfall_bps = (exact - quoted) as f64 / exact as f64 * 10_000.0;
            assert!(
                shortfall_bps < 4.0,
                "swap_for_y={swap_for_y}: the leg is {shortfall_bps:.2} bps under the walk, \
                 which is too much to find an edge through"
            );
        }
    }

    /// The depth reported must be the real bin depth and not the arithmetic device the
    /// quote is built from, or every depth comparison in the instrument is fiction.
    #[test]
    fn the_reported_depth_is_the_bins_and_not_the_flat_reserve() {
        let (pool, array) = coherent();
        let state = to_pool_state(pool_key(), &pool, &[&array], 7).unwrap();
        let p = decode(&pool).unwrap();
        let arrays = vec![decode_bin_array(&array).unwrap()];

        let sell_x = state.quotable_depth(&p.token_x_mint).unwrap();
        let fill = quote_exact_in(&p, &arrays, sell_x, true).unwrap();
        assert_eq!(fill.amount_in_used, sell_x, "the bound must be a size the bins can take");
        assert_eq!(state.reserve_a(), sell_x);
        assert_eq!(state.dex, Dex::MeteoraDlmm);
        assert_eq!(state.slot, 7);
        assert!(
            (100..=400).contains(&state.fee_ppm),
            "fee {} ppm is not one basis point plus a small surcharge",
            state.fee_ppm
        );
    }

    /// The price the dashboard and the USD index read has to be the market's, not the
    /// ratio of two depths in two different tokens.
    #[test]
    fn the_reported_price_is_the_market_price() {
        let (pool, array) = coherent();
        let state = to_pool_state(pool_key(), &pool, &[&array], 1).unwrap();
        let usd = state.spot_price().unwrap() * 1_000.0;
        assert!((90.0..130.0).contains(&usd), "priced SOL at ${usd:.2}");
    }

    /// An array from a different pool would price this one from somebody else's
    /// liquidity, which is the worst available failure and so is an error, not a skip.
    #[test]
    fn an_array_belonging_to_another_pool_is_refused() {
        let (pool, mut array) = coherent();
        array[OFF_BA_LB_PAIR] ^= 0xFF;
        let err = to_pool_state(pool_key(), &pool, &[&array], 1).unwrap_err().to_string();
        assert!(err.contains("belongs to another pool"), "unexpected error: {err}");
    }

    #[test]
    fn a_pool_whose_active_array_was_not_supplied_is_refused() {
        let (pool, array) = coherent();
        let mut other = array.clone();
        // Move the array two indices away, so it no longer holds the active bin.
        let idx = i64_at(&array, OFF_BA_INDEX) + 2;
        other[OFF_BA_INDEX..OFF_BA_INDEX + 8].copy_from_slice(&idx.to_le_bytes());
        let err = to_pool_state(pool_key(), &pool, &[&other], 1).unwrap_err().to_string();
        assert!(err.contains("holds the active bin"), "unexpected error: {err}");
        assert!(to_pool_state(pool_key(), &pool, &[], 1).is_err(), "no arrays is no quote");
    }

    #[test]
    fn accounts_of_the_wrong_shape_are_refused() {
        assert!(decode(&[0u8; 100]).is_err());
        assert!(decode_bin_array(&[0u8; 100]).is_err());
        let mut zeroed = vec![0u8; LB_PAIR_LEN];
        assert!(decode(&zeroed).is_err(), "a zero bin step is not a pool");
        zeroed[OFF_BIN_STEP] = 1;
        assert!(decode(&zeroed).is_ok(), "and a nonzero one is, however empty the rest");
        zeroed[OFF_STATUS] = 1;
        assert!(decode(&zeroed).is_err(), "a pool the program has closed must not be quoted");
    }

    /// The span rule has to self-scale with the bin step, or a coarse pool would be quoted
    /// across a window many basis points wide.
    #[test]
    fn the_quote_window_narrows_as_the_bin_step_widens() {
        let mut p = decode(&real_pool_account()).unwrap();
        for (step, bins) in [(1u16, 3i32), (2, 2), (3, 1), (10, 1), (100, 1)] {
            p.bin_step = step;
            assert_eq!(p.quote_span_bins(), bins, "bin step {step}");
        }
    }
}
