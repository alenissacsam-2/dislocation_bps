//! Jito's block engine: where to send, whom to tip, and the least a tip may be.
//!
//! See [`crate::rpc::Rpc::send_jito`] for why a trade goes this way at all. In one line:
//! a bundle that would fail is dropped instead of landing, so a missed floor costs
//! nothing, where through an ordinary RPC it costs the whole fee.

use crate::rpc::{JitoStatus, Rpc};
use anyhow::{bail, ensure, Result};
use cb_wallet::Wallet;
use solana_sdk::{hash::Hash, pubkey::Pubkey};
use std::str::FromStr;
use std::time::Duration;

/// The Singapore block engine, the nearest to this machine (see [`REGIONS`]), sending
/// one transaction as a bundle of one.
///
/// `bundleOnly=true` is the half that matters: without it the block engine also
/// forwards the transaction as an ordinary one, which can land and revert.
pub const DEFAULT_URL: &str =
    "https://singapore.mainnet.block-engine.jito.wtf/api/v1/transactions?bundleOnly=true";

/// The least tip the block engine accepts on a bundle, in lamports.
pub const MIN_TIP_LAMPORTS: u64 = 1_000;

/// How long to leave between sends, in milliseconds.
///
/// The documented unauthenticated limit is one request a second per IP per region; a
/// little over it so clock jitter on this side cannot produce a 429 on that one.
pub const MIN_SEND_GAP_MS: u64 = 1_100;

/// The most bundle ids one `getInflightBundleStatuses` request may ask about.
pub const MAX_STATUS_IDS: usize = 5;

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

/// Whether `url` asks for revert protection: a `bundleOnly=true` query parameter.
/// Without it the block engine also forwards the transaction as an ordinary one, which
/// can land, revert, and pay — so a send that skips our own simulation must not go there.
#[must_use]
pub fn is_bundle_only(url: &str) -> bool {
    reqwest::Url::parse(url.trim())
        .is_ok_and(|u| u.query_pairs().any(|(k, v)| k == "bundleOnly" && v == "true"))
}

/// The block engine regions a send goes to, by the host prefix each is served under:
/// only the nearest.
///
/// Measured from India on 2026-11-09 with `getTipAccounts`, Singapore answers in about
/// 415 ms on a fresh connection (115 ms warm); the global host resolves to Singapore's
/// address but answers in 648, Tokyo in 727, and Europe and the US in 920 to 1,360 ms.
/// Fanning out to all eight was tried from 2026-09-26 and every send was `Invalid`
/// either way, so the far regions bought nothing and cost each one a request.
/// shortcut: fixed to this machine's nearest; re-measure if the bot moves to a VPS.
pub const REGIONS: [&str; 1] = ["singapore"];

/// The domain every regional block engine is a subdomain of.
const REGIONAL_DOMAIN: &str = "mainnet.block-engine.jito.wtf";

/// `send_url` on every [`REGIONS`] host, a regional `send_url` itself first.
///
/// The global host is left out: it resolves to one of the regions (Singapore, from
/// India), so sending to both put two requests on that region in the same instant,
/// over its one-a-second limit.
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
    if host == REGIONAL_DOMAIN {
        out.clear();
    }
    for region in REGIONS {
        let u = format!("https://{region}.{REGIONAL_DOMAIN}{path}");
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

/// How long [`probe_bundle_only`] follows its bundle: about a blockhash's life, after
/// which it can no longer land.
pub const PROBE_FOLLOW: Duration = Duration::from_secs(60);

/// How often it asks, half the one-a-second allowance of the one region it asks.
const PROBE_POLL: Duration = Duration::from_secs(2);

/// A tip-only bundle of one: [`MIN_TIP_LAMPORTS`] from the wallet to a tip account, as
/// the only instruction, signed and base64 for the wire. The tip is inside the
/// transaction and its account is named directly, never through a lookup table, as the
/// block engine requires.
///
/// # Errors
/// If the transaction cannot be assembled.
pub fn probe_tx(wallet: &Wallet, blockhash: Hash) -> Result<String> {
    let to = tip_account(u64::from(blockhash.to_bytes()[0]));
    let tip = crate::tx::transfer_lamports(&wallet.pubkey(), &to, MIN_TIP_LAMPORTS);
    Ok(crate::tx::assemble(wallet, &[tip], blockhash)?.tx_base64)
}

/// Whether Jito's revert-protected path takes this sender's bundles now.
///
/// Sends one [`probe_tx`] through `sendTransaction?bundleOnly=true` to every URL in
/// `urls`, then follows it in the background with `getInflightBundleStatuses` at the
/// first (nearest) URL — the in-flight call, since `getBundleStatuses` knows only
/// bundles that landed — and logs one line saying what the trail of statuses means
/// (see [`describe`]). The send's own line from [`Rpc::send_jito_all`] says what every
/// region answered.
///
/// Costs [`MIN_TIP_LAMPORTS`] and the 5,000-lamport base fee if it lands; nothing if
/// it does not, which is what `bundleOnly=true` buys.
///
/// # Errors
/// If a URL lacks `bundleOnly=true` (nothing is sent), or every region refuses or
/// cannot be reached; the message says what each answered.
pub async fn probe_bundle_only(rpc: &Rpc, wallet: &Wallet, urls: &[String], blockhash: Hash) -> Result<()> {
    ensure!(!urls.is_empty(), "no block engine to send the probe to");
    if let Some(u) = urls.iter().find(|u| !is_bundle_only(u)) {
        bail!(
            "{} lacks bundleOnly=true, so the probe would be forwarded as an ordinary transaction",
            cb_core::redact::redact_urls_in(u)
        );
    }
    let receipt = rpc.send_jito_all(urls, &probe_tx(wallet, blockhash)?).await?;
    let id = receipt.bundle_id.clone().unwrap_or_else(|| bundle_id(&receipt.signature));
    let sig = receipt.signature;
    tracing::info!(
        "Jito bundleOnly probe: sent a tip-only bundle of {MIN_TIP_LAMPORTS} lamports to {} block engines: \
         {sig} (bundle {id}); following it for {} s",
        urls.len(),
        PROBE_FOLLOW.as_secs()
    );
    let (status, url) = (rpc.jito_status(), urls[0].clone());
    tokio::spawn(async move {
        let trail = follow(&status, &url, &id).await;
        let line = format!("Jito bundleOnly probe {sig} (bundle {id}): {}", describe(&trail));
        if trail.last().is_some_and(|(_, s)| s == "Landed") {
            tracing::info!("{line}");
        } else {
            tracing::warn!("{line}");
        }
    });
    Ok(())
}

/// Ask `url` about bundle `id` every [`PROBE_POLL`] for [`PROBE_FOLLOW`], keeping each
/// answer that differs from the last with the second it came. Stops at `Landed` or
/// `Failed`, which are final.
async fn follow(status: &JitoStatus, url: &str, id: &str) -> Vec<(u64, String)> {
    let start = tokio::time::Instant::now();
    let mut trail: Vec<(u64, String)> = Vec::new();
    while start.elapsed() < PROBE_FOLLOW {
        tokio::time::sleep(PROBE_POLL).await;
        let now = match status.status(url, id).await {
            Ok(Some(s)) => s,
            Ok(None) => "no status".to_string(),
            Err(e) => format!("query failed: {e:#}"),
        };
        if trail.last().is_none_or(|(_, s)| *s != now) {
            trail.push((start.elapsed().as_secs(), now));
        }
        if trail.last().is_some_and(|(_, s)| s == "Landed" || s == "Failed") {
            break;
        }
    }
    trail
}

/// What a probe's trail of `(second, status)` means, then the trail itself.
#[must_use]
pub fn describe(trail: &[(u64, String)]) -> String {
    let any = |w: &str| trail.iter().any(|(_, s)| s == w);
    let meaning = match trail.last().map(|(_, s)| s.as_str()) {
        None => "no answer from the block engine",
        Some("Landed") => {
            "LANDED: the block engine takes this sender's revert-protected bundles without a UUID; \
             the probe paid its tip and the base fee"
        }
        Some("Failed") => {
            "Failed: the block engine took the bundle and its simulation failed, so the path is \
             open and this transaction was not valid"
        }
        _ if any("Pending") => {
            "not landed: it reached the auction and no Jito leader took it at the minimum tip, so \
             the path is open and the tip is the lever"
        }
        _ if trail.iter().all(|(_, s)| s == "Invalid") => {
            "Invalid throughout: the block engine accepted the request and never recorded the \
             bundle, so it still drops this sender's bundles before the auction, where no tip helps"
        }
        _ => "no verdict from the statuses below",
    };
    let seen: Vec<String> = trail.iter().map(|(t, s)| format!("{s} at {t} s")).collect();
    let seen = if seen.is_empty() { format!("nothing in {} s", PROBE_FOLLOW.as_secs()) } else { seen.join(", then ") };
    format!("{meaning} ({seen})")
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

    /// By default a send goes to Singapore alone, on the documented `sendTransaction`
    /// path with revert protection on. The global host is Singapore under another name
    /// from here, so naming it sends to Singapore once, not twice.
    #[test]
    fn sends_go_to_singapore_alone_by_default() {
        let sg = "https://singapore.mainnet.block-engine.jito.wtf/api/v1/transactions?bundleOnly=true";
        assert_eq!(fanout_urls(DEFAULT_URL), vec![sg.to_string()]);
        assert_eq!(fanout_urls("https://mainnet.block-engine.jito.wtf/api/v1/transactions?bundleOnly=true"), vec![
            sg.to_string()
        ]);
        let fra = "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/transactions?bundleOnly=true";
        assert_eq!(fanout_urls(fra), vec![fra.to_string(), sg.to_string()], "a configured region is kept, first");
    }

    #[test]
    fn a_probe_trail_says_what_it_means() {
        let t = |v: &[(u64, &str)]| v.iter().map(|(s, w)| (*s, (*w).to_string())).collect::<Vec<_>>();
        assert!(describe(&t(&[(2, "Pending"), (4, "Landed")])).starts_with("LANDED"));
        assert!(describe(&t(&[(2, "Failed")])).starts_with("Failed"));
        assert!(describe(&t(&[(2, "Pending"), (62, "Invalid")])).contains("tip is the lever"));
        let invalid = describe(&t(&[(2, "Invalid")]));
        assert!(invalid.starts_with("Invalid throughout") && invalid.ends_with("(Invalid at 2 s)"), "{invalid}");
        assert!(describe(&[]).contains("nothing in 60 s"));
        assert!(describe(&t(&[(2, "query failed: 429")])).contains("no verdict"));
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
        assert!(is_bundle_only(DEFAULT_URL));
        assert!(fanout_urls(DEFAULT_URL).iter().all(|u| is_bundle_only(u)));
        assert!(!is_bundle_only("https://mainnet.block-engine.jito.wtf/api/v1/transactions"));
        assert!(!is_bundle_only("https://x.example/api/v1/transactions?bundleOnly=false"));
        assert!(!is_bundle_only("https://x.example/api/v1/transactions?notbundleOnly=true"));
        assert!(!is_bundle_only("not a url bundleOnly=true"));
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
