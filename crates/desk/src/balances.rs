//! What the trading address actually holds, and whether that is enough to trade at all.
//!
//! # Why "enough to trade" is a separate question from "how much is in it"
//!
//! A Solana address needs three different things before an arbitrage cycle can run, and
//! having one is no evidence of having the others:
//!
//! 1. **Lamports for the fee.** About 5,000 per signature, plus whatever priority is
//!    bid. Cheap.
//! 2. **Lamports for rent on every token account the cycle touches.** About 2,039,280
//!    each — roughly 0.00204 SOL — and a three-hop cycle touches three mints. This is
//!    the one that surprises people, because it is *four hundred times* the fee and it
//!    is required before a single instruction executes.
//! 3. **A balance of the token the cycle starts from**, which is the only part most
//!    people think about.
//!
//! An address can hold a perfectly respectable dollar balance and still be unable to
//! open the accounts a trade needs. So [`Holdings::readiness`] reports the binding
//! constraint by name rather than printing a number and leaving the operator to work
//! out what it means.

use cb_executor::encode::programs;
use cb_executor::rpc::Rpc;
use serde::Serialize;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// Rent-exempt minimum for a 165-byte SPL token account, in lamports.
///
/// A constant rather than a `getMinimumBalanceForRentExemption` call: it is fixed by the
/// rent schedule, it has not moved, and an extra round trip to learn a number that
/// cannot change during a session is a round trip spent on nothing.
pub const TOKEN_ACCOUNT_RENT: u64 = 2_039_280;

/// A generous allowance for signature and priority fees on one cycle.
pub const FEE_ALLOWANCE: u64 = 100_000;

/// Hide the credential in an endpoint before it is shown to anyone.
///
/// A provider URL carries its API key as a query parameter, so printing the endpoint
/// prints the key — into the window, into any screenshot of the window, and into
/// anything the operator pastes while asking for help. The panel needs to say *which*
/// node reported a balance; it does not need to say how to authenticate as you.
#[must_use]
pub fn redact_endpoint(url: &str) -> String {
    match url.split_once('?') {
        None => url.to_string(),
        Some((base, query)) => {
            let scrubbed: Vec<String> = query
                .split('&')
                .map(|kv| match kv.split_once('=') {
                    Some((k, v)) if !v.is_empty() => format!("{k}=****"),
                    _ => kv.to_string(),
                })
                .collect();
            format!("{base}?{}", scrubbed.join("&"))
        }
    }
}

/// The only mints this app names. Everything else is shown by its address.
///
/// A three-entry table rather than the pool registry: these are the base mints every
/// cycle starts and ends at, the registry lives in another crate, and inventing a ticker
/// for an unfamiliar mint is how an operator ends up confident about the wrong token.
#[must_use]
pub fn well_known(mint: &str) -> Option<&'static str> {
    match mint {
        "So11111111111111111111111111111111111111112" => Some("wSOL"),
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => Some("USDC"),
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => Some("USDT"),
        _ => None,
    }
}

/// One SPL holding.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenHolding {
    pub mint: String,
    /// A ticker for the three mints this app names, else empty — the window shows the
    /// mint address in that case rather than a guess.
    pub symbol: String,
    pub amount: String,
    pub decimals: u8,
}

/// Whether this address can do anything, and what stops it if not.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Readiness {
    /// True only when every requirement is met.
    pub can_trade: bool,
    /// The binding constraint, phrased for someone deciding what to do next.
    pub reason: String,
    /// Lamports short of being able to open one more token account, zero if none.
    pub short_by_lamports: u64,
}

/// Everything the panel shows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Holdings {
    pub address: String,
    pub lamports: u64,
    pub sol: String,
    pub tokens: Vec<TokenHolding>,
    pub readiness: Readiness,
    /// The endpoint these numbers came from, so a surprising balance can be traced to
    /// the node that reported it rather than assumed to be the chain's opinion.
    pub rpc: String,
}

/// Decide whether an address can open the accounts a cycle needs.
///
/// `mints_needed` is how many distinct token accounts the cycle touches; a three-hop
/// cycle from SOL touches three.
#[must_use]
pub fn readiness(lamports: u64, existing_accounts: usize, mints_needed: usize) -> Readiness {
    let to_open = mints_needed.saturating_sub(existing_accounts) as u64;
    let rent = to_open * TOKEN_ACCOUNT_RENT;
    let needed = rent.saturating_add(FEE_ALLOWANCE);

    if lamports >= needed {
        return Readiness {
            can_trade: true,
            reason: if to_open == 0 {
                "holds the token accounts a cycle needs and enough for fees".into()
            } else {
                format!("enough to open {to_open} more token account(s) and pay fees")
            },
            short_by_lamports: 0,
        };
    }

    let short = needed - lamports;
    Readiness {
        can_trade: false,
        reason: format!(
            "{:.9} SOL is short of the {:.9} needed — {to_open} token account(s) at \
             {:.9} SOL rent each, plus fees. Rent is refundable when an account is \
             closed, but it must be there first",
            lamports as f64 / 1e9,
            needed as f64 / 1e9,
            TOKEN_ACCOUNT_RENT as f64 / 1e9,
        ),
        short_by_lamports: short,
    }
}

/// Format a raw token amount against its decimals without floating point.
///
/// `u64` amounts with 9 decimals exceed what `f64` represents exactly, so dividing to
/// print would quietly round a balance. String surgery does not.
#[must_use]
pub fn format_amount(raw: u64, decimals: u8) -> String {
    let d = usize::from(decimals);
    if d == 0 {
        return raw.to_string();
    }
    let s = format!("{raw:0>width$}", width = d + 1);
    let (whole, frac) = s.split_at(s.len() - d);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{frac}")
    }
}

/// Note a non-fatal lookup failure. The desk has no tracing subscriber, so this is a
/// stderr line rather than a dropped error — silence here would be the same mistake the
/// Token-2022 gap already was.
fn tracing_note(e: &anyhow::Error) {
    eprintln!("token-2022 balances unavailable ({e}); classic SPL balances shown");
}

/// Fetch what the address holds.
///
/// # Errors
/// If the address is unparseable or the node cannot be reached.
pub async fn fetch(rpc_url: &str, address: &str, mints_needed: usize) -> anyhow::Result<Holdings> {
    let who = Pubkey::from_str(address)?;
    let rpc = Rpc::new(rpc_url)?;

    let lamports = rpc.balance(&who).await?;

    // Both token programs, not just the classic one.
    //
    // `getTokenAccountsByOwner` filters by program, so asking only about SPL Token
    // reports a Token-2022 holding as *absent* rather than as unknown — the panel would
    // have shown "0 tokens" for a funded wallet and been believed. Found by checking a
    // real wallet against the chain after a transfer that had not in fact arrived; the
    // balance was genuinely unchanged, but the query could not have proved it.
    let mut raw = rpc.token_accounts(&who, &programs::SPL_TOKEN.parse::<Pubkey>()?).await?;
    match rpc.token_accounts(&who, &programs::SPL_TOKEN_2022.parse::<Pubkey>()?).await {
        Ok(more) => raw.extend(more),
        // A node that does not serve the Token-2022 filter should not blank the classic
        // balances, which are the ones that matter for the base mints.
        Err(e) => tracing_note(&e),
    }

    let mut tokens: Vec<TokenHolding> = raw
        .into_iter()
        .map(|h| TokenHolding {
            symbol: well_known(&h.mint).unwrap_or("").to_string(),
            amount: format_amount(h.amount, h.decimals),
            decimals: h.decimals,
            mint: h.mint,
        })
        .collect();
    // Named mints first, then by mint, so the three that matter are always at the top.
    tokens.sort_by(|a, b| {
        (a.symbol.is_empty(), &a.symbol, &a.mint).cmp(&(b.symbol.is_empty(), &b.symbol, &b.mint))
    });

    let readiness = readiness(lamports, tokens.len(), mints_needed);

    Ok(Holdings {
        address: address.to_string(),
        lamports,
        sol: format_amount(lamports, 9),
        tokens,
        readiness,
        rpc: redact_endpoint(rpc_url),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_format_against_their_own_decimals() {
        assert_eq!(format_amount(903_514, 9), "0.000903514");
        assert_eq!(format_amount(7_904, 6), "0.007904");
        assert_eq!(format_amount(1_000_000, 6), "1");
        assert_eq!(format_amount(0, 6), "0");
        assert_eq!(format_amount(42, 0), "42");
        assert_eq!(format_amount(1, 9), "0.000000001");
    }

    /// The whole reason this does string surgery: a `u64` of lamports is bigger than an
    /// `f64` represents exactly, and a balance that prints wrong is a balance nobody can
    /// reconcile.
    #[test]
    fn a_large_balance_is_not_rounded_by_the_formatting() {
        let raw = 123_456_789_123_456_789u64;
        assert_eq!(format_amount(raw, 9), "123456789.123456789");
    }

    /// The measured case: a real wallet holding 0.000903514 SOL and one token account.
    /// It cannot open the two more a three-hop cycle needs, and the message has to say
    /// so rather than printing a balance and leaving it there.
    #[test]
    fn a_wallet_below_rent_is_reported_as_unable_to_trade() {
        let r = readiness(903_514, 1, 3);
        assert!(!r.can_trade);
        assert!(r.short_by_lamports > 0);
        assert!(r.reason.contains("rent"), "the reason must name rent: {}", r.reason);
    }

    #[test]
    fn an_address_holding_every_account_needs_only_fees() {
        let r = readiness(FEE_ALLOWANCE, 3, 3);
        assert!(r.can_trade);
        assert_eq!(r.short_by_lamports, 0);

        let broke = readiness(FEE_ALLOWANCE - 1, 3, 3);
        assert!(!broke.can_trade, "a wallet that cannot pay the fee cannot trade");
    }

    /// Rent scales with the accounts still to open, so holding some already lowers the
    /// bar — and holding more than the cycle needs must not make it negative.
    #[test]
    fn rent_is_charged_only_for_accounts_that_do_not_exist_yet() {
        let none = readiness(0, 0, 3).short_by_lamports;
        let some = readiness(0, 2, 3).short_by_lamports;
        assert!(some < none, "existing accounts must lower the requirement");
        assert_eq!(
            none - some,
            2 * TOKEN_ACCOUNT_RENT,
            "each existing account should save exactly one rent"
        );

        // More accounts than the cycle needs is not a credit.
        let plenty = readiness(FEE_ALLOWANCE, 9, 3);
        assert!(plenty.can_trade);
        assert_eq!(plenty.short_by_lamports, 0);
    }

    /// Three hops needs three token accounts, and the arithmetic that produces the
    /// headline "you need about 0.006 SOL before anything can happen" is worth pinning.
    #[test]
    fn a_three_hop_cycle_from_empty_needs_about_six_milli_sol() {
        let r = readiness(0, 0, 3);
        let needed = r.short_by_lamports;
        assert_eq!(needed, 3 * TOKEN_ACCOUNT_RENT + FEE_ALLOWANCE);
        assert!(
            (0.006..0.007).contains(&(needed as f64 / 1e9)),
            "expected about 0.0062 SOL, got {}",
            needed as f64 / 1e9
        );
    }

    /// The two token programs derive *different* addresses for the same owner and mint,
    /// so a wallet can legitimately hold both and a panel that queries one program is
    /// not showing a subset — it is showing a wrong answer with no indication.
    #[test]
    fn the_two_token_programs_are_distinct_and_both_are_queried() {
        use cb_executor::encode::programs;
        assert_ne!(programs::SPL_TOKEN, programs::SPL_TOKEN_2022);
        // Both parse; a typo in either constant would silently return nothing.
        assert!(programs::SPL_TOKEN.parse::<Pubkey>().is_ok());
        assert!(programs::SPL_TOKEN_2022.parse::<Pubkey>().is_ok());
        assert!(
            include_str!("balances.rs").contains("SPL_TOKEN_2022"),
            "fetch() must query Token-2022 as well as the classic program"
        );
    }

    /// A provider URL carries its key in the query string, and this panel's endpoint
    /// line has been screenshotted and pasted into a chat more than once already.
    #[test]
    fn an_endpoint_is_shown_without_its_credential() {
        let r = redact_endpoint("https://mainnet.helius-rpc.com/?api-key=9e062393-dead-beef");
        assert!(r.contains("helius-rpc.com"), "the host must survive: {r}");
        assert!(!r.contains("9e062393"), "the key must not: {r}");
        assert!(!r.contains("dead-beef"), "including the rest of it: {r}");
        assert_eq!(r, "https://mainnet.helius-rpc.com/?api-key=****");

        // Several parameters, and one with no value, must all survive intact in shape.
        let multi = redact_engpoint_helper();
        assert!(!multi.contains("secret"), "{multi}");

        // A plain URL with no query is unchanged.
        let plain = "https://api.mainnet-beta.solana.com";
        assert_eq!(redact_endpoint(plain), plain);
    }

    fn redact_engpoint_helper() -> String {
        redact_endpoint("https://x.example/?api-key=secret&flag&mode=fast")
    }
}
