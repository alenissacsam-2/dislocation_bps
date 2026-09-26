//! Address lookup tables: how a cycle too wide for one packet fits in one.
//!
//! # Why
//!
//! A transaction is capped at 1,232 bytes and every account it names costs 32 of them.
//! A three-hop Raydium cycle names 34 to 39 accounts and serialises to 1,411–1,592
//! bytes. On 2026-09-25, 363 such cycles cleared their floor on fresh state and were
//! refused at assembly for size alone, against 4 that fit and were sent.
//!
//! A lookup table is an on-chain list of addresses. A version-0 message can then name
//! any account in it by a one-byte index instead of its 32-byte key, which brings the
//! same three-hop cycle to roughly 750 bytes.
//!
//! # What is encoded here
//!
//! The four instructions this bot needs from the lookup table program, written out by
//! hand because the SDK in use no longer ships them: create, extend, deactivate and
//! close. Each is a bincode enum — a little-endian `u32` tag, then its fields.
//!
//! Signers and invoked programs cannot be looked up, and Jito's tip accounts are kept
//! out on purpose: the block engine looks for the tip among the transaction's own
//! keys. [`candidates`] applies all three rules.

use crate::encode::{pk, programs};
use anyhow::{ensure, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::AddressLookupTableAccount;
use solana_sdk::pubkey::Pubkey;

/// The address lookup table program.
pub const PROGRAM_ID: &str = "AddressLookupTab1e1111111111111111111111111";

/// Bytes of metadata before the first address in a table account.
pub const META_LEN: usize = 56;

/// The most addresses one table can hold.
pub const MAX_ADDRESSES: usize = 256;

/// The most addresses one extend instruction carries here. Twenty keeps the extending
/// transaction comfortably inside a packet with its own fee payer and accounts.
pub const EXTEND_CHUNK: usize = 20;

/// The table address for `authority` created at `recent_slot`, and its bump.
#[must_use]
pub fn derive(authority: &Pubkey, recent_slot: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[authority.as_ref(), &recent_slot.to_le_bytes()],
        &pk(PROGRAM_ID),
    )
}

/// Create a table owned by `authority`, funded by `payer`.
///
/// `recent_slot` must be one the chain still remembers (the last ~150 slots is safe);
/// it seeds the address, so the same authority can own many tables.
#[must_use]
pub fn create(authority: &Pubkey, payer: &Pubkey, recent_slot: u64) -> (Instruction, Pubkey) {
    let (table, bump) = derive(authority, recent_slot);
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend_from_slice(&recent_slot.to_le_bytes());
    data.push(bump);
    let ix = Instruction {
        program_id: pk(PROGRAM_ID),
        accounts: vec![
            AccountMeta::new(table, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(pk(programs::SYSTEM), false),
        ],
        data,
    };
    (ix, table)
}

/// Append `addresses` to `table`. Usable by transactions from the next slot on.
///
/// # Errors
/// If `addresses` is empty or longer than [`EXTEND_CHUNK`].
pub fn extend(
    table: &Pubkey,
    authority: &Pubkey,
    payer: &Pubkey,
    addresses: &[Pubkey],
) -> Result<Instruction> {
    ensure!(!addresses.is_empty(), "an extend with no addresses is refused by the program");
    ensure!(addresses.len() <= EXTEND_CHUNK, "at most {EXTEND_CHUNK} addresses per extend");
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&(addresses.len() as u64).to_le_bytes());
    for a in addresses {
        data.extend_from_slice(a.as_ref());
    }
    Ok(Instruction {
        program_id: pk(PROGRAM_ID),
        accounts: vec![
            AccountMeta::new(*table, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(pk(programs::SYSTEM), false),
        ],
        data,
    })
}

/// Begin retiring `table`. It can be closed, and its rent recovered, about 513 slots
/// later.
#[must_use]
pub fn deactivate(table: &Pubkey, authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: pk(PROGRAM_ID),
        accounts: vec![AccountMeta::new(*table, false), AccountMeta::new_readonly(*authority, true)],
        data: 3u32.to_le_bytes().to_vec(),
    }
}

/// Close a deactivated `table` and send its rent to `recipient`.
#[must_use]
pub fn close(table: &Pubkey, authority: &Pubkey, recipient: &Pubkey) -> Instruction {
    Instruction {
        program_id: pk(PROGRAM_ID),
        accounts: vec![
            AccountMeta::new(*table, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*recipient, false),
        ],
        data: 4u32.to_le_bytes().to_vec(),
    }
}

/// The addresses stored in a table account's data.
///
/// # Errors
/// If the account is shorter than its metadata or not a whole number of addresses.
pub fn parse(key: Pubkey, data: &[u8]) -> Result<AddressLookupTableAccount> {
    ensure!(data.len() >= META_LEN, "a lookup table is at least {META_LEN} bytes");
    let body = &data[META_LEN..];
    ensure!(body.len() % 32 == 0, "a lookup table body is whole 32-byte addresses");
    let addresses = body
        .chunks_exact(32)
        .map(|c| Pubkey::new_from_array(c.try_into().expect("chunks of 32")))
        .collect();
    Ok(AddressLookupTableAccount { key, addresses })
}

/// The accounts in `instructions` a lookup table could carry and does not yet:
/// neither a signer, nor a program the transaction invokes, nor a Jito tip account.
#[must_use]
pub fn candidates(instructions: &[Instruction], held: &[Pubkey]) -> Vec<Pubkey> {
    let programs: Vec<Pubkey> = instructions.iter().map(|ix| ix.program_id).collect();
    let tips: Vec<Pubkey> = crate::jito::TIP_ACCOUNTS.iter().map(|t| pk(t)).collect();
    let mut out: Vec<Pubkey> = Vec::new();
    for meta in instructions.iter().flat_map(|ix| ix.accounts.iter()) {
        let k = meta.pubkey;
        if meta.is_signer
            || programs.contains(&k)
            || tips.contains(&k)
            || held.contains(&k)
            || out.contains(&k)
        {
            continue;
        }
        out.push(k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_encodes_tag_slot_and_bump_and_derives_its_address() {
        let auth = Pubkey::new_unique();
        let (ix, table) = create(&auth, &auth, 450_000_000);
        assert_eq!(ix.data.len(), 4 + 8 + 1);
        assert_eq!(&ix.data[..4], &0u32.to_le_bytes());
        assert_eq!(&ix.data[4..12], &450_000_000u64.to_le_bytes());
        let (expected, bump) = derive(&auth, 450_000_000);
        assert_eq!(table, expected);
        assert_eq!(ix.data[12], bump);
        assert_eq!(ix.accounts[0].pubkey, table);
        assert!(ix.accounts[1].is_signer && ix.accounts[2].is_signer && ix.accounts[2].is_writable);
    }

    #[test]
    fn extend_carries_a_bincode_vec_of_keys() {
        let (t, a) = (Pubkey::new_unique(), Pubkey::new_unique());
        let keys = [Pubkey::new_unique(), Pubkey::new_unique()];
        let ix = extend(&t, &a, &a, &keys).unwrap();
        assert_eq!(&ix.data[..4], &2u32.to_le_bytes());
        assert_eq!(&ix.data[4..12], &2u64.to_le_bytes());
        assert_eq!(&ix.data[12..44], keys[0].as_ref());
        assert_eq!(ix.data.len(), 12 + 64);
        assert!(extend(&t, &a, &a, &[]).is_err());
        assert!(extend(&t, &a, &a, &vec![Pubkey::new_unique(); EXTEND_CHUNK + 1]).is_err());
    }

    #[test]
    fn a_table_account_parses_to_its_addresses() {
        let keys = [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()];
        let mut data = vec![0u8; META_LEN];
        for k in &keys {
            data.extend_from_slice(k.as_ref());
        }
        let t = Pubkey::new_unique();
        let parsed = parse(t, &data).unwrap();
        assert_eq!(parsed.key, t);
        assert_eq!(parsed.addresses, keys.to_vec());
        assert!(parse(t, &data[..40]).is_err());
        assert!(parse(t, &data[..META_LEN + 5]).is_err());
    }

    #[test]
    fn signers_programs_tips_and_held_keys_are_never_candidates() {
        let owner = Pubkey::new_unique();
        let (pool, vault, held) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let program = Pubkey::new_unique();
        let tip = pk(crate::jito::TIP_ACCOUNTS[0]);
        let ixs = vec![
            Instruction {
                program_id: program,
                accounts: vec![
                    AccountMeta::new(owner, true),
                    AccountMeta::new(pool, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new(pool, false),
                    AccountMeta::new_readonly(held, false),
                ],
                data: vec![],
            },
            crate::tx::transfer_lamports(&owner, &tip, 1_000),
        ];
        let c = candidates(&ixs, &[held]);
        assert_eq!(c, vec![pool, vault], "deduplicated, in first-seen order");
    }
}
