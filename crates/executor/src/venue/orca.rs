//! Orca Whirlpool `swap`.
//!
//! # The trap in this account list
//!
//! Positions 3 and 5 are the user's token accounts for **token A and token B**, not
//! for *input* and *output*. Which of them is being spent depends on the direction,
//! so a builder that writes `user_source` into position 3 unconditionally is correct
//! for half of all swaps and silently reversed for the other half. It is not a
//! decoding error and no test of the instruction's *shape* catches it; the program
//! takes tokens out of the account it was told is A and the arithmetic is wrong from
//! there. [`swap`] maps by direction, and the test below asserts both directions.

use super::{price_limit, SwapContext};
use crate::encode::{pk, programs, to_pubkey, Args};
use crate::pda::orca_oracle;
use anyhow::{ensure, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};

/// The program's own bounds on `sqrt_price_limit`. Only used to clamp a limit that is
/// derived from live state, so being conservative here costs nothing.
const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
const MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;

/// Build a `swap` instruction against a Whirlpool, from the pool account's raw bytes.
///
/// # Errors
/// If the account does not decode as a Whirlpool — which includes adaptive-fee pools,
/// deliberately, since the decoder refuses those and quoting one understates its cost.
pub fn swap(ctx: &SwapContext, pool_data: &[u8]) -> Result<Instruction> {
    let w = cb_dex::orca_whirlpool::decode(pool_data)?;
    ensure!(ctx.amount_in > 0, "a swap of zero is not a swap");
    ensure!(
        ctx.min_amount_out > 0,
        "a swap with no output floor would accept dust; refusing to encode one"
    );

    let program = pk(cb_dex::orca_whirlpool::PROGRAM_ID);
    let a_to_b = ctx.input_is_a;

    // Positions 3 and 5 are A and B, not source and destination. See the module docs.
    let (owner_a, owner_b) = if a_to_b {
        (ctx.user_source, ctx.user_dest)
    } else {
        (ctx.user_dest, ctx.user_source)
    };

    let limit = price_limit(w.sqrt_price_x64, a_to_b, MIN_SQRT_PRICE_X64, MAX_SQRT_PRICE_X64);

    let accounts = vec![
        AccountMeta::new_readonly(pk(programs::SPL_TOKEN), false),
        AccountMeta::new_readonly(ctx.owner, true),
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new(owner_a, false),
        AccountMeta::new(to_pubkey(&w.vault_a), false),
        AccountMeta::new(owner_b, false),
        AccountMeta::new(to_pubkey(&w.vault_b), false),
        AccountMeta::new(ctx.tick_arrays[0], false),
        AccountMeta::new(ctx.tick_arrays[1], false),
        AccountMeta::new(ctx.tick_arrays[2], false),
        AccountMeta::new(orca_oracle(&ctx.pool, &program), false),
    ];

    let data = Args::anchor("swap")
        .u64(ctx.amount_in)
        .u64(ctx.min_amount_out)
        .u128(limit)
        // Exact input: the amount above is what we spend, the threshold is a floor on
        // what comes back. The other mode makes the threshold a *ceiling* on spend,
        // which would silently invert the protection.
        .bool(true)
        .bool(a_to_b)
        .build();

    Ok(Instruction { program_id: program, accounts, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::types::Pubkey32;
    use crate::pda::ORCA_TICKS_PER_ARRAY;
    use solana_sdk::pubkey::Pubkey;

    /// A synthetic Whirlpool account laid out at the offsets the decoder reads.
    fn whirlpool(mint_a: Pubkey32, mint_b: Pubkey32, va: Pubkey32, vb: Pubkey32) -> Vec<u8> {
        let mut d = vec![0u8; cb_dex::orca_whirlpool::WHIRLPOOL_LEN];
        let spacing: u16 = 64;
        d[41..43].copy_from_slice(&spacing.to_le_bytes());
        // The decoder rejects a pool whose seed disagrees with its spacing.
        d[43..45].copy_from_slice(&spacing.to_le_bytes());
        d[45..47].copy_from_slice(&400u16.to_le_bytes());
        d[49..65].copy_from_slice(&1_000_000_000_000u128.to_le_bytes());
        d[65..81].copy_from_slice(&(1u128 << 64).to_le_bytes());
        d[81..85].copy_from_slice(&0i32.to_le_bytes());
        d[101..133].copy_from_slice(&mint_a);
        d[133..165].copy_from_slice(&va);
        d[181..213].copy_from_slice(&mint_b);
        d[213..245].copy_from_slice(&vb);
        d
    }

    fn ctx(input_is_a: bool, src: Pubkey, dst: Pubkey, pool: Pubkey) -> SwapContext {
        SwapContext {
            owner: Pubkey::new_unique(),
            pool,
            user_source: src,
            user_dest: dst,
            amount_in: 1_000_000,
            min_amount_out: 999_000,
            input_is_a,
            tick_arrays: arrays(pool, input_is_a),
        }
    }

    /// The naive sequence, which is what a caller with no chain access would produce.
    fn arrays(pool: Pubkey, a_to_b: bool) -> [Pubkey; 3] {
        let program = pk(cb_dex::orca_whirlpool::PROGRAM_ID);
        let starts = crate::pda::tick_array_starts(0, 64, ORCA_TICKS_PER_ARRAY, a_to_b);
        [
            crate::pda::orca_tick_array(&pool, starts[0], &program),
            crate::pda::orca_tick_array(&pool, starts[1], &program),
            crate::pda::orca_tick_array(&pool, starts[2], &program),
        ]
    }

    /// The module's whole reason for existing. Spending A must put the source account
    /// at position 3; spending B must put it at position 5.
    #[test]
    fn the_user_accounts_swap_positions_with_the_direction() {
        let (src, dst, pool) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let data = whirlpool([1; 32], [2; 32], [3; 32], [4; 32]);

        let a = swap(&ctx(true, src, dst, pool), &data).expect("a->b");
        assert_eq!(a.accounts[3].pubkey, src, "spending A must spend from position 3");
        assert_eq!(a.accounts[5].pubkey, dst, "receiving B must receive into position 5");

        let b = swap(&ctx(false, src, dst, pool), &data).expect("b->a");
        assert_eq!(b.accounts[3].pubkey, dst, "receiving A must receive into position 3");
        assert_eq!(b.accounts[5].pubkey, src, "spending B must spend from position 5");

        // The vaults do not move with the direction — they are A and B unconditionally.
        for ix in [&a, &b] {
            assert_eq!(ix.accounts[4].pubkey, Pubkey::new_from_array([3; 32]));
            assert_eq!(ix.accounts[6].pubkey, Pubkey::new_from_array([4; 32]));
        }
    }

    #[test]
    fn the_direction_flag_is_the_last_byte_and_matches_the_layout() {
        let (src, dst, pool) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let data = whirlpool([1; 32], [2; 32], [3; 32], [4; 32]);

        let a = swap(&ctx(true, src, dst, pool), &data).unwrap();
        let b = swap(&ctx(false, src, dst, pool), &data).unwrap();
        assert_eq!(*a.data.last().unwrap(), 1, "a_to_b must be true when spending A");
        assert_eq!(*b.data.last().unwrap(), 0, "a_to_b must be false when spending B");
        // Exact-input mode, the byte before the direction.
        assert_eq!(a.data[a.data.len() - 2], 1);
    }

    #[test]
    fn the_argument_buffer_is_the_shape_the_program_expects() {
        let (src, dst, pool) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let data = whirlpool([1; 32], [2; 32], [3; 32], [4; 32]);
        let ix = swap(&ctx(true, src, dst, pool), &data).unwrap();

        // 8 discriminator + 8 + 8 + 16 + 1 + 1
        assert_eq!(ix.data.len(), 42);
        assert_eq!(&ix.data[..8], &crate::encode::anchor_discriminator("swap"));
        assert_eq!(&ix.data[8..16], &1_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[16..24], &999_000u64.to_le_bytes());
        assert_eq!(ix.accounts.len(), 11);
    }

    /// The signer must be the authority and nothing else in the list may be one.
    #[test]
    fn exactly_one_account_signs_and_it_is_the_authority() {
        let (src, dst, pool) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let data = whirlpool([1; 32], [2; 32], [3; 32], [4; 32]);
        let c = ctx(true, src, dst, pool);
        let ix = swap(&c, &data).unwrap();

        let signers: Vec<_> = ix.accounts.iter().filter(|a| a.is_signer).collect();
        assert_eq!(signers.len(), 1);
        assert_eq!(signers[0].pubkey, c.owner);
        assert!(!ix.accounts[0].is_writable, "the token program must not be writable");
        assert!(ix.accounts[2].is_writable, "the pool must be writable");
    }

    /// The arrays land in positions 7, 8 and 9 in the order the caller supplied, and
    /// are not re-derived or re-ordered by the encoder.
    #[test]
    fn the_supplied_tick_arrays_are_passed_through_in_order() {
        let (src, dst, pool) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let data = whirlpool([1; 32], [2; 32], [3; 32], [4; 32]);

        let mut c = ctx(true, src, dst, pool);
        let chosen = [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()];
        c.tick_arrays = chosen;
        let ix = swap(&c, &data).unwrap();
        for (i, want) in chosen.iter().enumerate() {
            assert_eq!(ix.accounts[7 + i].pubkey, *want, "array {i} moved");
            assert!(ix.accounts[7 + i].is_writable, "a tick array is written");
        }
    }

    #[test]
    fn a_swap_with_no_floor_or_no_amount_is_refused() {
        let (src, dst, pool) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let data = whirlpool([1; 32], [2; 32], [3; 32], [4; 32]);

        let mut zero_in = ctx(true, src, dst, pool);
        zero_in.amount_in = 0;
        assert!(swap(&zero_in, &data).is_err());

        let mut no_floor = ctx(true, src, dst, pool);
        no_floor.min_amount_out = 0;
        assert!(swap(&no_floor, &data).is_err());
    }

    /// The decoder refuses adaptive-fee pools, and the encoder must inherit that
    /// refusal rather than building a swap the quote never priced correctly.
    #[test]
    fn an_adaptive_fee_pool_is_refused_by_inheritance() {
        let (src, dst, pool) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let mut data = whirlpool([1; 32], [2; 32], [3; 32], [4; 32]);
        // Break the seed/spacing agreement, which is the adaptive-fee marker.
        data[43..45].copy_from_slice(&7u16.to_le_bytes());
        assert!(swap(&ctx(true, src, dst, pool), &data).is_err());
    }
}
