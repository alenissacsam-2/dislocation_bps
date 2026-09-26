//! PumpSwap `sell` and `buy_exact_quote_in`.
//!
//! # Which instruction, which direction
//!
//! Every pool is base/quote. Spending the base token is `sell(base_amount_in,
//! min_quote_amount_out)`; spending the quote token is `buy_exact_quote_in(
//! spendable_quote_in, min_base_amount_out, track_volume)`. Both are exact-input with an
//! output floor, which is the shape every other hop in a route has. Plain `buy` is
//! exact-output and is not used.
//!
//! # The accounts are the on-chain IDL's, plus what the IDL does not list
//!
//! The fixed accounts and their order come from the program's own Anchor IDL, read from
//! chain on 2026-09-26. Live calls then carry two more, as remaining accounts: one of
//! the eight buyback fee recipients in `GlobalConfig` (read-only) and its token account
//! for the quote mint (writable). Both were identified from landed transactions:
//! each recipient named was one of the eight, and each token account belonged to it.
//!
//! A pump.fun pool needs one more, first: its `pool_v2` account, a PDA of
//! `["pool-v2", base_mint]` that need not exist. The IDL does not list it; a simulated
//! swap on a pump pool without it fails with `InvalidPoolV2` ("pool_v2 remaining account
//! is missing or invalid"), and the seeds were recovered by matching the unexplained
//! account in a landed sale. Any other pool must **not** be given it: the program then
//! reads it as the buyback recipient and fails with `BuybackFeeRecipientNotAuthorized`.
//! Which kind a pool is comes from [`is_pump_pool`], the same test its fees depend on.
//!
//! # A one-time account
//!
//! `buy_exact_quote_in` names the buyer's `user_volume_accumulator`, a PDA the program
//! creates on first use at the payer's expense. Created inside an arbitrage, that rent
//! would come out of the balance the trade's profit is measured in, so it is created
//! once, beforehand, by [`init_user_volume_accumulator`].

use super::SwapContext;
use crate::encode::{pk, programs, to_pubkey, Args};
use crate::pda::{anchor_event_authority, associated_token_address};
use anyhow::{ensure, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

/// The fee recipients a swap names, chosen from `GlobalConfig`. Any of the eight in each
/// list is accepted; the caller reads the config and picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpFeeRecipients {
    pub protocol: Pubkey,
    pub buyback: Pubkey,
}

/// The program's `fee_config`: a PDA of the fee program, seeded with PumpSwap's id.
#[must_use]
pub fn fee_config() -> Pubkey {
    let program = pk(cb_dex::pumpswap::PROGRAM_ID);
    Pubkey::find_program_address(&[b"fee_config", program.as_ref()], &pk(cb_dex::pumpswap::FEE_PROGRAM_ID)).0
}

/// Whether pump.fun's migration created this pool: its creator is pump.fun's
/// `["pool-authority", base_mint]` PDA. Such a pool pays the market-cap fee tiers and
/// takes the `pool_v2` remaining account; any other pool pays the flat fee and does not.
#[must_use]
pub fn is_pump_pool(pool: &cb_dex::pumpswap::PumpPool) -> bool {
    let base_mint = to_pubkey(&pool.base_mint);
    to_pubkey(&pool.creator)
        == Pubkey::find_program_address(
            &[b"pool-authority", base_mint.as_ref()],
            &pk(cb_dex::pumpswap::PUMP_FUN_PROGRAM_ID),
        )
        .0
}

/// The pool's `pool_v2` PDA, which a pump pool's swaps name first among their
/// remaining accounts.
#[must_use]
pub fn pool_v2(base_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool-v2", base_mint.as_ref()], &pk(cb_dex::pumpswap::PROGRAM_ID)).0
}

/// The buyer's volume accumulator PDA.
#[must_use]
pub fn user_volume_accumulator(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"user_volume_accumulator", user.as_ref()],
        &pk(cb_dex::pumpswap::PROGRAM_ID),
    )
    .0
}

/// Create `user`'s volume accumulator, paid for by `user`.
#[must_use]
pub fn init_user_volume_accumulator(user: &Pubkey) -> Instruction {
    let program = pk(cb_dex::pumpswap::PROGRAM_ID);
    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(*user, false),
            AccountMeta::new(user_volume_accumulator(user), false),
            AccountMeta::new_readonly(pk(programs::SYSTEM), false),
            AccountMeta::new_readonly(anchor_event_authority(&program), false),
            AccountMeta::new_readonly(program, false),
        ],
        data: Args::anchor("init_user_volume_accumulator").build(),
    }
}

/// Build a PumpSwap swap. `ctx.input_is_a` means the input is the pool's base mint,
/// which makes this a sale.
///
/// # Errors
/// If the pool does not decode, an amount is zero, or no fee recipients were given.
pub fn swap(ctx: &SwapContext, pool_data: &[u8], fees: Option<PumpFeeRecipients>) -> Result<Instruction> {
    let pool = cb_dex::pumpswap::decode_pool(pool_data)?;
    let fees = fees.ok_or_else(|| {
        anyhow::anyhow!("a PumpSwap swap needs its fee recipients from GlobalConfig, and none were given")
    })?;
    ensure!(ctx.amount_in > 0, "a swap of zero is not a swap");
    ensure!(
        ctx.min_amount_out > 0,
        "PumpSwap has no price limit, so a zero floor is the whole protection missing; refusing \
         to encode one"
    );

    let program = pk(cb_dex::pumpswap::PROGRAM_ID);
    let selling = ctx.input_is_a;
    let (base_program, quote_program) = if selling {
        (ctx.input_token_program, ctx.output_token_program)
    } else {
        (ctx.output_token_program, ctx.input_token_program)
    };
    let (user_base, user_quote) =
        if selling { (ctx.user_source, ctx.user_dest) } else { (ctx.user_dest, ctx.user_source) };
    let base_mint = to_pubkey(&pool.base_mint);
    let quote_mint = to_pubkey(&pool.quote_mint);
    let creator_vault_authority = Pubkey::find_program_address(
        &[b"creator_vault", to_pubkey(&pool.coin_creator).as_ref()],
        &program,
    )
    .0;

    let mut accounts = vec![
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new(ctx.owner, true),
        AccountMeta::new_readonly(pk(cb_dex::pumpswap::GLOBAL_CONFIG), false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(user_base, false),
        AccountMeta::new(user_quote, false),
        AccountMeta::new(to_pubkey(&pool.base_vault), false),
        AccountMeta::new(to_pubkey(&pool.quote_vault), false),
        AccountMeta::new_readonly(fees.protocol, false),
        AccountMeta::new(associated_token_address(&fees.protocol, &quote_mint, &quote_program), false),
        AccountMeta::new_readonly(base_program, false),
        AccountMeta::new_readonly(quote_program, false),
        AccountMeta::new_readonly(pk(programs::SYSTEM), false),
        AccountMeta::new_readonly(pk(programs::ASSOCIATED_TOKEN), false),
        AccountMeta::new_readonly(anchor_event_authority(&program), false),
        AccountMeta::new_readonly(program, false),
        AccountMeta::new(associated_token_address(&creator_vault_authority, &quote_mint, &quote_program), false),
        AccountMeta::new_readonly(creator_vault_authority, false),
    ];
    if !selling {
        accounts.push(AccountMeta::new_readonly(
            Pubkey::find_program_address(&[b"global_volume_accumulator"], &program).0,
            false,
        ));
        accounts.push(AccountMeta::new(user_volume_accumulator(&ctx.owner), false));
    }
    accounts.push(AccountMeta::new_readonly(fee_config(), false));
    accounts.push(AccountMeta::new_readonly(pk(cb_dex::pumpswap::FEE_PROGRAM_ID), false));
    // Remaining accounts: a pump pool's pool_v2, then the buyback fee recipient and its
    // quote-mint account.
    if is_pump_pool(&pool) {
        accounts.push(AccountMeta::new_readonly(pool_v2(&base_mint), false));
    }
    accounts.push(AccountMeta::new_readonly(fees.buyback, false));
    accounts.push(AccountMeta::new(associated_token_address(&fees.buyback, &quote_mint, &quote_program), false));

    let data = if selling {
        Args::anchor("sell").u64(ctx.amount_in).u64(ctx.min_amount_out).build()
    } else {
        // `track_volume` is an OptionBool, a one-field struct: one byte, false.
        Args::anchor("buy_exact_quote_in").u64(ctx.amount_in).u64(ctx.min_amount_out).bool(false).build()
    };
    Ok(Instruction { program_id: program, accounts, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminators_match_the_on_chain_idl() {
        assert_eq!(crate::encode::anchor_discriminator("sell"), [51, 230, 133, 164, 1, 127, 131, 173]);
        assert_eq!(
            crate::encode::anchor_discriminator("buy_exact_quote_in"),
            [198, 46, 21, 82, 180, 217, 232, 112]
        );
    }

    #[test]
    fn pool_v2_is_the_account_a_landed_sale_named() {
        // Sale MbFMvBac... on 2026-09-26, base mint 3DQbzaaQ...pump.
        assert_eq!(
            pool_v2(&pk("3DQbzaaQtDpNyeKY3ZQFWbqZxr8FV1FwGMxf1WpVpump")),
            pk("8TkavjFuCKYVBtLCoQqmy2sLdh8ARYSbn6jNgb4jxJaK")
        );
    }

    #[test]
    fn the_fee_config_pda_is_the_one_live_swaps_name() {
        assert_eq!(fee_config(), pk(cb_dex::pumpswap::FEE_CONFIG));
    }

    fn pool_account() -> Vec<u8> {
        let mut v = vec![0u8; cb_dex::pumpswap::POOL_MIN_LEN];
        v[43..75].copy_from_slice(&[11u8; 32]);
        v[75..107].copy_from_slice(&pk(programs::WSOL_MINT).to_bytes());
        v
    }

    fn ctx(input_is_a: bool) -> SwapContext {
        SwapContext {
            owner: Pubkey::new_unique(),
            pool: Pubkey::new_unique(),
            user_source: Pubkey::new_unique(),
            user_dest: Pubkey::new_unique(),
            amount_in: 1_000,
            min_amount_out: 1,
            input_is_a,
            input_token_program: pk(programs::SPL_TOKEN),
            output_token_program: pk(programs::SPL_TOKEN),
            tick_arrays: [Pubkey::default(); crate::pda::TICK_ARRAYS_PER_SWAP],
        }
    }

    #[test]
    fn a_sale_and_a_purchase_carry_the_counts_live_calls_carry() {
        let fees = Some(PumpFeeRecipients { protocol: Pubkey::new_unique(), buyback: Pubkey::new_unique() });
        // Not a pump pool (its creator is not pump.fun's authority): 21 fixed + 2
        // remaining for a sale, 23 + 2 for a purchase.
        assert_eq!(swap(&ctx(true), &pool_account(), fees).unwrap().accounts.len(), 23);
        let buy = swap(&ctx(false), &pool_account(), fees).unwrap();
        assert_eq!(buy.accounts.len(), 25);
        // A pump pool takes pool_v2 as well: 24 and 26, as landed pump-pool calls carry.
        let mut pump = pool_account();
        let authority = Pubkey::find_program_address(
            &[b"pool-authority", &[11u8; 32]],
            &pk(cb_dex::pumpswap::PUMP_FUN_PROGRAM_ID),
        )
        .0;
        pump[11..43].copy_from_slice(&authority.to_bytes());
        assert!(is_pump_pool(&cb_dex::pumpswap::decode_pool(&pump).unwrap()));
        assert_eq!(swap(&ctx(true), &pump, fees).unwrap().accounts.len(), 24);
        assert_eq!(swap(&ctx(false), &pump, fees).unwrap().accounts.len(), 26);
        assert_eq!(buy.data.len(), 8 + 8 + 8 + 1);
        assert!(swap(&ctx(true), &pool_account(), None).is_err(), "no recipients, no swap");
    }
}
