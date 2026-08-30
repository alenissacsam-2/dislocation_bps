//! Checking the encoders against mainnet, which is the only thing that can check them.
//!
//! # Why this exists
//!
//! Everything in [`crate::venue`] rests on facts about on-chain layouts and account
//! orders that cannot be established from a development machine. The project's second
//! design principle — *a new decoder is not trusted until something outside it agrees*
//! — applies with more force to an encoder, because a decoder that is wrong produces a
//! number somebody might notice, while an encoder that is wrong produces a transaction.
//!
//! # Four checks, three of which need no money and no key
//!
//! 1. **Vaults.** Read the addresses the decoder pulled out of the pool account and ask
//!    the chain what they are. A real vault is a token account, owned by a token
//!    program, whose mint is the pool's own mint. If the vault offsets were wrong these
//!    would be arbitrary bytes and would not resolve to anything, let alone to the
//!    right mint.
//!
//! 2. **Tick arrays.** Derive the array for the pool's current tick, fetch it, and read
//!    back the start index and pool address it declares about *itself*. If the seed
//!    scheme or the floor division were wrong, the derived address would be a different
//!    array — one whose declared start index does not match what we computed. This
//!    checks the arithmetic in [`crate::pda`] exactly, against the chain, for free.
//!
//! 3. **Oracle.** Confirm the derived address exists and belongs to the pool's program.
//!
//! 4. **The instruction itself.** Simulate a swap. This one is different: it is run
//!    with a throwaway key that holds nothing, so it *cannot* succeed. That is the
//!    point. A program validates its accounts before it checks a balance, so a
//!    simulation that gets as far as complaining about funds has already accepted the
//!    discriminator, the account count, the account order, every seed constraint and
//!    every ownership constraint. Reaching "insufficient funds" is the pass condition.
//!
//! The throwaway key matters beyond convenience: verification never has access to the
//! operator's wallet, so running it can neither spend nor expose anything.

use anyhow::Result;
use cb_core::types::Dex;

/// What a single check concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The chain agreed.
    Pass,
    /// The chain disagreed. Something in the encoder is wrong.
    Fail,
    /// Could not be determined — usually a missing account that is legitimately
    /// allowed to be missing. Not a pass.
    Inconclusive,
}

impl Verdict {
    #[must_use]
    pub fn mark(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "FAIL",
            Verdict::Inconclusive => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct PoolReport {
    pub address: String,
    pub label: String,
    pub dex: Dex,
    pub checks: Vec<Check>,
}

impl PoolReport {
    #[must_use]
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.verdict == Verdict::Fail)
    }

    #[must_use]
    pub fn all_passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.verdict == Verdict::Pass)
    }
}

/// The header an Orca tick array declares about itself.
///
/// Layout: an 8-byte discriminator, the start index, 88 ticks of 113 bytes, then the
/// whirlpool it belongs to — 9988 bytes in total, which is what pins the trailing
/// offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrcaTickArrayHeader {
    pub start_tick_index: i32,
    pub whirlpool: [u8; 32],
}

pub const ORCA_TICK_ARRAY_LEN: usize = 9988;

/// # Errors
/// If the account is not the right length to be a tick array.
pub fn orca_tick_array_header(data: &[u8]) -> Result<OrcaTickArrayHeader> {
    anyhow::ensure!(
        data.len() >= ORCA_TICK_ARRAY_LEN,
        "orca tick array is {} bytes, need {ORCA_TICK_ARRAY_LEN}",
        data.len()
    );
    let start = i32::from_le_bytes(data[8..12].try_into()?);
    let mut whirlpool = [0u8; 32];
    whirlpool.copy_from_slice(&data[ORCA_TICK_ARRAY_LEN - 32..ORCA_TICK_ARRAY_LEN]);
    Ok(OrcaTickArrayHeader { start_tick_index: start, whirlpool })
}

/// The header a Raydium CLMM tick array declares about itself.
///
/// Layout puts the pool first and the start index after it, the opposite of Orca.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaydiumTickArrayHeader {
    pub pool: [u8; 32],
    pub start_tick_index: i32,
}

/// Enough of the account to read the header; the full account is larger.
pub const RAYDIUM_TICK_ARRAY_MIN: usize = 44;

/// # Errors
/// If the account is too short to carry a header.
pub fn raydium_tick_array_header(data: &[u8]) -> Result<RaydiumTickArrayHeader> {
    anyhow::ensure!(
        data.len() >= RAYDIUM_TICK_ARRAY_MIN,
        "raydium tick array is {} bytes, need at least {RAYDIUM_TICK_ARRAY_MIN}",
        data.len()
    );
    let mut pool = [0u8; 32];
    pool.copy_from_slice(&data[8..40]);
    let start = i32::from_le_bytes(data[40..44].try_into()?);
    Ok(RaydiumTickArrayHeader { pool, start_tick_index: start })
}

/// The mint an SPL token account holds, read from the 165-byte layout.
///
/// # Errors
/// If the account is not long enough to be a token account.
pub fn token_account_mint(data: &[u8]) -> Result<[u8; 32]> {
    anyhow::ensure!(data.len() >= 165, "not a token account: {} bytes", data.len());
    let mut mint = [0u8; 32];
    mint.copy_from_slice(&data[..32]);
    Ok(mint)
}

/// Decide what a simulation error says about the *encoding*, as opposed to about the
/// throwaway key's empty balance.
///
/// # The reasoning
///
/// A Solana program validates its account list before it executes. Anchor's constraint
/// errors — wrong seeds, wrong owner, wrong discriminator, missing account — all fire
/// during that phase, so seeing one means the encoding is wrong. A balance complaint
/// fires afterwards, which means everything before it was accepted. That is why an
/// error is the pass condition here and why the *kind* of error is the whole signal.
#[must_use]
pub fn classify(err: &str, logs: &[String]) -> (Verdict, String) {
    let hay = format!("{err} {}", logs.join(" ")).to_lowercase();

    // A transaction-level rejection produces no logs at all, because the runtime never
    // loaded the program. That is a different fact from a program rejecting an account,
    // and conflating the two sends you looking inside an encoder for a problem that is
    // actually a missing fee payer or an uncreated token account.
    if logs.is_empty() && hay.contains("accountnotfound") {
        return (
            Verdict::Inconclusive,
            "the runtime rejected the transaction before running it: an address in the \
             list does not exist on chain. Usually the payer or its token accounts, not \
             the encoder — rerun with --as <a funded address that holds both mints>"
                .to_string(),
        );
    }

    // Reaching a funds or liquidity complaint means every account was accepted.
    const ACCEPTED: &[&str] = &[
        "insufficient funds",
        "insufficient lamports",
        "insufficient liquidity",
        "0x1770",   // Anchor user error 6000, typically a slippage/amount guard
        "amountoutbelowminimum",
        "zerotradableamount",
        "tooliltletickarrays",
        "custom program error: 0x1",
    ];
    // These all fire during account validation, before any balance is consulted.
    const REJECTED: &[(&str, &str)] = &[
        ("instructionfallbacknotfound", "the discriminator matched no method"),
        ("0x65", "the discriminator matched no method"),
        ("accountdiscriminatormismatch", "an account is not the type the program expected"),
        ("constraintseeds", "a derived address is wrong"),
        ("0x7d6", "a derived address is wrong (ConstraintSeeds)"),
        ("accountownedbywrongprogram", "an account belongs to the wrong program"),
        ("accountnotinitialized", "a named account does not exist"),
        ("accountnotfound", "a named account does not exist"),
        ("notenoughaccountkeys", "too few accounts in the list"),
        ("invalidaccountdata", "an account is not the shape the program expected"),
        ("could not create program address", "a seed derivation is wrong"),
    ];

    for (needle, why) in REJECTED {
        if hay.contains(needle) {
            return (Verdict::Fail, (*why).to_string());
        }
    }
    for needle in ACCEPTED {
        if hay.contains(needle) {
            return (
                Verdict::Pass,
                "accounts and discriminator accepted; stopped on the empty test balance"
                    .to_string(),
            );
        }
    }
    (Verdict::Inconclusive, format!("unrecognised failure: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_orca_tick_array_header_reads_from_both_ends_of_the_account() {
        let mut d = vec![0u8; ORCA_TICK_ARRAY_LEN];
        d[8..12].copy_from_slice(&(-5632i32).to_le_bytes());
        d[ORCA_TICK_ARRAY_LEN - 32..].copy_from_slice(&[0xAB; 32]);
        let h = orca_tick_array_header(&d).unwrap();
        assert_eq!(h.start_tick_index, -5632);
        assert_eq!(h.whirlpool, [0xAB; 32]);
    }

    /// The two venues put the pool and the start index in opposite orders. Reading one
    /// with the other's layout yields a start index that is part of a pubkey.
    #[test]
    fn raydium_puts_the_pool_first_and_orca_puts_it_last() {
        let mut d = vec![0u8; 10_240];
        d[8..40].copy_from_slice(&[0xCD; 32]);
        d[40..44].copy_from_slice(&(120i32).to_le_bytes());
        let h = raydium_tick_array_header(&d).unwrap();
        assert_eq!(h.pool, [0xCD; 32]);
        assert_eq!(h.start_tick_index, 120);

        // Orca's reader applied to the same bytes must not agree, or the two layouts
        // have been conflated.
        let orca = orca_tick_array_header(&d).unwrap();
        assert_ne!(orca.start_tick_index, h.start_tick_index);
    }

    #[test]
    fn short_accounts_are_refused_rather_than_read_out_of_bounds() {
        assert!(orca_tick_array_header(&[0u8; 100]).is_err());
        assert!(raydium_tick_array_header(&[0u8; 20]).is_err());
        assert!(token_account_mint(&[0u8; 10]).is_err());
    }

    #[test]
    fn a_token_accounts_mint_is_its_first_field() {
        let mut d = vec![0u8; 165];
        d[..32].copy_from_slice(&[0xEE; 32]);
        assert_eq!(token_account_mint(&d).unwrap(), [0xEE; 32]);
    }

    /// The classifier is the whole verification result, so its two directions are
    /// asserted separately and by the error strings the programs actually emit.
    #[test]
    fn a_funds_complaint_means_the_encoding_was_accepted() {
        for e in [
            "Transfer: insufficient lamports 0, need 2039280",
            "Error: insufficient funds",
            "custom program error: 0x1",
        ] {
            let (v, _) = classify(e, &[]);
            assert_eq!(v, Verdict::Pass, "{e} should read as accepted");
        }
    }

    #[test]
    fn an_account_validation_complaint_means_the_encoding_is_wrong() {
        for e in [
            "Error: InstructionFallbackNotFound",
            "AnchorError caused by account: tick_array_0. Error Code: ConstraintSeeds",
            "Program log: AccountOwnedByWrongProgram",
            "Error processing Instruction 0: NotEnoughAccountKeys",
        ] {
            let (v, why) = classify(e, &[]);
            assert_eq!(v, Verdict::Fail, "{e} should read as rejected");
            assert!(!why.is_empty());
        }
    }

    /// A validation error must win over a funds error appearing in the same logs, or a
    /// wrong account list is reported as a pass because something later mentioned
    /// funds.
    #[test]
    fn a_validation_error_outranks_a_funds_error_in_the_same_logs() {
        let logs = vec![
            "Program log: insufficient funds".to_string(),
            "Program log: AnchorError ConstraintSeeds".to_string(),
        ];
        assert_eq!(classify("", &logs).0, Verdict::Fail);
    }

    /// Anything unrecognised must not be reported as a pass.
    #[test]
    fn an_unknown_error_is_inconclusive_rather_than_a_pass() {
        let (v, detail) = classify("BlockhashNotFound", &[]);
        assert_eq!(v, Verdict::Inconclusive);
        assert!(detail.contains("BlockhashNotFound"));
    }

    #[test]
    fn a_report_with_any_failure_is_a_failure() {
        let mut r = PoolReport {
            address: "x".into(),
            label: "y".into(),
            dex: Dex::OrcaWhirlpool,
            checks: vec![Check { name: "a", verdict: Verdict::Pass, detail: String::new() }],
        };
        assert!(r.all_passed() && !r.failed());
        r.checks.push(Check { name: "b", verdict: Verdict::Fail, detail: String::new() });
        assert!(r.failed() && !r.all_passed());
        // An inconclusive check is not a pass either.
        let inconclusive = PoolReport {
            checks: vec![Check { name: "c", verdict: Verdict::Inconclusive, detail: String::new() }],
            ..r.clone()
        };
        assert!(!inconclusive.all_passed());
    }

    /// The failure mode that produced this branch: a fresh keypair has never been
    /// funded, so on Solana it does not exist, and a payer that does not exist is
    /// rejected before the program ever runs. No logs is the tell.
    #[test]
    fn a_transaction_level_rejection_is_not_blamed_on_the_encoder() {
        let (v, why) = classify("\"AccountNotFound\"", &[]);
        assert_eq!(v, Verdict::Inconclusive);
        assert!(why.contains("--as"), "the advice must name the way out: {why}");

        // But the same words *with* logs came from the program, and do mean the encoder.
        let logs = vec!["Program log: AnchorError AccountNotFound".to_string()];
        assert_eq!(classify("", &logs).0, Verdict::Fail);
    }
}
