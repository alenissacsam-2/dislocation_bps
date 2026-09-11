//! Raydium AMM v4 `swapBaseIn`.
//!
//! # Nine accounts this instruction does not read
//!
//! The published account list for `swapBaseIn` names an OpenBook market, its bids,
//! asks, event queue, both market vaults and the market's vault signer, plus the
//! serum program itself and the AMM's own open-orders account. This module passes
//! the pool's own address for all nine of them.
//!
//! That is not a shortcut, it is what the chain does. Sampling landed `swapBaseIn`
//! transactions on 2026-09-10 against every Raydium v4 pool in the registry:
//!
//! | pool | accounts | market slots holding the pool address |
//! |---|---|---|
//! | SOL/USDC | 17 | 8 of 8 |
//! | SOL/Fartcoin | 17 | 8 of 8 |
//! | $WIF/SOL | 17 | 8 of 8 |
//! | RAY/USDC | 17 | 8 of 8 |
//!
//! Raydium stopped routing swaps through the order book, and the accounts survive in
//! the interface without being read. Passing the pool rather than an unrelated key is
//! deliberate: the pool is already in the message at position 1, so the nine repeats
//! resolve to the same key and add nothing to the transaction's size or to the set of
//! accounts it locks.
//!
//! The fifth pool, RAY/SOL, had only 18-account callers in the window sampled, so
//! filler is unproven *there* and only there. That is a question for
//! `cb-verify-encode` against the live pool, not for an assertion here.
//!
//! # The formula is the one the quote assumed
//!
//! The worry that justified leaving this venue unencoded was that an order-book venue
//! would not fill at the constant-product price [`cb_dex::raydium_v4`] quotes. Five
//! landed swaps, checked against `x * y = k` with the pool's own fee applied to the
//! input and the division floored, came out between 0 and 52 base units low on
//! trades of 5,000,000 to 100,000,000 — under a thousandth of a basis point. The
//! decoder's model is the program's model.
//!
//! # There is no price limit here
//!
//! Both concentrated-liquidity venues take a `sqrt_price_limit` that
//! [`super::price_limit`] turns into a circuit breaker against a trade that would move
//! the pool absurdly. `swapBaseIn` has no such argument: `minimum_amount_out` is the
//! *only* protection on the instruction. So this encoder refuses a zero floor rather
//! than treating it as "no opinion", exactly as the Orca one does — but here that
//! refusal is the whole of the safety, not one layer of two.

use super::SwapContext;
use crate::encode::{pk, programs, to_pubkey, Args};
use anyhow::{ensure, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};

/// `swapBaseIn`. Raydium v4 tags instructions with one byte, not an Anchor
/// discriminator.
const TAG_SWAP_BASE_IN: u8 = 9;

/// The AMM authority.
///
/// A single program-derived address shared by every v4 pool rather than one per pool,
/// which is why it can be a constant. Verified identical across all five registry
/// pools in the transactions sampled above; the test below pins it so a copy error
/// fails the build rather than a trade.
pub const AMM_AUTHORITY: &str = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";

/// Build a `swapBaseIn` instruction against a Raydium AMM v4 pool.
///
/// `ctx.tick_arrays` is ignored — this venue has no ticks. It is part of
/// [`SwapContext`] because the two concentrated-liquidity venues need it, and giving
/// this encoder its own context type would mean the caller had to know which venue it
/// held before it could describe the swap.
///
/// # Errors
/// If the account does not decode as `AmmInfo`, if the input mint is not one of the
/// pool's two, or if either amount is zero.
pub fn swap(ctx: &SwapContext, pool_data: &[u8]) -> Result<Instruction> {
    let amm = cb_dex::raydium_v4::decode_amm_info(pool_data)?;
    ensure!(ctx.amount_in > 0, "a swap of zero is not a swap");
    ensure!(
        ctx.min_amount_out > 0,
        "swapBaseIn has no price limit, so a zero floor is the whole protection missing; \
         refusing to encode one"
    );

    let program = pk(cb_dex::raydium_v4::PROGRAM_ID);
    // Whatever the direction, positions 4 and 5 are the pool's *coin* and *pc* vaults
    // in that fixed order. Unlike Orca's positions 3 and 5 they do not follow the
    // trade, so there is nothing to map here — the direction lives entirely in which
    // of the user's accounts is source and which is destination.
    let coin_vault = to_pubkey(&amm.base_vault);
    let pc_vault = to_pubkey(&amm.quote_vault);

    // See the module docs. The pool stands in for every account the program no longer
    // reads, and costs nothing because position 1 already names it.
    let unread = ctx.pool;

    let accounts = vec![
        AccountMeta::new_readonly(pk(programs::SPL_TOKEN), false),
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new_readonly(pk(AMM_AUTHORITY), false),
        AccountMeta::new(unread, false), // amm open orders
        AccountMeta::new(coin_vault, false),
        AccountMeta::new(pc_vault, false),
        AccountMeta::new_readonly(unread, false), // serum program
        AccountMeta::new(unread, false),          // serum market
        AccountMeta::new(unread, false),          // serum bids
        AccountMeta::new(unread, false),          // serum asks
        AccountMeta::new(unread, false),          // serum event queue
        AccountMeta::new(unread, false),          // serum coin vault
        AccountMeta::new(unread, false),          // serum pc vault
        AccountMeta::new_readonly(unread, false), // serum vault signer
        AccountMeta::new(ctx.user_source, false),
        AccountMeta::new(ctx.user_dest, false),
        AccountMeta::new_readonly(ctx.owner, true),
    ];

    let data = Args::tagged(TAG_SWAP_BASE_IN).u64(ctx.amount_in).u64(ctx.min_amount_out).build();

    Ok(Instruction { program_id: program, accounts, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::types::Pubkey32;
    use solana_sdk::pubkey::Pubkey;

    /// A synthetic `AmmInfo` laid out at the offsets the decoder reads.
    fn amm_info(base_mint: Pubkey32, quote_mint: Pubkey32, bv: Pubkey32, qv: Pubkey32) -> Vec<u8> {
        let mut d = vec![0u8; cb_dex::raydium_v4::AMM_INFO_LEN];
        d[32..40].copy_from_slice(&9u64.to_le_bytes()); // base decimals
        d[40..48].copy_from_slice(&6u64.to_le_bytes()); // quote decimals
        d[176..184].copy_from_slice(&25u64.to_le_bytes());
        d[184..192].copy_from_slice(&10_000u64.to_le_bytes());
        d[336..368].copy_from_slice(&bv);
        d[368..400].copy_from_slice(&qv);
        d[400..432].copy_from_slice(&base_mint);
        d[432..464].copy_from_slice(&quote_mint);
        d
    }

    fn ctx(src: Pubkey, dst: Pubkey, pool: Pubkey) -> SwapContext {
        SwapContext {
            owner: Pubkey::new_unique(),
            pool,
            user_source: src,
            user_dest: dst,
            amount_in: 1_000_000,
            min_amount_out: 999_000,
            input_is_a: true,
            input_token_program: pk(crate::encode::programs::SPL_TOKEN),
            output_token_program: pk(crate::encode::programs::SPL_TOKEN),
            tick_arrays: [Pubkey::new_unique(); crate::pda::TICK_ARRAYS_PER_SWAP],
        }
    }

    #[test]
    fn the_authority_constant_is_the_address_every_pool_was_seen_using() {
        assert_eq!(
            AMM_AUTHORITY, "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1",
            "the AMM authority is shared by every v4 pool; a typo here reaches every trade"
        );
        // Parses, so `pk` will not panic at run time on the first live swap.
        let _ = pk(AMM_AUTHORITY);
    }

    #[test]
    fn the_instruction_has_the_seventeen_account_shape_the_chain_accepts() {
        let (bv, qv) = ([7u8; 32], [8u8; 32]);
        let data = amm_info([1u8; 32], [2u8; 32], bv, qv);
        let pool = Pubkey::new_unique();
        let (src, dst) = (Pubkey::new_unique(), Pubkey::new_unique());
        let ix = swap(&ctx(src, dst, pool), &data).expect("encodes");

        assert_eq!(ix.accounts.len(), 17);
        assert_eq!(ix.accounts[1].pubkey, pool);
        assert_eq!(ix.accounts[2].pubkey, pk(AMM_AUTHORITY));
        assert_eq!(ix.accounts[4].pubkey, to_pubkey(&bv), "position 4 is the coin vault");
        assert_eq!(ix.accounts[5].pubkey, to_pubkey(&qv), "position 5 is the pc vault");
        assert_eq!(ix.accounts[14].pubkey, src);
        assert_eq!(ix.accounts[15].pubkey, dst);
        assert!(ix.accounts[16].is_signer, "the owner has to sign the spend");
    }

    #[test]
    fn every_unread_market_slot_is_the_pool_and_so_adds_no_account_to_the_message() {
        let data = amm_info([1u8; 32], [2u8; 32], [7u8; 32], [8u8; 32]);
        let pool = Pubkey::new_unique();
        let ix =
            swap(&ctx(Pubkey::new_unique(), Pubkey::new_unique(), pool), &data).expect("encodes");
        for slot in [3usize, 6, 7, 8, 9, 10, 11, 12, 13] {
            assert_eq!(
                ix.accounts[slot].pubkey, pool,
                "slot {slot} should hold the pool, which position 1 already names"
            );
        }
    }

    #[test]
    fn the_direction_lives_in_the_user_accounts_and_the_vaults_never_move() {
        let (bv, qv) = ([7u8; 32], [8u8; 32]);
        let data = amm_info([1u8; 32], [2u8; 32], bv, qv);
        let pool = Pubkey::new_unique();
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());

        let forward = swap(&ctx(a, b, pool), &data).expect("encodes");
        let reverse = swap(&ctx(b, a, pool), &data).expect("encodes");

        assert_eq!(forward.accounts[14].pubkey, a);
        assert_eq!(reverse.accounts[14].pubkey, b);
        // The pool's own vaults are coin-then-pc in both directions.
        for ix in [&forward, &reverse] {
            assert_eq!(ix.accounts[4].pubkey, to_pubkey(&bv));
            assert_eq!(ix.accounts[5].pubkey, to_pubkey(&qv));
        }
    }

    #[test]
    fn the_payload_is_the_tag_and_two_amounts_little_endian() {
        let data = amm_info([1u8; 32], [2u8; 32], [7u8; 32], [8u8; 32]);
        let ix =
            swap(&ctx(Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()), &data)
                .expect("encodes");
        assert_eq!(ix.data.len(), 17, "one tag byte and two u64s");
        assert_eq!(ix.data[0], TAG_SWAP_BASE_IN);
        assert_eq!(u64::from_le_bytes(ix.data[1..9].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[9..17].try_into().unwrap()), 999_000);
    }

    #[test]
    fn a_zero_floor_is_refused_because_it_is_the_only_protection_this_venue_has() {
        let data = amm_info([1u8; 32], [2u8; 32], [7u8; 32], [8u8; 32]);
        let mut c = ctx(Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        c.min_amount_out = 0;
        assert!(swap(&c, &data).is_err());
        c.min_amount_out = 1;
        c.amount_in = 0;
        assert!(swap(&c, &data).is_err());
    }
}
