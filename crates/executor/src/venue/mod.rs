//! Per-venue swap instruction builders.
//!
//! # Which venues, and why not the others
//!
//! Orca Whirlpool and Raydium CLMM are 82 of the 90 pools in the registry. Both are
//! concentrated-liquidity, so they share the tick-array machinery in [`crate::pda`],
//! and both keep every account a swap needs either in the pool account or derivable
//! from it.
//!
//! Raydium AMM v4 is five pools and needs the pool's OpenBook market, bids, asks,
//! event queue, both market vaults and the market's vault signer — nine accounts that
//! live in the AMM account at offsets this codebase has never read, plus an order-book
//! program whose behaviour is not the constant-product formula the quote assumed.
//! CP-Swap is two pools and Meteora DAMM v2 is one. Encoding the three of them is
//! about as much work as the two above and reaches 9% of the universe, so they refuse
//! by returning an error naming themselves rather than pretending.
//!
//! # The price limit is derived, not constant
//!
//! Both programs require a `sqrt_price_limit` strictly inside their own MIN/MAX
//! constants, and the obvious encoding is to paste those constants and pass the
//! extreme. Two problems: the two venues' constants differ in their last few digits
//! for rounding reasons, and a pasted constant that is one too far *out* fails a range
//! check for a reason that looks nothing like "wrong number".
//!
//! So the limit is computed from the pool's live `sqrt_price` instead — half it when
//! the price is falling, double it when rising. Doubling a square root is a 4× price
//! move, which no arbitrage of ours will ever approach, so it never binds; but it is
//! always in range without knowing the exact extreme, and it is a real circuit breaker
//! against a size that would move the pool by more than any plausible trade should.

pub mod orca;
pub mod raydium;

use crate::encode::pk;
use anyhow::{bail, Result};
use cb_core::types::Dex;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

/// Everything a swap needs that is not venue-specific.
#[derive(Debug, Clone)]
pub struct SwapContext {
    /// The signer, which is also the owner of both token accounts.
    pub owner: Pubkey,
    /// The pool's own account address.
    pub pool: Pubkey,
    /// The account tokens are spent from.
    pub user_source: Pubkey,
    /// The account tokens are received into.
    pub user_dest: Pubkey,
    /// Exact input. Both venues are called in `is_base_input` mode.
    pub amount_in: u64,
    /// The floor below which the swap must fail rather than fill.
    ///
    /// This is the only real protection on a swap, because the price limit is
    /// deliberately set never to bind. A zero here would let the pool return dust.
    pub min_amount_out: u64,
    /// True when the input mint is the pool's token A / token 0.
    pub input_is_a: bool,
    /// The tick arrays the program will be given, in traversal order.
    ///
    /// An input rather than something the encoder derives, because choosing them
    /// correctly needs to know which ones exist on chain and an encoder cannot make a
    /// round trip. See [`crate::pda::tick_array_sweep`] for the measurement that forced
    /// this, and [`crate::ticks::resolve`] for the resolver that fills it in.
    pub tick_arrays: [Pubkey; crate::pda::TICK_ARRAYS_PER_SWAP],
}

/// Scale a live square-root price into a limit that is in range and will not bind.
///
/// See the module docs. Halving or doubling a square root is a 4× move in price.
#[must_use]
pub fn price_limit(sqrt_price_x64: u128, price_falling: bool, min: u128, max: u128) -> u128 {
    let raw = if price_falling {
        sqrt_price_x64 / 2
    } else {
        sqrt_price_x64.saturating_mul(2)
    };
    // Strictly inside, because both programs use strict inequalities.
    raw.clamp(min.saturating_add(1), max.saturating_sub(1))
}

/// Build the swap instruction for a pool, given its raw account data.
///
/// `pool_data` is the pool account exactly as the node returned it, and is decoded
/// here rather than passed in already-parsed: the accounts a swap needs and the
/// numbers that priced it must come from the same read, or the instruction can name
/// vaults belonging to a state the quote never saw.
///
/// # Errors
/// If the venue is not one of the two implemented, or the account does not decode.
pub fn build_swap(
    dex: Dex,
    ctx: &SwapContext,
    pool_data: &[u8],
    extra: &VenueExtra,
) -> Result<Instruction> {
    match dex {
        Dex::OrcaWhirlpool => orca::swap(ctx, pool_data),
        Dex::RaydiumClmm => {
            raydium::swap(ctx, pool_data, extra.token_program, extra.bitmap_policy)
        }
        other => bail!(
            "{} swaps are not encoded — see crates/executor/src/venue/mod.rs for why",
            other.name()
        ),
    }
}

/// Venue-specific inputs that cannot be recovered from the pool account itself.
#[derive(Debug, Clone, Copy)]
pub struct VenueExtra {
    /// The token program owning both mints. Raydium CLMM's v1 `swap` takes exactly
    /// one, so a pool mixing classic and Token-2022 mints cannot use it.
    pub token_program: Pubkey,
    /// Whether Raydium's swap is given the tick-array bitmap extension. See
    /// [`raydium::BitmapPolicy`] — this is settled by simulation, not by assertion.
    pub bitmap_policy: raydium::BitmapPolicy,
}

impl Default for VenueExtra {
    fn default() -> Self {
        Self {
            token_program: pk(crate::encode::programs::SPL_TOKEN),
            // Raydium's own SDK passes it, so it is the better of the two guesses —
            // and it is only a guess until `--verify-encode` has run against a pool.
            bitmap_policy: raydium::BitmapPolicy::Include,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u128 = 4_295_048_016;
    const MAX: u128 = 79_226_673_515_401_279_992_447_579_055;

    #[test]
    fn the_limit_moves_the_right_way_and_stays_strictly_in_range() {
        let p = 1u128 << 64;
        let down = price_limit(p, true, MIN, MAX);
        let up = price_limit(p, false, MIN, MAX);
        assert!(down < p, "a falling price must be limited below the current one");
        assert!(up > p, "a rising price must be limited above the current one");
        for v in [down, up] {
            assert!(v > MIN && v < MAX, "{v} is not strictly inside the range");
        }
    }

    /// A pool already sitting near an extreme must not produce a limit outside it.
    #[test]
    fn the_limit_clamps_at_both_extremes() {
        assert!(price_limit(MIN + 2, true, MIN, MAX) > MIN);
        assert!(price_limit(MAX - 2, false, MIN, MAX) < MAX);
        // And a degenerate zero price cannot underflow into an invalid limit.
        assert!(price_limit(0, true, MIN, MAX) > MIN);
        assert!(price_limit(u128::MAX, false, MIN, MAX) < MAX);
    }

    #[test]
    fn unimplemented_venues_refuse_by_name_rather_than_encoding_something() {
        let ctx = SwapContext {
            owner: Pubkey::new_unique(),
            pool: Pubkey::new_unique(),
            user_source: Pubkey::new_unique(),
            user_dest: Pubkey::new_unique(),
            amount_in: 1,
            min_amount_out: 1,
            input_is_a: true,
            tick_arrays: [Pubkey::new_unique(); crate::pda::TICK_ARRAYS_PER_SWAP],
        };
        let extra = VenueExtra::default();
        for dex in [Dex::RaydiumAmmV4, Dex::RaydiumCpmm, Dex::MeteoraDammV2, Dex::PumpSwap] {
            let e = build_swap(dex, &ctx, &[0u8; 2000], &extra).unwrap_err().to_string();
            assert!(e.contains(dex.name()), "refusal for {dex:?} does not name it: {e}");
        }
    }
}
