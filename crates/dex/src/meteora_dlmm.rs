//! Meteora DLMM (`LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo`).
//!
//! # Why this decodes a pool it will not quote
//!
//! Two independent censuses put this venue at the front of the queue. Fifteen
//! consecutive mainnet blocks, filtered to closed-loop arbitrage, produced thirteen
//! winners and Meteora DLMM appears in four of them. Asking an outside router for the
//! best round trip on twenty-two tokens, it is the venue holding the mispriced side
//! more often than any other this codebase can see. It is where the money is.
//!
//! It is also the venue where being wrong is most expensive, because a DLMM's price
//! lives in the pool account and its *depth* does not. Liquidity sits in discrete bins,
//! spread across separate `BinArray` accounts of seventy bins each, and a swap walks
//! outward from the active bin until it is filled. A quote that reads only the pool
//! account knows the price at the margin and nothing about how much is available
//! there — and the error is one-directional: it always reports more fill at a better
//! rate than the pool will give. That is the shape of mistake this instrument has
//! already made once and spent weeks unwinding.
//!
//! So this module stops where the evidence stops. It decodes the pool account, whose
//! every field below was checked against something the chain independently asserted,
//! and it returns no [`cb_core::types::PoolState`], because a `PoolState` is a promise
//! that a quote can be priced from it. Bin traversal is the remaining work and it needs
//! the bin arrays fetched alongside the pool; see the note at the end of this file.
//!
//! # How the offsets below were established
//!
//! Not from an IDL. Every `swap2` instruction on this program in those same fifteen
//! blocks was dumped with its accounts resolved — 102 calls in four shapes — which
//! names the pool's reserves, mints and oracle from the program's own point of view.
//! Decoding the pool account then had to reproduce those exact addresses at fixed
//! offsets, which is a four-way agreement that no plausible wrong guess survives.
//!
//! The two numeric fields cannot be checked that way, so they were checked against the
//! market instead: `bin_step` and `active_id` together give a price of
//! `(1 + bin_step/10_000)^active_id`, and on the SOL/USDC pool captured here that is
//! $102.50 against $102.73 quoted by an outside router at the same moment — a fifth of
//! a percent apart, which is this pool's own bin granularity. A wrong `active_id` would
//! not be out by a fifth of a percent; it would be out by orders of magnitude.
//!
//! # The `swap2` account list, for whoever writes the encoder
//!
//! Identical across all 102 calls, with positions 1 and 9 carrying the program's own id
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
//! Arguments are `amount_in: u64`, `min_amount_out: u64`, then a
//! `RemainingAccountsInfo` whose empty form is four zero bytes — which is what the
//! calls passing plain bin arrays use, so that is the form to emit.

use anyhow::{ensure, Result};
use cb_core::types::Pubkey32;

pub const PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

/// `LbPair` is a fixed-size account. A different length is a different account.
pub const LB_PAIR_LEN: usize = 904;

// Verified offsets. See the module docs for what each was checked against.
const OFF_BASE_FACTOR: usize = 8;
const OFF_VARIABLE_FEE_CONTROL: usize = 16;
const OFF_MAX_VOLATILITY_ACCUMULATOR: usize = 20;
const OFF_PROTOCOL_SHARE: usize = 32;
const OFF_VOLATILITY_ACCUMULATOR: usize = 48;
const OFF_ACTIVE_ID: usize = 76;
const OFF_BIN_STEP: usize = 80;
const OFF_TOKEN_X_MINT: usize = 88;
const OFF_TOKEN_Y_MINT: usize = 120;
const OFF_RESERVE_X: usize = 152;
const OFF_RESERVE_Y: usize = 184;
const OFF_ORACLE: usize = 552;

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&data[offset..offset + 4]);
    u32::from_le_bytes(b)
}

fn i32_at(data: &[u8], offset: usize) -> i32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&data[offset..offset + 4]);
    i32::from_le_bytes(b)
}

fn pubkey_at(data: &[u8], offset: usize) -> Pubkey32 {
    let mut k = [0u8; 32];
    k.copy_from_slice(&data[offset..offset + 32]);
    k
}

/// The pool account, decoded. Everything a swap needs except its depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LbPair {
    pub token_x_mint: Pubkey32,
    pub token_y_mint: Pubkey32,
    pub reserve_x: Pubkey32,
    pub reserve_y: Pubkey32,
    pub oracle: Pubkey32,
    /// The bin the price currently sits in. Bin `i` prices at
    /// `(1 + bin_step/10_000)^i`, in token-y units per token-x unit before decimals.
    pub active_id: i32,
    /// Width of one bin, in basis points.
    pub bin_step: u16,
    /// Scales the base fee. See [`LbPair::base_fee_bps`].
    pub base_factor: u16,
    /// Scales the volatility-driven part of the fee.
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    /// How volatile the pool currently considers itself. Moves with trading and decays
    /// with time, so unlike everything else here it is *state* and goes stale.
    pub volatility_accumulator: u32,
    /// The protocol's cut of the fee, in hundredths of a percent.
    pub protocol_share: u16,
}

impl LbPair {
    /// The fee charged on every bin crossed, before the volatility surcharge, in bps.
    ///
    /// Meteora's own formula is `base_factor * bin_step * 10` in units of 1e-9. On the
    /// captured SOL/USDC pool that is `10_000 * 1 * 10 / 1e9` = one basis point, which
    /// is what the pool advertises.
    #[must_use]
    pub fn base_fee_bps(&self) -> f64 {
        f64::from(self.base_factor) * f64::from(self.bin_step) * 10.0 / 1e9 * 10_000.0
    }

    /// Price of the active bin, in token-y units per token-x unit, ignoring decimals.
    ///
    /// This is the **marginal** price and it is all the pool account can tell you. What
    /// a trade of any size would actually receive depends on how much liquidity sits in
    /// this bin and the ones beyond it, which lives in accounts this module does not
    /// read. Sizing anything against this number would overstate the fill, always.
    #[must_use]
    pub fn marginal_price(&self) -> f64 {
        (1.0 + f64::from(self.bin_step) / 10_000.0).powi(self.active_id)
    }
}

/// Decode an `LbPair` account.
///
/// # Errors
/// If the account is not the right length, or carries a bin step of zero, which would
/// make every bin the same price and is not a pool anyone can swap through.
pub fn decode(data: &[u8]) -> Result<LbPair> {
    ensure!(
        data.len() == LB_PAIR_LEN,
        "lb_pair account is {} bytes, not {LB_PAIR_LEN} — this is not the account it claims",
        data.len()
    );
    let bin_step = u16_at(data, OFF_BIN_STEP);
    ensure!(bin_step > 0, "a bin step of zero prices every bin the same");
    Ok(LbPair {
        token_x_mint: pubkey_at(data, OFF_TOKEN_X_MINT),
        token_y_mint: pubkey_at(data, OFF_TOKEN_Y_MINT),
        reserve_x: pubkey_at(data, OFF_RESERVE_X),
        reserve_y: pubkey_at(data, OFF_RESERVE_Y),
        oracle: pubkey_at(data, OFF_ORACLE),
        active_id: i32_at(data, OFF_ACTIVE_ID),
        bin_step,
        base_factor: u16_at(data, OFF_BASE_FACTOR),
        variable_fee_control: u32_at(data, OFF_VARIABLE_FEE_CONTROL),
        max_volatility_accumulator: u32_at(data, OFF_MAX_VOLATILITY_ACCUMULATOR),
        volatility_accumulator: u32_at(data, OFF_VOLATILITY_ACCUMULATOR),
        protocol_share: u16_at(data, OFF_PROTOCOL_SHARE),
    })
}

// # What is deliberately missing
//
// `to_pool_state`. Every other module in this crate has one, and its absence here is
// the point: a `PoolState` carries a `Leg` that can be asked for a quote at a size, and
// nothing in the account decoded above can answer that question honestly.
//
// To add it, three things are needed and none of them is a guess:
//
// 1. `BinArray` decoding. Seventy bins per account, each holding an x and a y amount.
//    The array covering bin `i` is index `i.div_euclid(70)`, at a PDA of
//    seeds `[b"bin_array", lb_pair, index.to_le_bytes()]`.
// 2. Traversal. A swap fills from the active bin outward in the direction of travel,
//    taking each bin's whole reserve on the relevant side before moving on, charging
//    the fee per bin. This is the part that makes the quote exact rather than
//    approximate, and it is why reading one account is not enough.
// 3. The variable fee. `volatility_accumulator` decays with elapsed time and grows with
//    bins crossed, so it is genuinely state and must be recomputed for the swap being
//    priced rather than read once.
//
// Until all three exist, a cycle through this venue is refused by the encoder, which is
// the correct behaviour and the reason it is safe to land this module first.

#[cfg(test)]
mod tests {
    use super::*;

    /// Real mainnet bytes for the Meteora DLMM SOL/USDC pool
    /// `HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR`, captured 2026-09-12.
    fn real_pool_account() -> Vec<u8> {
        let b64 = include_str!("../tests/fixtures/meteora_dlmm_sol_usdc.b64").trim();
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
        for &c in b64.as_bytes() {
            let v = table(c);
            if v < 0 {
                continue;
            }
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((acc >> bits) & 0xFF) as u8);
            }
        }
        out
    }

    fn to_b58(k: &Pubkey32) -> String {
        bs58::encode(k).into_string()
    }

    #[test]
    fn the_fixture_is_the_expected_size() {
        assert_eq!(real_pool_account().len(), LB_PAIR_LEN);
    }

    /// The four-way agreement the offsets were established by: the program named these
    /// accounts itself, in its own `swap2` instruction, in the same slot this account
    /// was captured. If an offset ever drifts, one of these stops matching.
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
    ///
    /// SOL was $102.73 to an outside router at the moment of capture. Nine decimals on
    /// one side and six on the other put three orders of magnitude between the raw bin
    /// price and the dollar price. A wrong `active_id` or `bin_step` does not miss this
    /// by a rounding error; it misses by powers of ten.
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
    /// factors in it and getting any of them wrong lands somewhere absurd rather than
    /// somewhere slightly off, which is what makes this worth asserting exactly.
    #[test]
    fn the_base_fee_is_the_advertised_one() {
        let p = decode(&real_pool_account()).unwrap();
        assert!((p.base_fee_bps() - 1.0).abs() < 1e-9, "got {} bps", p.base_fee_bps());
    }

    #[test]
    fn an_account_of_the_wrong_size_is_refused() {
        assert!(decode(&[0u8; 100]).is_err());
        let mut zeroed = vec![0u8; LB_PAIR_LEN];
        assert!(decode(&zeroed).is_err(), "a zero bin step is not a pool");
        zeroed[OFF_BIN_STEP] = 1;
        assert!(decode(&zeroed).is_ok(), "and a nonzero one is, however empty the rest");
    }
}
