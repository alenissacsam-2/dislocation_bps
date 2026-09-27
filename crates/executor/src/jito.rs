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

/// The id the block engine files a bundle of one under: the SHA-256 of its signature
/// in base58, as lowercase hex. The same for every region, so a status can be asked of
/// any of them without the `x-bundle-id` header the send answered with.
#[must_use]
pub fn bundle_id(signature: &str) -> String {
    solana_sdk::hash::hashv(&[signature.as_bytes()]).to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `uuid` has the shape of the key Jito issues: 36 characters, hex digits in
/// groups of 8-4-4-4-12. Checked so a pasted key with a stray quote or space is caught
/// at start-up, where it can be said, rather than as sends that quietly fail.
#[must_use]
pub fn looks_like_uuid(uuid: &str) -> bool {
    let groups: Vec<&str> = uuid.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Another block engine method on the same host as `send_url`.
///
/// `send_url` is the configured `.../api/v1/transactions?bundleOnly=true`; every other
/// method lives beside it under `/api/v1/`, with no query string.
#[must_use]
pub fn api_url(send_url: &str, method: &str) -> String {
    let base = send_url.find("/api/v1/").map_or(send_url.trim_end_matches('/'), |i| &send_url[..i]);
    format!("{base}/api/v1/{method}")
}

/// Every block engine region Jito runs, by the host prefix each is served under.
///
/// # Why a trade goes to all of them
///
/// A validator takes its bundles from the one block engine it is connected to, usually
/// the region nearest it. A bundle sent only to Singapore therefore waits for a leader
/// connected to Singapore, and most stake is in Europe and North America. On
/// 2026-09-26 two tip-only bundles — no floor, no race, the minimum tip — were sent to
/// Singapore alone and neither landed in 30 s, after fifteen trade bundles had gone
/// the same way. The same signed transaction sent to every region reaches whichever
/// leader comes next, and cannot land twice: a signature is included at most once.
pub const REGIONS: [&str; 8] = ["singapore", "tokyo", "frankfurt", "amsterdam", "london", "dublin", "ny", "slc"];

/// The domain every regional block engine is a subdomain of.
const REGIONAL_DOMAIN: &str = "mainnet.block-engine.jito.wtf";

/// `send_url` on every region's host, `send_url` itself first.
///
/// A URL that is not on one of Jito's own block engine hosts comes back alone: a
/// custom relay has no regions to fan out across.
#[must_use]
pub fn fanout_urls(send_url: &str) -> Vec<String> {
    let url = send_url.trim();
    let mut out = vec![url.to_string()];
    let Some(scheme_end) = url.find("://") else { return out };
    let rest = &url[scheme_end + 3..];
    let (host, path) = rest.find('/').map_or((rest, ""), |i| (&rest[..i], &rest[i..]));
    if host != REGIONAL_DOMAIN && !host.ends_with(&format!(".{REGIONAL_DOMAIN}")) {
        return out;
    }
    for region in REGIONS {
        let u = format!("https://{region}.{REGIONAL_DOMAIN}{path}");
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bundle of one that landed, as Jito's explorer lists it.
    #[test]
    fn a_bundle_id_is_the_sha256_of_the_base58_signature() {
        assert_eq!(
            bundle_id("3D9KSDtv242JrQh7TV1mv7KfihmqnurSP8vtWjCQ1sSWv8U7zTsvvZhPGa6Ri7tcKYTPDBw9uyy5KhBN8TVeUUFC"),
            "aa2103c79bc47e580f986d6f0fde88b26b877c430fab7eaa036ecfee3a081296"
        );
    }

    #[test]
    fn a_uuid_is_recognised_and_a_mangled_one_is_not() {
        assert!(looks_like_uuid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(!looks_like_uuid("\"123e4567-e89b-12d3-a456-426614174000"));
        assert!(!looks_like_uuid("123e4567-e89b-12d3-a456-42661417400 "));
        assert!(!looks_like_uuid("123e4567e89b12d3a456426614174000"));
        assert!(!looks_like_uuid(""));
    }

    #[test]
    fn a_regional_url_fans_out_to_every_region_itself_first() {
        let sg = "https://singapore.mainnet.block-engine.jito.wtf/api/v1/transactions?bundleOnly=true";
        let urls = fanout_urls(sg);
        assert_eq!(urls[0], sg, "the configured region stays the first to be asked");
        assert_eq!(urls.len(), REGIONS.len(), "singapore is not sent to twice");
        for r in REGIONS {
            assert!(urls.iter().any(|u| u.starts_with(&format!("https://{r}.mainnet"))), "{r} missing");
        }
        assert!(urls.iter().all(|u| u.ends_with("/api/v1/transactions?bundleOnly=true")), "{urls:?}");
    }

    #[test]
    fn the_global_url_keeps_itself_and_adds_every_region() {
        let urls = fanout_urls(DEFAULT_URL);
        assert_eq!(urls[0], DEFAULT_URL);
        assert_eq!(urls.len(), REGIONS.len() + 1);
    }

    #[test]
    fn a_url_that_is_not_jitos_is_not_fanned_out() {
        assert_eq!(fanout_urls("https://relay.example.com/api/v1/transactions"), vec![
            "https://relay.example.com/api/v1/transactions".to_string()
        ]);
        assert_eq!(fanout_urls("not a url"), vec!["not a url".to_string()]);
        // A look-alike host is not Jito's.
        assert_eq!(fanout_urls("https://evilmainnet.block-engine.jito.wtf.example/x").len(), 1);
    }

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

    #[test]
    fn other_methods_are_found_beside_the_send_url() {
        assert_eq!(
            api_url(DEFAULT_URL, "getInflightBundleStatuses"),
            format!("{}/api/v1/getInflightBundleStatuses", &DEFAULT_URL[..DEFAULT_URL.find("/api/v1/").unwrap()])
        );
        assert_eq!(
            api_url("https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/transactions?bundleOnly=true", "bundles"),
            "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/bundles"
        );
    }
}
