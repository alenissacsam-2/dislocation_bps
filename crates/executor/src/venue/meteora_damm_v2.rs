//! Meteora DAMM v2 (`cp-amm`) `swap`.
//!
//! Fourteen accounts, from the program's on-chain Anchor IDL read on 2026-09-26: the
//! fixed pool authority, the pool, the user's input and output token accounts, both
//! vaults and both mints **in the pool's own A/B order**, the payer, both token programs
//! (again in A/B order), an optional referral account (absent: the program's own id),
//! the event authority and the program. `amount_in` and `minimum_amount_out` follow the
//! discriminator; the swap has no price limit, so the floor is its whole protection.

use super::SwapContext;
use crate::encode::{pk, to_pubkey, Args};
use crate::pda::anchor_event_authority;
use anyhow::{ensure, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};

/// The program's one pool authority, a constant in the IDL.
pub const POOL_AUTHORITY: &str = "HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC";

/// Build a DAMM v2 `swap`. `ctx.input_is_a` means the input is the pool's token A.
///
/// # Errors
/// If the pool account does not decode or either amount is zero.
pub fn swap(ctx: &SwapContext, pool_data: &[u8]) -> Result<Instruction> {
    let p = cb_dex::meteora_damm_v2::decode_layout(pool_data)?;
    ensure!(ctx.amount_in > 0, "a swap of zero is not a swap");
    ensure!(
        ctx.min_amount_out > 0,
        "DAMM v2 has no price limit, so a zero floor is the whole protection missing; refusing \
         to encode one"
    );
    let program = pk(cb_dex::meteora_damm_v2::PROGRAM_ID);
    let (token_a_program, token_b_program) = if ctx.input_is_a {
        (ctx.input_token_program, ctx.output_token_program)
    } else {
        (ctx.output_token_program, ctx.input_token_program)
    };
    let accounts = vec![
        AccountMeta::new_readonly(pk(POOL_AUTHORITY), false),
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new(ctx.user_source, false),
        AccountMeta::new(ctx.user_dest, false),
        AccountMeta::new(to_pubkey(&p.vault_a), false),
        AccountMeta::new(to_pubkey(&p.vault_b), false),
        AccountMeta::new_readonly(to_pubkey(&p.mint_a), false),
        AccountMeta::new_readonly(to_pubkey(&p.mint_b), false),
        AccountMeta::new_readonly(ctx.owner, true),
        AccountMeta::new_readonly(token_a_program, false),
        AccountMeta::new_readonly(token_b_program, false),
        // No referral: an absent optional account is the program's own id.
        AccountMeta::new_readonly(program, false),
        AccountMeta::new_readonly(anchor_event_authority(&program), false),
        AccountMeta::new_readonly(program, false),
    ];
    let data = Args::anchor("swap").u64(ctx.amount_in).u64(ctx.min_amount_out).build();
    Ok(Instruction { program_id: program, accounts, data })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_discriminator_matches_the_on_chain_idl() {
        assert_eq!(crate::encode::anchor_discriminator("swap"), [248, 198, 158, 145, 225, 117, 135, 200]);
    }
}
