//! Helius Sender: where to send, whom to tip, and the least a tip may be.
//!
//! See [`crate::rpc::Rpc::send_sender`] for why a trade goes this way. In one line: Jito
//! drops unauthenticated sends before its auction, and Sender lands them — but with no
//! revert protection, so a trade sent here is always simulated first.
//!
//! Facts from <https://www.helius.dev/docs/sending-transactions/sender> (2026-11-08).

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// The global endpoint, SWQOS-only: the cheaper tier, which needs a smaller tip but
/// still requires a compute-unit price on every transaction.
pub const BASE_URL: &str = "https://sender.helius-rpc.com/fast?swqos_only=true";

/// The least tip the SWQOS-only tier accepts, in lamports (0.000005 SOL).
pub const MIN_TIP_LAMPORTS: u64 = 5_000;

/// Helius's documented Sender tip accounts (mainnet-beta). Fixed rather than fetched,
/// as [`crate::jito::TIP_ACCOUNTS`] is, and spread over to avoid one write lock.
pub const TIP_ACCOUNTS: [&str; 10] = [
    "4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE",
    "D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ",
    "9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta",
    "5VY91ws6B2hMmBFRsXkoAAdsPHBJwRfBht4DXox3xkwn",
    "2nyhqdwKcJZR2vcqCyrYsaPVdAnFoJjiksCXJ7hfEYgD",
    "2q5pghRs6arqVjRvT5gfgWfWcHWmw1ZuCzphgd5KfWGJ",
    "wyvPkWjVZz1M8fHQnMMCDTQDbkManefNNhweYk5WkcF",
    "3KCKozbAaF75qEU33jtzozcJ29yJuaLJTy2jFdzUY8bT",
    "4vieeGHPYPG2MmyPRcYjdiDmmhN3ww7hsFNap8pVN3Ey",
    "4TQLFNWK8AovT1gFvda5jfw2oJeRMKEmw7aH6MGBJ3or",
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

/// The send URL, with the api key when there is one. Holds a secret when keyed: only
/// ever logged through `cb_core::redact`.
#[must_use]
pub fn url(api_key: Option<&str>) -> String {
    match api_key {
        Some(k) => format!("{BASE_URL}&api-key={k}"),
        None => BASE_URL.to_string(),
    }
}

/// The Helius api key in the first of `endpoints` on a `helius-rpc.com` host that
/// carries one as `api-key=`. Sender takes the same key as the RPC.
#[must_use]
pub fn api_key_in(endpoints: &[String]) -> Option<String> {
    endpoints.iter().find_map(|e| {
        let u = reqwest::Url::parse(e.trim()).ok()?;
        let host = u.host_str()?;
        if host != "helius-rpc.com" && !host.ends_with(".helius-rpc.com") {
            return None;
        }
        u.query_pairs().find(|(k, v)| k == "api-key" && !v.trim().is_empty()).map(|(_, v)| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tip_account_is_a_valid_distinct_key_and_none_is_jitos() {
        let keys: std::collections::HashSet<Pubkey> =
            TIP_ACCOUNTS.iter().map(|k| Pubkey::from_str(k).expect("valid")).collect();
        assert_eq!(keys.len(), TIP_ACCOUNTS.len());
        for seed in 0..20 {
            assert!(keys.contains(&tip_account(seed)));
        }
        assert!(TIP_ACCOUNTS.iter().all(|k| !crate::jito::TIP_ACCOUNTS.contains(k)));
    }

    #[test]
    fn the_url_is_swqos_only_with_and_without_a_key() {
        assert_eq!(url(None), "https://sender.helius-rpc.com/fast?swqos_only=true");
        assert_eq!(url(Some("K1")), "https://sender.helius-rpc.com/fast?swqos_only=true&api-key=K1");
        let redacted = cb_core::redact::redact_endpoint(&url(Some("SECRET-KEY")));
        assert!(!redacted.contains("SECRET-KEY"), "{redacted}");
    }

    #[test]
    fn the_key_comes_only_from_a_helius_url() {
        let e = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(api_key_in(&e(&["https://mainnet.helius-rpc.com/?api-key=abc"])).as_deref(), Some("abc"));
        assert_eq!(
            api_key_in(&e(&["https://api.mainnet-beta.solana.com", "https://mainnet.helius-rpc.com/?x=1&api-key=k2"]))
                .as_deref(),
            Some("k2")
        );
        assert_eq!(api_key_in(&e(&["https://other.example/?api-key=abc"])), None);
        assert_eq!(api_key_in(&e(&["https://helius-rpc.com.evil.example/?api-key=abc"])), None);
        assert_eq!(api_key_in(&e(&["https://mainnet.helius-rpc.com/?api-key="])), None);
        assert_eq!(api_key_in(&[]), None);
    }
}
