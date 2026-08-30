//! Assembling instructions into a signed transaction, and the two limits that decide
//! what an arbitrage can be.
//!
//! # A packet is 1232 bytes and that is the real constraint
//!
//! Every account a transaction touches costs 32 bytes in its address table, and a
//! concentrated-liquidity swap touches eleven — pool, two vaults, two user accounts,
//! three tick arrays, an oracle, the token program, the signer. Three of those in one
//! atomic cycle is around thirty distinct addresses before a single instruction byte,
//! which is most of the packet.
//!
//! So [`Assembled::fits`] is not a defensive check, it is the thing that decides
//! whether a given cycle is executable at all, and [`assemble`] refuses rather than
//! truncating. A transaction that is one byte too long is rejected by the node with an
//! error about serialisation, which reads like a bug in the encoder rather than a cycle
//! that was always too big — so the refusal here names the real reason and the measured
//! size.
//!
//! # Wrapped SOL is not SOL
//!
//! A swap moves SPL tokens, and native SOL is not one. Trading out of SOL means putting
//! lamports into a token account owned by the token program and calling `sync_native`
//! so the program believes them; trading back means closing that account to recover
//! them. Both ends are instructions in the same transaction, which is what makes the
//! whole cycle atomic — if any leg fails, the wrap never happened either.

use crate::encode::{pk, programs, Args};
use anyhow::{ensure, Result};
use cb_wallet::Wallet;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::VersionedTransaction;

/// The largest a serialised transaction may be. A Solana packet is 1280 bytes and 48 of
/// them are IPv6 header the transaction does not get to use.
pub const PACKET_LIMIT: usize = 1232;

/// The compute unit ceiling a single transaction may request.
pub const MAX_COMPUTE_UNITS: u32 = 1_400_000;

/// A signed transaction and the measurements that decide whether to send it.
#[derive(Debug, Clone)]
pub struct Assembled {
    pub tx_base64: String,
    pub size_bytes: usize,
    pub account_count: usize,
}

impl Assembled {
    #[must_use]
    pub fn fits(&self) -> bool {
        self.size_bytes <= PACKET_LIMIT
    }
}

/// Ask for a compute unit limit. Charged for what is requested, not what is used, so
/// this is a real cost and not a formality.
#[must_use]
pub fn set_compute_limit(units: u32) -> Instruction {
    Instruction {
        program_id: pk(programs::COMPUTE_BUDGET),
        accounts: vec![],
        data: Args::tagged(2).u32(units.min(MAX_COMPUTE_UNITS)).build(),
    }
}

/// Bid for inclusion, in micro-lamports per compute unit.
#[must_use]
pub fn set_compute_price(micro_lamports: u64) -> Instruction {
    Instruction {
        program_id: pk(programs::COMPUTE_BUDGET),
        accounts: vec![],
        data: Args::tagged(3).u64(micro_lamports).build(),
    }
}

/// Create an associated token account if it does not already exist.
///
/// The idempotent variant, deliberately: the non-idempotent one fails when the account
/// is already there, which for us is the *normal* case after the first trade. Using it
/// would mean every subsequent cycle failed for a reason that has nothing to do with
/// the trade.
#[must_use]
pub fn create_ata_idempotent(
    funder: &Pubkey,
    ata: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: pk(programs::ASSOCIATED_TOKEN),
        accounts: vec![
            AccountMeta::new(*funder, true),
            AccountMeta::new(*ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(pk(programs::SYSTEM), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: Args::tagged(1).build(),
    }
}

/// Move lamports. The System program tags its instructions with a **four-byte** index,
/// not the one byte the SPL programs use.
#[must_use]
pub fn transfer_lamports(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    Instruction {
        program_id: pk(programs::SYSTEM),
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data: Args::tagged(2).u8(0).u8(0).u8(0).u64(lamports).build(),
    }
}

/// Tell the token program to count the lamports sitting in a wrapped-SOL account.
#[must_use]
pub fn sync_native(account: &Pubkey) -> Instruction {
    Instruction {
        program_id: pk(programs::SPL_TOKEN),
        accounts: vec![AccountMeta::new(*account, false)],
        data: Args::tagged(17).build(),
    }
}

/// Close a token account, returning its rent and any wrapped SOL to `destination`.
#[must_use]
pub fn close_account(account: &Pubkey, destination: &Pubkey, owner: &Pubkey) -> Instruction {
    Instruction {
        program_id: pk(programs::SPL_TOKEN),
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: Args::tagged(9).build(),
    }
}

/// Compile, sign, and measure. Refuses anything that will not fit in a packet.
///
/// # Errors
/// If the instructions cannot be compiled, if the result is over [`PACKET_LIMIT`], or
/// if serialisation fails.
pub fn assemble(
    wallet: &Wallet,
    instructions: &[Instruction],
    blockhash: Hash,
) -> Result<Assembled> {
    ensure!(!instructions.is_empty(), "a transaction with no instructions is not a transaction");

    let payer = wallet.pubkey();
    let message = v0::Message::try_compile(&payer, instructions, &[], blockhash)?;
    let account_count = message.account_keys.len();
    let versioned = VersionedMessage::V0(message);

    // Sign the compiled message, not the instructions — the compiler reorders accounts
    // and the signature covers the compiled bytes.
    let signature = wallet.sign(&versioned.serialize());
    let required = versioned.header().num_required_signatures as usize;
    ensure!(
        required == 1,
        "this transaction needs {required} signatures and only the trading key is held; \
         an instruction is naming a signer that is not the wallet"
    );

    let tx = VersionedTransaction { signatures: vec![signature], message: versioned };
    let bytes = bincode::serialize(&tx)?;
    let size_bytes = bytes.len();

    ensure!(
        size_bytes <= PACKET_LIMIT,
        "the assembled transaction is {size_bytes} bytes over a {PACKET_LIMIT}-byte limit \
         ({account_count} accounts across {} instructions) — this cycle cannot be executed \
         atomically without an address lookup table",
        instructions.len()
    );

    Ok(Assembled {
        tx_base64: crate::rpc::base64_encode(&bytes),
        size_bytes,
        account_count,
    })
}

/// Compile for **simulation only**, with a placeholder signature and no key involved.
///
/// `simulateTransaction` is called with `sigVerify: false`, so the signature is never
/// checked and a zeroed one is as good as a real one. That makes it possible to ask
/// the chain what it thinks of a transaction on behalf of an address whose key nobody
/// has — which is exactly what verification wants: it needs a *funded* payer, not an
/// *owned* one, and requiring a real key to run a check would put the operator's
/// wallet in the path of a diagnostic.
///
/// Never use this for anything that could be submitted. A transaction carrying a
/// placeholder signature cannot land, which is a property worth keeping rather than
/// working around.
///
/// # Errors
/// If the instructions cannot be compiled or serialised.
pub fn compile_unsigned(
    payer: &Pubkey,
    instructions: &[Instruction],
    blockhash: Hash,
) -> Result<Assembled> {
    ensure!(!instructions.is_empty(), "a transaction with no instructions is not a transaction");
    let message = v0::Message::try_compile(payer, instructions, &[], blockhash)?;
    let account_count = message.account_keys.len();
    let versioned = VersionedMessage::V0(message);
    let signatures = vec![Signature::default(); versioned.header().num_required_signatures as usize];
    let tx = VersionedTransaction { signatures, message: versioned };
    let bytes = bincode::serialize(&tx)?;
    Ok(Assembled {
        tx_base64: crate::rpc::base64_encode(&bytes),
        size_bytes: bytes.len(),
        account_count,
    })
}

/// Compile and measure **without** signing, for asking whether a cycle would fit before
/// committing to building it.
///
/// # Errors
/// If the instructions cannot be compiled.
pub fn measure(payer: &Pubkey, instructions: &[Instruction], blockhash: Hash) -> Result<usize> {
    let message = v0::Message::try_compile(payer, instructions, &[], blockhash)?;
    let versioned = VersionedMessage::V0(message);
    let tx = VersionedTransaction {
        // A placeholder signature is the same 64 bytes as a real one.
        signatures: vec![Signature::default()],
        message: versioned,
    };
    Ok(bincode::serialize(&tx)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway key. `Wallet` has no constructor from raw bytes on purpose — the
    /// only way to get one is through the sealed-file path — so a test wallet is
    /// produced the same way a real one is.
    fn wallet() -> Wallet {
        use solana_sdk::signer::keypair::Keypair;
        let bytes = Keypair::new().to_bytes();
        match cb_wallet::EncryptedKey::seal(&bytes, "pass").and_then(|e| e.unseal("pass")) {
            Ok(w) => w,
            Err(e) => panic!("could not build a test wallet: {e}"),
        }
    }

    fn noop(program: Pubkey, accounts: usize) -> Instruction {
        Instruction {
            program_id: program,
            accounts: (0..accounts).map(|_| AccountMeta::new(Pubkey::new_unique(), false)).collect(),
            data: vec![0u8; 40],
        }
    }

    #[test]
    fn the_compute_budget_tags_are_the_documented_ones() {
        assert_eq!(set_compute_limit(200_000).data, vec![2, 0x40, 0x0d, 0x03, 0x00]);
        assert_eq!(set_compute_price(1).data[0], 3);
        assert_eq!(set_compute_price(1).data.len(), 9);
        // A request above the ceiling is capped rather than rejected by the node later.
        let capped = set_compute_limit(u32::MAX).data;
        assert_eq!(&capped[1..5], &MAX_COMPUTE_UNITS.to_le_bytes());
    }

    /// The System program uses a four-byte index and the SPL programs use one byte.
    /// Encoding a System instruction with a one-byte tag produces a transfer of a
    /// different amount, not an error.
    #[test]
    fn the_system_transfer_index_is_four_bytes_wide() {
        let ix = transfer_lamports(&Pubkey::new_unique(), &Pubkey::new_unique(), 5_000);
        assert_eq!(&ix.data[..4], &[2, 0, 0, 0], "transfer is index 2 as a u32");
        assert_eq!(&ix.data[4..12], &5_000u64.to_le_bytes());
        assert_eq!(ix.data.len(), 12);
    }

    #[test]
    fn the_spl_tags_are_one_byte_and_are_the_documented_ones() {
        assert_eq!(sync_native(&Pubkey::new_unique()).data, vec![17]);
        assert_eq!(close_account(&Pubkey::new_unique(), &Pubkey::new_unique(), &Pubkey::new_unique()).data, vec![9]);
        assert_eq!(create_ata_idempotent(
            &Pubkey::new_unique(), &Pubkey::new_unique(), &Pubkey::new_unique(),
            &Pubkey::new_unique(), &pk(programs::SPL_TOKEN),
        ).data, vec![1]);
    }

    /// Closing a wrapped-SOL account is what recovers the lamports, and only the owner
    /// may do it — so the owner must be the signer and the account must not be.
    #[test]
    fn only_the_owner_signs_a_close() {
        let (acct, dest, owner) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let ix = close_account(&acct, &dest, &owner);
        assert!(!ix.accounts[0].is_signer);
        assert!(ix.accounts[2].is_signer);
        assert_eq!(ix.accounts[2].pubkey, owner);
    }

    #[test]
    fn a_small_transaction_assembles_signs_and_measures() {
        let w = wallet();
        let ix = noop(Pubkey::new_unique(), 4);
        let a = assemble(&w, &[ix], Hash::default()).expect("a four-account transaction fits");
        assert!(a.fits());
        assert!(a.size_bytes > 100, "a signed transaction is not tiny");
        assert!(!a.tx_base64.is_empty());
        // payer + 4 accounts + the program itself.
        assert_eq!(a.account_count, 6);
    }

    /// The point of the module. A cycle whose account list overflows a packet must be
    /// refused with the measured size, not truncated and not sent.
    #[test]
    fn an_oversized_transaction_is_refused_and_says_how_big_it_was() {
        let w = wallet();
        let fat: Vec<Instruction> = (0..4).map(|_| noop(Pubkey::new_unique(), 12)).collect();
        let err = assemble(&w, &fat, Hash::default()).unwrap_err().to_string();
        assert!(err.contains("1232"), "the refusal must name the limit: {err}");
        assert!(err.contains("accounts"), "the refusal must name the account count: {err}");
    }

    #[test]
    fn an_empty_transaction_is_refused() {
        assert!(assemble(&wallet(), &[], Hash::default()).is_err());
    }

    /// `measure` must agree with `assemble` to the byte, or a cycle could pass the
    /// cheap pre-check and then fail the real one.
    #[test]
    fn measuring_agrees_with_assembling() {
        let w = wallet();
        for n in [1usize, 3, 6] {
            let ixs: Vec<Instruction> = (0..n).map(|_| noop(Pubkey::new_unique(), 3)).collect();
            let assembled = assemble(&w, &ixs, Hash::default()).expect("fits");
            let measured = measure(&w.pubkey(), &ixs, Hash::default()).expect("measurable");
            assert_eq!(assembled.size_bytes, measured, "disagreement at {n} instructions");
        }
    }

    /// How much of the packet an unshared account list costs, measured rather than
    /// asserted from arithmetic.
    ///
    /// This is the pessimistic bound: eleven *distinct* accounts per hop, nothing
    /// shared. A real cycle shares the signer, the token program and the token accounts
    /// between adjacent legs, which is why `route.rs` measures the real thing and gets
    /// a higher ceiling than this. The number here is the floor under that.
    #[test]
    fn without_any_account_sharing_the_ceiling_is_two_hops() {
        let w = wallet();
        let per_hop = |_| noop(Pubkey::new_unique(), 10);

        let two: Vec<Instruction> = (0..2).map(per_hop).collect();
        assert!(assemble(&w, &two, Hash::default()).is_ok(), "two unshared hops must fit");

        let three: Vec<Instruction> = (0..3).map(per_hop).collect();
        assert!(
            assemble(&w, &three, Hash::default()).is_err(),
            "three hops with no shared accounts should overflow; if this now fits, the \
             per-hop account cost dropped and route.rs's ceiling should be re-measured"
        );
    }

    /// The unsigned form must be byte-identical in size to the signed one, or a size
    /// measured during verification would not predict the real transaction.
    #[test]
    fn an_unsigned_compile_is_the_same_size_as_a_signed_one() {
        let w = wallet();
        let ixs = vec![noop(Pubkey::new_unique(), 3)];
        let signed = assemble(&w, &ixs, Hash::default()).unwrap();
        let unsigned = compile_unsigned(&w.pubkey(), &ixs, Hash::default()).unwrap();
        assert_eq!(signed.size_bytes, unsigned.size_bytes);
        assert_eq!(signed.account_count, unsigned.account_count);
        // But not the same bytes: one carries a real signature and one carries zeroes.
        assert_ne!(signed.tx_base64, unsigned.tx_base64);
    }

    /// A placeholder-signed transaction must not be mistakable for a sendable one.
    #[test]
    fn an_unsigned_compile_carries_a_zero_signature() {
        let payer = Pubkey::new_unique();
        let a = compile_unsigned(&payer, &[noop(Pubkey::new_unique(), 2)], Hash::default()).unwrap();
        let raw = crate::rpc::base64_decode_for_test(&a.tx_base64).expect("own output decodes");
        // One signature, 64 bytes, all zero, right after the count byte.
        assert_eq!(raw[0], 1);
        assert!(raw[1..65].iter().all(|b| *b == 0), "the signature must be a placeholder");
    }
}
