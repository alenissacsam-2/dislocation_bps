//! Hiding a credential in a URL before it goes anywhere it might be seen.
//!
//! # Why this exists as a shared, single implementation
//!
//! A provider's RPC endpoint carries its API key as a query parameter, and this
//! codebase logs endpoints in more than one place — the feed announcing what it
//! connected to, the bot announcing what it will read from, the desk app showing where
//! a balance came from. Three call sites and one leak in any of them is the whole
//! guarantee gone: a Helius URL reached `cb-bot.log` in plain text this way, eleven
//! times, in a live run, before this module existed to stop it. One function, reused
//! everywhere an endpoint is ever printed, is what makes "no key in a log" a property
//! of the codebase rather than a habit someone has to remember at each site.

/// Replace every query-parameter *value* in a URL with `****`, keeping the host and
/// the parameter names.
///
/// Redacting names too would make an endpoint impossible to distinguish from another
/// in the same log; keeping the plain path and host is enough to say which provider
/// answered without saying how to authenticate as the account that pays for it.
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

/// Redact every URL literal found anywhere inside a blob of text.
///
/// For error messages, not clean endpoint strings. `reqwest`'s own errors embed the
/// full destination URL in their `Display` text — `"error sending request for url
/// (https://.../?api-key=XXX)"` — with no fixed structure around it and no guarantee
/// the format stays the same across versions. Rather than parse that shape, this scans
/// for anything starting `http(s)://` or `ws(s)://`, finds where it plausibly ends (the
/// next whitespace, quote, or bracket), and redacts that whole substring through
/// [`redact_endpoint`]. A message with no URL in it passes through unchanged.
#[must_use]
pub fn redact_urls_in(text: &str) -> String {
    const SCHEMES: &[&str] = &["https://", "http://", "wss://", "ws://"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    'outer: loop {
        // The earliest scheme occurrence, so URLs are redacted in the order they
        // appear rather than in scheme-list order.
        let mut earliest: Option<(usize, &str)> = None;
        for scheme in SCHEMES {
            if let Some(i) = rest.find(scheme) {
                if earliest.is_none_or(|(j, _)| i < j) {
                    earliest = Some((i, scheme));
                }
            }
        }
        let Some((i, _)) = earliest else {
            out.push_str(rest);
            break 'outer;
        };

        out.push_str(&rest[..i]);
        let url_start = &rest[i..];
        let end = url_start
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ','))
            .unwrap_or(url_start.len());
        out.push_str(&redact_endpoint(&url_start[..end]));
        rest = &url_start[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape of the leak this module fixes: a Helius URL, logged verbatim.
    #[test]
    fn a_provider_key_does_not_survive_redaction() {
        let r = redact_endpoint("wss://mainnet.helius-rpc.com/?api-key=9e062393-dead-beef");
        assert!(r.contains("helius-rpc.com"), "the host must survive: {r}");
        assert!(!r.contains("9e062393"), "the key must not: {r}");
        assert_eq!(r, "wss://mainnet.helius-rpc.com/?api-key=****");
    }

    #[test]
    fn several_parameters_and_a_bare_flag_are_all_handled() {
        let r = redact_endpoint("https://x.example/?api-key=secret&flag&mode=fast");
        assert!(!r.contains("secret"));
        assert_eq!(r, "https://x.example/?api-key=****&flag&mode=****");
    }

    #[test]
    fn a_url_with_no_query_is_unchanged() {
        let plain = "https://api.mainnet-beta.solana.com";
        assert_eq!(redact_endpoint(plain), plain);
    }

    /// The exact shape a real error took this session: reqwest's own message, with the
    /// full endpoint — key included — sitting inside parentheses in free-form text.
    #[test]
    fn a_url_embedded_in_an_error_messages_prose_is_redacted() {
        let msg = "rpc: getBalance request failed\n\nCaused by:\n    0: error sending \
                   request for url (https://mainnet.helius-rpc.com/?api-key=9e06live): \
                   connection closed before message completed";
        let r = redact_urls_in(msg);
        assert!(!r.contains("9e06live"), "{r}");
        assert!(r.contains("getBalance request failed"), "surrounding text must survive: {r}");
        assert!(r.contains("connection closed"), "surrounding text must survive: {r}");
        assert!(r.contains("helius-rpc.com"), "the host is fine to keep: {r}");
    }

    #[test]
    fn text_with_no_url_at_all_is_returned_unchanged() {
        let msg = "trading halted: 3 trades failed in a row";
        assert_eq!(redact_urls_in(msg), msg);
    }

    /// Two different endpoints failing in the same message must both be caught, not
    /// just the first.
    #[test]
    fn every_url_in_a_message_is_redacted_not_just_the_first() {
        let msg = "tried https://a.example/?api-key=one and https://b.example/?api-key=two";
        let r = redact_urls_in(msg);
        assert!(!r.contains("one") && !r.contains("two"), "{r}");
        assert!(r.contains("a.example") && r.contains("b.example"), "{r}");
    }
}
