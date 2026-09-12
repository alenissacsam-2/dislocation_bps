//! Meteora DLMM `swap2`.
//!
//! # Why `swap2` and not `swap`
//!
//! The program ships both. `swap` takes one token program and no mints; `swap2` takes
//! both programs, both mints, the memo program, and a trailing `RemainingAccountsInfo`.
//! Sampling live calls, `swap2` is what real integrators use, including on pools where
//! both mints are classic SPL — so there is no classic-only path to fall back to and no
//! reason to maintain one. One instruction, one set of accounts, one thing to be wrong
//! about.
//!
//! # The account order is measured, and one position is counter-intuitive
//!
//! Every `swap2` call sampled passes the same sixteen fixed accounts in the same order,
//! listed in [`cb_dex::meteora_dlmm`]. The position worth stating twice is 11 and 12:
//! they are `token_x_program` and `token_y_program` **in the pool's own x/y order**.
//!
//! Raydium's `swap_v2` orders the same pair differently — classic first, Token-2022
//! second, whichever mint owns which — so the obvious assumption, that two venues
//! solving the same problem in the same year solve it the same way, is false. The
//! evidence is a call whose token X is classic and whose token Y is Token-2022: it
//! passes classic at 11 and Token-2022 at 12. Had the order been fixed by program, it
//! would have been the other way round.
//!
//! # There is no price limit here either
//!
//! Like Raydium v4 and unlike both concentrated venues, `swap2` takes no
//! `sqrt_price_limit`. `min_amount_out` is the entire protection on the instruction, so
//! a zero floor is refused rather than treated as "no opinion".
//!
//! # What is deliberately refused
//!
//! A pool whose active bin array sits outside the inline bitmap's range needs the
//! `bin_array_bitmap_extension` account, which is a separate PDA that may or may not
//! have been created. Passing a derived address for an account that does not exist fails
//! on chain for a reason that looks nothing like the cause, so such a pool is refused by
//! name instead. Every pool in this registry sits well inside the range — the SOL/USDC
//! pool's active array is index −327 against a limit of 512 — so the branch costs
//! nothing today and says so clearly if that ever changes.

use super::SwapContext;
use crate::encode::{pk, programs, to_pubkey, Args};
use crate::pda::{anchor_event_authority, meteora_bin_array};
use anyhow::{ensure, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

/// Bin array indices the pool account's own bitmap covers, either side of zero.
///
/// Meteora's `BIN_ARRAY_BITMAP_SIZE`. An index outside `-512..=511` lives in the
/// extension account instead.
pub const BITMAP_INLINE_RANGE: i64 = 512;

/// Build a `swap2` instruction against a Meteora DLMM pool.
///
/// `ctx.tick_arrays` carries the **bin** arrays, in traversal order — the same slot the
/// concentrated venues use for tick arrays, because both venues need exactly three
/// ordered auxiliary accounts and giving this encoder its own context type would force
/// every caller to know the venue before it could describe the swap.
///
/// # Errors
/// If the pool account does not decode, if either amount is zero, or if the pool needs a
/// bitmap extension this encoder will not guess at.
pub fn swap(ctx: &SwapContext, pool_data: &[u8]) -> Result<Instruction> {
    let pair = cb_dex::meteora_dlmm::decode(pool_data)?;
    ensure!(ctx.amount_in > 0, "a swap of zero is not a swap");
    ensure!(
        ctx.min_amount_out > 0,
        "swap2 has no price limit, so a zero floor is the whole protection missing; refusing \
         to encode one"
    );

    let program = pk(cb_dex::meteora_dlmm::PROGRAM_ID);
    let active_array = cb_dex::meteora_dlmm::bin_array_index(pair.active_id);
    ensure!(
        (-BITMAP_INLINE_RANGE..BITMAP_INLINE_RANGE).contains(&active_array),
        "pool {} sits at bin array {active_array}, outside the ±{BITMAP_INLINE_RANGE} its own \
         bitmap covers — swapping it needs the bitmap extension account, which this encoder \
         will not name without knowing it exists",
        ctx.pool
    );

    // Positions 11 and 12 are the pool's x and y token programs, so the context's
    // input/output pair has to be rotated into pool order. `input_is_a` means the input
    // mint is token X.
    let (token_x_program, token_y_program) = if ctx.input_is_a {
        (ctx.input_token_program, ctx.output_token_program)
    } else {
        (ctx.output_token_program, ctx.input_token_program)
    };

    // The two optional accounts, absent. The program's own convention is that an absent
    // optional account is its own id, and it is read-only there — passing it writable
    // would be asking to mutate an executable account.
    let absent = program;

    let mut accounts = vec![
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new_readonly(absent, false), // bin_array_bitmap_extension
        AccountMeta::new(to_pubkey(&pair.reserve_x), false),
        AccountMeta::new(to_pubkey(&pair.reserve_y), false),
        AccountMeta::new(ctx.user_source, false),
        AccountMeta::new(ctx.user_dest, false),
        AccountMeta::new_readonly(to_pubkey(&pair.token_x_mint), false),
        AccountMeta::new_readonly(to_pubkey(&pair.token_y_mint), false),
        AccountMeta::new(to_pubkey(&pair.oracle), false),
        AccountMeta::new_readonly(absent, false), // host_fee_in
        AccountMeta::new_readonly(ctx.owner, true),
        AccountMeta::new_readonly(token_x_program, false),
        AccountMeta::new_readonly(token_y_program, false),
        AccountMeta::new_readonly(pk(programs::MEMO), false),
        AccountMeta::new_readonly(anchor_event_authority(&program), false),
        AccountMeta::new_readonly(program, false),
    ];
    // The bins the swap will walk, written to as it crosses them.
    accounts.extend(ctx.tick_arrays.iter().map(|a| AccountMeta::new(*a, false)));

    // `amount_in`, `min_amount_out`, then a `RemainingAccountsInfo` whose empty form is a
    // zero-length borsh vector: four zero bytes. Twenty-eight bytes in total, which is
    // what the shortest real call on chain carries.
    let data = Args::anchor("swap2")
        .u64(ctx.amount_in)
        .u64(ctx.min_amount_out)
        .u32(0)
        .build();

    Ok(Instruction { program_id: program, accounts, data })
}

/// The three bin arrays a swap from `active_id` will walk, derived.
///
/// Exposed so a caller can name them without reimplementing either the direction rule or
/// the seed layout. `price_falling` is true when the swap spends token X.
#[must_use]
pub fn bin_arrays_for(
    pool: &Pubkey,
    active_id: i32,
    price_falling: bool,
) -> [Pubkey; crate::pda::BIN_ARRAYS_PER_SWAP] {
    let program = pk(cb_dex::meteora_dlmm::PROGRAM_ID);
    let mut out = [*pool; crate::pda::BIN_ARRAYS_PER_SWAP];
    for (slot, index) in
        out.iter_mut().zip(crate::pda::meteora_bin_array_indices(active_id, price_falling))
    {
        *slot = meteora_bin_array(pool, index, &program);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_core::types::Pubkey32;

    /// The real SOL/USDC pool account, so the accounts this encoder names are the ones
    /// the chain would resolve.
    fn pool_bytes() -> Vec<u8> {
        let s = include_str!("../../../dex/tests/fixtures/meteora_dlmm_pair.b64");
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

    fn ctx(input_is_a: bool, in_prog: &str, out_prog: &str) -> SwapContext {
        SwapContext {
            owner: Pubkey::new_unique(),
            pool: pk("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR"),
            user_source: Pubkey::new_unique(),
            user_dest: Pubkey::new_unique(),
            amount_in: 1_000_000,
            min_amount_out: 99,
            input_is_a,
            input_token_program: pk(in_prog),
            output_token_program: pk(out_prog),
            tick_arrays: [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()],
        }
    }

    fn raw(k: &Pubkey) -> Pubkey32 {
        k.to_bytes()
    }

    #[test]
    fn the_fixed_accounts_are_the_ones_every_real_call_passes() {
        let c = ctx(true, programs::SPL_TOKEN, programs::SPL_TOKEN);
        let ix = swap(&c, &pool_bytes()).unwrap();
        let program = pk(cb_dex::meteora_dlmm::PROGRAM_ID);
        let pair = cb_dex::meteora_dlmm::decode(&pool_bytes()).unwrap();

        assert_eq!(ix.program_id, program);
        assert_eq!(ix.accounts.len(), 16 + 3);
        assert_eq!(ix.accounts[0].pubkey, c.pool);
        assert_eq!(ix.accounts[1].pubkey, program, "absent bitmap extension is the program id");
        assert_eq!(raw(&ix.accounts[2].pubkey), pair.reserve_x);
        assert_eq!(raw(&ix.accounts[3].pubkey), pair.reserve_y);
        assert_eq!(ix.accounts[4].pubkey, c.user_source);
        assert_eq!(ix.accounts[5].pubkey, c.user_dest);
        assert_eq!(raw(&ix.accounts[6].pubkey), pair.token_x_mint);
        assert_eq!(raw(&ix.accounts[7].pubkey), pair.token_y_mint);
        assert_eq!(raw(&ix.accounts[8].pubkey), pair.oracle);
        assert_eq!(ix.accounts[9].pubkey, program, "absent host fee is the program id");
        assert_eq!(ix.accounts[10].pubkey, c.owner);
        assert_eq!(ix.accounts[13].pubkey, pk(programs::MEMO));
        assert_eq!(
            ix.accounts[14].pubkey,
            pk("D1ZN9Wj1fRSUQfCjhvnu1hqDMT7hzjzBBpi12nVniYD6"),
            "the event authority every real call passes"
        );
        assert_eq!(ix.accounts[15].pubkey, program);
        for (i, a) in c.tick_arrays.iter().enumerate() {
            assert_eq!(ix.accounts[16 + i].pubkey, *a, "bin array {i} is out of order");
        }
    }

    /// Only the signer signs, and the accounts a swap mutates are the pool, its two
    /// reserves, the user's two token accounts, the oracle and the bins. Marking an
    /// executable account writable is rejected by the runtime, so the two absent optional
    /// slots must stay read-only.
    #[test]
    fn exactly_the_accounts_a_swap_changes_are_writable() {
        let c = ctx(true, programs::SPL_TOKEN, programs::SPL_TOKEN);
        let ix = swap(&c, &pool_bytes()).unwrap();
        let writable: Vec<usize> =
            ix.accounts.iter().enumerate().filter(|(_, a)| a.is_writable).map(|(i, _)| i).collect();
        assert_eq!(writable, vec![0, 2, 3, 4, 5, 8, 16, 17, 18]);
        let signers: Vec<usize> =
            ix.accounts.iter().enumerate().filter(|(_, a)| a.is_signer).map(|(i, _)| i).collect();
        assert_eq!(signers, vec![10], "only the owner signs");
    }

    /// The counter-intuitive position. Whichever direction the swap runs, 11 and 12 must
    /// come out in the pool's x/y order — which for a mixed pool means the input program
    /// can land in either slot.
    #[test]
    fn the_token_programs_are_in_pool_order_and_not_classic_first() {
        let classic = pk(programs::SPL_TOKEN);
        let t22 = pk(programs::SPL_TOKEN_2022);

        // Input is token X and it is the Token-2022 one.
        let ix = swap(&ctx(true, programs::SPL_TOKEN_2022, programs::SPL_TOKEN), &pool_bytes())
            .unwrap();
        assert_eq!(ix.accounts[11].pubkey, t22, "token X's program belongs at 11");
        assert_eq!(ix.accounts[12].pubkey, classic);

        // Same pool, same mints, opposite direction: the slots must not move.
        let ix = swap(&ctx(false, programs::SPL_TOKEN, programs::SPL_TOKEN_2022), &pool_bytes())
            .unwrap();
        assert_eq!(ix.accounts[11].pubkey, t22, "direction must not reorder the programs");
        assert_eq!(ix.accounts[12].pubkey, classic);
    }

    /// Twenty-eight bytes: discriminator, two amounts, and an empty
    /// `RemainingAccountsInfo`. The shortest real call on chain is exactly this long.
    #[test]
    fn the_argument_buffer_matches_the_shortest_real_call() {
        let c = ctx(true, programs::SPL_TOKEN, programs::SPL_TOKEN);
        let ix = swap(&c, &pool_bytes()).unwrap();
        assert_eq!(ix.data.len(), 28);
        assert_eq!(
            hex(&ix.data[..8]),
            "414b3f4ceb5b5b88",
            "the discriminator is sha256(\"global:swap2\")[..8]"
        );
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), c.amount_in);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), c.min_amount_out);
        assert_eq!(&ix.data[24..], &[0, 0, 0, 0], "an empty slice vector is four zero bytes");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// `min_amount_out` is the only protection this instruction has, so zero is refused.
    #[test]
    fn a_swap_with_no_floor_is_refused_because_nothing_else_protects_it() {
        let mut c = ctx(true, programs::SPL_TOKEN, programs::SPL_TOKEN);
        c.min_amount_out = 0;
        let e = swap(&c, &pool_bytes()).unwrap_err().to_string();
        assert!(e.contains("no price limit"), "unexpected error: {e}");
        c.min_amount_out = 1;
        c.amount_in = 0;
        assert!(swap(&c, &pool_bytes()).is_err(), "a swap of zero is not a swap");
    }

    /// A pool whose bins sit beyond the inline bitmap needs an account this encoder will
    /// not guess at, and must say so by name.
    #[test]
    fn a_pool_outside_its_own_bitmap_is_refused_rather_than_guessed_at() {
        let mut data = pool_bytes();
        // An active bin far enough out that its array index passes 512, and a range wide
        // enough to still contain it.
        let far: i32 = 512 * 70 + 7;
        data[76..80].copy_from_slice(&far.to_le_bytes());
        data[24..28].copy_from_slice(&(-600_000i32).to_le_bytes());
        data[28..32].copy_from_slice(&600_000i32.to_le_bytes());
        let c = ctx(true, programs::SPL_TOKEN, programs::SPL_TOKEN);
        let e = swap(&c, &data).unwrap_err().to_string();
        assert!(e.contains("bitmap extension"), "unexpected error: {e}");
    }

    /// The derived bin arrays must be the chain's, and must walk the way the price moves.
    #[test]
    fn the_derived_bin_arrays_start_at_the_active_one() {
        let pool = pk("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR");
        let falling = bin_arrays_for(&pool, -22849, true);
        assert_eq!(falling[0], pk("DxyNRLdPkPsaV73w6qe9Ytau8siAnzBXfTFaaJD8UegD"));
        let rising = bin_arrays_for(&pool, -22849, false);
        assert_eq!(rising[0], falling[0], "both directions start where the price is");
        assert_ne!(rising[1], falling[1], "and then go opposite ways");
    }
}
