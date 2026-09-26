//! Raydium CP-Swap (CPMM) `swap_base_input`.
//!
//! The program publishes no on-chain IDL; the account list is Raydium's open-source
//! `raydium-cp-swap` `Swap` context, and it is checked by simulation on live pools
//! (`cb-check-cpmm`), not asserted: payer, the vault authority PDA
//! (`["vault_and_lp_mint_auth_seed"]`), the pool's config, the pool, the user's input
//! and output token accounts, the input and output vaults, the input and output token
//! programs, the input and output mints, and the pool's observation account. Arguments
//! are `amount_in` and `minimum_amount_out`; there is no price limit.

use super::SwapContext;
use crate::encode::{pk, to_pubkey, Args};
use anyhow::{ensure, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

/// The vault and LP-mint authority every CP-Swap pool shares.
#[must_use]
pub fn authority() -> Pubkey {
    Pubkey::find_program_address(&[b"vault_and_lp_mint_auth_seed"], &pk(cb_dex::raydium_cpmm::PROGRAM_ID)).0
}

/// Build a CP-Swap `swap_base_input`. `ctx.input_is_a` means the input is token 0.
///
/// # Errors
/// If the pool account does not decode or either amount is zero.
pub fn swap(ctx: &SwapContext, pool_data: &[u8]) -> Result<Instruction> {
    let p = cb_dex::raydium_cpmm::decode(pool_data)?;
    ensure!(ctx.amount_in > 0, "a swap of zero is not a swap");
    ensure!(
        ctx.min_amount_out > 0,
        "CP-Swap has no price limit, so a zero floor is the whole protection missing; refusing \
         to encode one"
    );
    let (input_vault, output_vault, input_mint, output_mint) = if ctx.input_is_a {
        (p.vault_0, p.vault_1, p.mint_0, p.mint_1)
    } else {
        (p.vault_1, p.vault_0, p.mint_1, p.mint_0)
    };
    let accounts = vec![
        AccountMeta::new_readonly(ctx.owner, true),
        AccountMeta::new_readonly(authority(), false),
        AccountMeta::new_readonly(to_pubkey(&p.amm_config), false),
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new(ctx.user_source, false),
        AccountMeta::new(ctx.user_dest, false),
        AccountMeta::new(to_pubkey(&input_vault), false),
        AccountMeta::new(to_pubkey(&output_vault), false),
        AccountMeta::new_readonly(ctx.input_token_program, false),
        AccountMeta::new_readonly(ctx.output_token_program, false),
        AccountMeta::new_readonly(to_pubkey(&input_mint), false),
        AccountMeta::new_readonly(to_pubkey(&output_mint), false),
        AccountMeta::new(to_pubkey(&p.observation), false),
    ];
    let data = Args::anchor("swap_base_input").u64(ctx.amount_in).u64(ctx.min_amount_out).build();
    Ok(Instruction { program_id: pk(cb_dex::raydium_cpmm::PROGRAM_ID), accounts, data })
}
