//! Jito's block engine: where to send, whom to tip, and the least a tip may be.
//!
//! See [`crate::rpc::Rpc::send_jito`] for why a trade goes this way at all. In one line:
//! a bundle that would fail is dropped instead of landing, so a missed floor costs
//! nothing, where through an ordinary RPC it costs the whole fee.

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// The global block engine, sending one transaction as a bundle of one.
///
/// `bundleOnly=true` is the half that matters: without it the block engine also
/// forwards the transaction as an ordinary one, which can land and revert.
pub const DEFAULT_URL: &str =
    "https://mainnet.block-engine.jito.wtf/api/v1/transactions?bundleOnly=true";

/// The least tip the block engine accepts on a bundle, in lamports.
pub const MIN_TIP_LAMPORTS: u64 = 1_000;

/// How long to leave between sends, in milliseconds.
///
/// The documented unauthenticated limit is one request a second per IP per region; a
/// little over it so clock jitter on this side cannot produce a 429 on that one.
pub const MIN_SEND_GAP_MS: u64 = 1_100;

/// The eight tip accounts `getTipAccounts` returns, as documented by Jito.
///
/// Fixed rather than fetched: they have not changed since bundles launched, and asking
/// would put a round trip on the path a trade takes. Spreading tips across them is
/// Jito's own advice, since every bundle tipping one account write-locks it.
pub const TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// One of the tip accounts, chosen by `seed`.
///
/// # Panics
/// Never: the table is checked by a test to hold only valid keys.
#[must_use]
pub fn tip_account(seed: u64) -> Pubkey {
    let i = usize::try_from(seed % TIP_ACCOUNTS.len() as u64).unwrap_or(0);
    Pubkey::from_str(TIP_ACCOUNTS[i]).expect("tip accounts are valid keys")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tip_account_is_a_valid_distinct_key() {
        let keys: std::collections::HashSet<Pubkey> =
            TIP_ACCOUNTS.iter().map(|k| Pubkey::from_str(k).expect("valid")).collect();
        assert_eq!(keys.len(), TIP_ACCOUNTS.len());
        for seed in 0..16 {
            assert!(keys.contains(&tip_account(seed)));
        }
    }

    #[test]
    fn the_default_url_asks_for_revert_protection() {
        assert!(DEFAULT_URL.starts_with("https://"));
        assert!(DEFAULT_URL.contains("bundleOnly=true"));
    }
}
