//! Raydium CLMM `swap`.
//!
//! # Where this differs from Orca, and why that matters
//!
//! Orca's account list is positional by *token* — A then B, whichever is being spent.
//! Raydium's is positional by *role* — input then output. The two conventions look
//! identical in a diagram and produce reversed instructions from the same inputs, so
//! neither builder shares code with the other even though both swap a CLMM pool.
//!
//! # The one thing here that is genuinely not known
//!
//! Raydium's `swap` takes its first tick array as a named account and the rest as
//! remaining accounts. Whether the pool's **tick-array bitmap extension** belongs at
//! the front of that remaining list is version-dependent: the program identifies it by
//! comparing the account's key against a PDA it derives itself, so passing it when it
//! is not wanted and omitting it when it is are both plausible failures, and the
//! extension account does not exist for every pool.
//!
//! Rather than pick one and hope, [`BitmapPolicy`] makes it a parameter and
//! `cb-bot --verify-encode` simulates both against each real pool and reports which
//! one the chain accepts. That converts a guess into a measurement, which is the only
//! move available for a fact that cannot be established from this machine.

use super::{price_limit, SwapContext};
use crate::encode::{pk, to_pubkey, Args};
use crate::pda::raydium_bitmap_extension;
use anyhow::{ensure, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
const MAX_SQRT_PRICE_X64: u128 = 79_226_673_521_066_979_257_578_248_091;

/// Whether to pass the tick-array bitmap extension. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapPolicy {
    /// Pass it as the first remaining account, which is what Raydium's own SDK does.
    Include,
    /// Pass only tick arrays.
    Omit,
}

/// Build a `swap` instruction against a Raydium CLMM pool.
///
/// # Errors
/// If the account does not decode, or the trade has no size or no output floor.
pub fn swap(
    ctx: &SwapContext,
    pool_data: &[u8],
    token_program: Pubkey,
    policy: BitmapPolicy,
) -> Result<Instruction> {
    let p = cb_dex::raydium_clmm::decode(pool_data)?;
    ensure!(ctx.amount_in > 0, "a swap of zero is not a swap");
    ensure!(
        ctx.min_amount_out > 0,
        "a swap with no output floor would accept dust; refusing to encode one"
    );

    let program = pk(cb_dex::raydium_clmm::PROGRAM_ID);
    // Spending token 0 moves the price down, which is `zero_for_one`.
    let zero_for_one = ctx.input_is_a;

    let (input_vault, output_vault) = if zero_for_one {
        (p.vault_0, p.vault_1)
    } else {
        (p.vault_1, p.vault_0)
    };

    let limit = price_limit(p.sqrt_price_x64, zero_for_one, MIN_SQRT_PRICE_X64, MAX_SQRT_PRICE_X64);

    let mut accounts = vec![
        AccountMeta::new(ctx.owner, true),
        AccountMeta::new_readonly(to_pubkey(&p.amm_config), false),
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new(ctx.user_source, false),
        AccountMeta::new(ctx.user_dest, false),
        AccountMeta::new(to_pubkey(&input_vault), false),
        AccountMeta::new(to_pubkey(&output_vault), false),
        AccountMeta::new(to_pubkey(&p.observation), false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new(ctx.tick_arrays[0], false),
    ];

    // Remaining accounts.
    if policy == BitmapPolicy::Include {
        accounts.push(AccountMeta::new(raydium_bitmap_extension(&ctx.pool, &program), false));
    }
    for array in &ctx.tick_arrays[1..] {
        accounts.push(AccountMeta::new(*array, false));
    }

    let data = Args::anchor("swap")
        .u64(ctx.amount_in)
        .u64(ctx.min_amount_out)
        .u128(limit)
        // is_base_input: the amount is what we spend.
        .bool(true)
        .build();

    Ok(Instruction { program_id: program, accounts, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::programs;

    /// A synthetic CLMM pool laid out at the offsets the decoder reads.
    fn pool() -> Vec<u8> {
        let mut d = vec![0u8; cb_dex::raydium_clmm::POOL_LEN];
        d[9..41].copy_from_slice(&[0x11; 32]); // amm_config
        d[73..105].copy_from_slice(&[0x22; 32]); // mint_0
        d[105..137].copy_from_slice(&[0x33; 32]); // mint_1
        d[137..169].copy_from_slice(&[0x44; 32]); // vault_0
        d[169..201].copy_from_slice(&[0x55; 32]); // vault_1
        d[201..233].copy_from_slice(&[0x66; 32]); // observation
        d[233] = 9;
        d[234] = 6;
        d[235..237].copy_from_slice(&1u16.to_le_bytes());
        d[237..253].copy_from_slice(&1_000_000_000u128.to_le_bytes());
        d[253..269].copy_from_slice(&(1u128 << 64).to_le_bytes());
        d[269..273].copy_from_slice(&0i32.to_le_bytes());
        d
    }

    fn ctx(input_is_a: bool) -> SwapContext {
        let pool = Pubkey::new_unique();
        let program = pk(cb_dex::raydium_clmm::PROGRAM_ID);
        let starts =
            crate::pda::tick_array_starts(0, 1, crate::pda::RAYDIUM_TICKS_PER_ARRAY, input_is_a);
        SwapContext {
            owner: Pubkey::new_unique(),
            pool,
            user_source: Pubkey::new_unique(),
            user_dest: Pubkey::new_unique(),
            amount_in: 500_000,
            min_amount_out: 499_000,
            input_is_a,
            tick_arrays: [
                crate::pda::raydium_tick_array(&pool, starts[0], &program),
                crate::pda::raydium_tick_array(&pool, starts[1], &program),
                crate::pda::raydium_tick_array(&pool, starts[2], &program),
            ],
        }
    }

    /// Raydium is positional by role, so source and destination never move — but the
    /// vaults must, or the pool pays itself out of the wrong side.
    #[test]
    fn the_vaults_swap_with_the_direction_and_the_user_accounts_do_not() {
        let tp = pk(programs::SPL_TOKEN);
        let (v0, v1) = (Pubkey::new_from_array([0x44; 32]), Pubkey::new_from_array([0x55; 32]));

        let c = ctx(true);
        let a = swap(&c, &pool(), tp, BitmapPolicy::Omit).unwrap();
        assert_eq!(a.accounts[3].pubkey, c.user_source);
        assert_eq!(a.accounts[4].pubkey, c.user_dest);
        assert_eq!(a.accounts[5].pubkey, v0, "spending token 0 must draw from vault 0");
        assert_eq!(a.accounts[6].pubkey, v1);

        let c = ctx(false);
        let b = swap(&c, &pool(), tp, BitmapPolicy::Omit).unwrap();
        assert_eq!(b.accounts[3].pubkey, c.user_source, "source stays at 3 in both directions");
        assert_eq!(b.accounts[5].pubkey, v1, "spending token 1 must draw from vault 1");
        assert_eq!(b.accounts[6].pubkey, v0);
    }

    #[test]
    fn the_argument_buffer_has_no_direction_flag_unlike_orca() {
        let ix = swap(&ctx(true), &pool(), pk(programs::SPL_TOKEN), BitmapPolicy::Omit).unwrap();
        // 8 discriminator + 8 + 8 + 16 + 1. One byte shorter than Orca's, because
        // Raydium infers the direction from which vault is named as the input.
        assert_eq!(ix.data.len(), 41);
        assert_eq!(&ix.data[..8], &crate::encode::anchor_discriminator("swap"));
        assert_eq!(&ix.data[8..16], &500_000u64.to_le_bytes());
        assert_eq!(&ix.data[16..24], &499_000u64.to_le_bytes());
        assert_eq!(*ix.data.last().unwrap(), 1, "is_base_input must be set");
    }

    #[test]
    fn the_bitmap_policy_changes_exactly_one_account() {
        let tp = pk(programs::SPL_TOKEN);
        let c = ctx(true);
        let with = swap(&c, &pool(), tp, BitmapPolicy::Include).unwrap();
        let without = swap(&c, &pool(), tp, BitmapPolicy::Omit).unwrap();

        assert_eq!(with.accounts.len(), without.accounts.len() + 1);
        assert_eq!(with.data, without.data, "the policy must not change the arguments");
        // The named accounts are identical; only the remaining list differs.
        assert_eq!(with.accounts[..10], without.accounts[..10]);
        assert_eq!(
            with.accounts[10].pubkey,
            raydium_bitmap_extension(&c.pool, &pk(cb_dex::raydium_clmm::PROGRAM_ID))
        );
    }

    #[test]
    fn exactly_one_account_signs_and_it_is_the_payer() {
        let c = ctx(true);
        let ix = swap(&c, &pool(), pk(programs::SPL_TOKEN), BitmapPolicy::Omit).unwrap();
        let signers: Vec<_> = ix.accounts.iter().filter(|a| a.is_signer).collect();
        assert_eq!(signers.len(), 1);
        assert_eq!(signers[0].pubkey, c.owner);
        assert!(!ix.accounts[1].is_writable, "amm_config is read only");
        assert!(!ix.accounts[8].is_writable, "the token program is read only");
        assert!(ix.accounts[7].is_writable, "the observation account is written");
    }

    /// The first array is a named account and the rest are remaining accounts, so a
    /// bitmap inserted between them must not displace the first.
    #[test]
    fn the_first_array_is_named_and_the_rest_follow_the_bitmap() {
        let c = ctx(true);
        let omit = swap(&c, &pool(), pk(programs::SPL_TOKEN), BitmapPolicy::Omit).unwrap();
        assert_eq!(omit.accounts[9].pubkey, c.tick_arrays[0]);
        assert_eq!(omit.accounts[10].pubkey, c.tick_arrays[1]);
        assert_eq!(omit.accounts[11].pubkey, c.tick_arrays[2]);

        let inc = swap(&c, &pool(), pk(programs::SPL_TOKEN), BitmapPolicy::Include).unwrap();
        assert_eq!(inc.accounts[9].pubkey, c.tick_arrays[0], "the named array must not move");
        assert_eq!(inc.accounts[11].pubkey, c.tick_arrays[1], "the bitmap goes before the rest");
        assert_eq!(inc.accounts[12].pubkey, c.tick_arrays[2]);
    }

    #[test]
    fn a_swap_with_no_floor_or_no_amount_is_refused() {
        let tp = pk(programs::SPL_TOKEN);
        let mut c = ctx(true);
        c.amount_in = 0;
        assert!(swap(&c, &pool(), tp, BitmapPolicy::Omit).is_err());
        let mut c = ctx(true);
        c.min_amount_out = 0;
        assert!(swap(&c, &pool(), tp, BitmapPolicy::Omit).is_err());
    }
}
