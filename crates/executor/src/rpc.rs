//! A small JSON-RPC client, carrying only the five calls execution needs.
//!
//! Not `solana-client`. That crate brings a large dependency tree and a blocking/async
//! split this project does not need, and the rest of the codebase already talks to
//! mainnet over `reqwest` and `tokio-tungstenite` directly. Five methods is less code
//! than the adapter would be.

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use solana_sdk::{hash::Hash, pubkey::Pubkey};
use std::str::FromStr;
use std::time::Duration;

/// Solana's own default. A request that has not answered in this long has lost the race
/// it was submitted for, whatever it eventually says.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long an unused connection may sit in the pool before it is discarded.
///
/// Shorter than any endpoint's own keep-alive, so this side closes first and never
/// writes into a socket the far end has already dropped.
const POOL_IDLE: Duration = Duration::from_secs(3);

pub struct Rpc {
    /// Endpoints in preference order. Reads fail over down this list; sends never do.
    endpoints: Vec<String>,
    /// Which endpoint reads start from, so a provider that has just failed is not
    /// retried first on every subsequent call.
    cursor: std::sync::atomic::AtomicUsize,
    http: reqwest::Client,
}

/// What a simulation said would happen.
#[derive(Debug, Clone)]
pub struct Simulation {
    /// `None` means the transaction executed without error.
    pub err: Option<String>,
    pub logs: Vec<String>,
    pub units_consumed: Option<u64>,
    /// Post-execution lamport balances, in the order the addresses were requested.
    pub post_lamports: Vec<u64>,
    /// Post-execution SPL token amounts, for those requested accounts that are token
    /// accounts. `None` where the account is not one, or did not exist.
    pub post_token_amounts: Vec<Option<u64>>,
}

impl Simulation {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.err.is_none()
    }
}

impl Rpc {
    /// # Errors
    /// If the HTTP client cannot be constructed.
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Self::with_fallbacks(vec![url.into()])
    }

    /// Build a client that fails over across several endpoints.
    ///
    /// # Errors
    /// If the list is empty or the HTTP client cannot be constructed.
    pub fn with_fallbacks(endpoints: Vec<String>) -> Result<Self> {
        let endpoints: Vec<String> =
            endpoints.into_iter().map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect();
        ensure!(!endpoints.is_empty(), "an RPC client needs at least one endpoint");
        Ok(Self {
            endpoints,
            cursor: std::sync::atomic::AtomicUsize::new(0),
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                // Drop idle connections well before the server does.
                //
                // reqwest keeps them for 90 s by default and public Solana endpoints
                // close them far sooner, so a burst of calls with a pause in the middle
                // reuses a socket the far end has already hung up: "connection closed
                // before message completed", a *transport* error rather than an HTTP
                // one. It surfaced as the fourth call in a sequence failing every time
                // while the first three succeeded, and it would have hit the running bot
                // the same way — intermittently, mid-trade, on whichever call happened
                // to land after a quiet moment.
                //
                // Deliberately not solved by retrying. `send` is on this client too, and
                // a retried `sendTransaction` that actually arrived the first time
                // submits the same trade twice.
                .pool_idle_timeout(POOL_IDLE)
                .build()?,
        })
    }

    /// How many times a rate-limited request is retried before giving up.
    ///
    /// Only a 429 is retried, and only with a growing delay. Retrying anything else
    /// would be wrong here: this client is used to submit transactions, and a request
    /// that failed for an unknown reason may have been received.
    const RATE_LIMIT_RETRIES: u32 = 4;

    /// How many endpoints deep a read will go before giving up.
    ///
    /// Every endpoint gets one chance per call. Going round twice would turn a
    /// provider-wide outage into a long stall on a loop that runs at 5 Hz.
    fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// One attempt against one endpoint.
    async fn call_on(&self, url: &str, method: &str, params: &Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});

        let mut backoff = Duration::from_millis(400);
        for attempt in 0..=Self::RATE_LIMIT_RETRIES {
            let resp = match self.http.post(url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    // Not `.with_context(...)?`. reqwest embeds the destination URL —
                    // API key included — directly in the error's own `Display`, and
                    // `with_context` only wraps: anyhow's `{:#}` still walks the source
                    // chain and prints the original message underneath the wrapper. A
                    // Helius key reached `cb-bot.log` this way. Building a fresh error
                    // from redacted text, rather than chaining the original, is the only
                    // way to be sure the raw URL cannot resurface later from a `{:#}` or
                    // a `.source()` walk anywhere downstream.
                    bail!(
                        "{method} request failed: {}",
                        cb_core::redact::redact_urls_in(&e.to_string())
                    );
                }
            };

            // A provider under load answers with an HTML error page, and serde's
            // complaint about that is unreadable. Measured: 48 execution attempts died
            // on `getMultipleAccounts returned something that is not JSON`, which was
            // the public endpoint rate-limiting mid-trade.
            let status = resp.status();
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => bail!(
                    "{method}: no body: {}",
                    cb_core::redact::redact_urls_in(&e.to_string())
                ),
            };
            let parsed: Value = serde_json::from_str(&text).map_err(|_| {
                anyhow!(
                    "{method} returned {} rather than JSON ({} bytes) — the endpoint is \
                     probably rate limiting",
                    status,
                    text.len()
                )
            })?;

            if let Some(e) = parsed.get("error") {
                let rate_limited = e.get("code").and_then(Value::as_i64) == Some(429);
                if rate_limited && attempt < Self::RATE_LIMIT_RETRIES {
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                    continue;
                }
                bail!("{method} failed: {e}");
            }
            return parsed
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("{method} returned neither a result nor an error"));
        }
        bail!("{method} was rate limited {} times running", Self::RATE_LIMIT_RETRIES + 1)
    }

    /// The endpoint index the next call should start from, advancing the shared
    /// rotation by one on every call — including a call that goes on to succeed.
    ///
    /// This is the difference between rotation and failover. The first version of this
    /// client only advanced on *failure*, which is sticky: once endpoint 0 answers
    /// once, every subsequent call keeps going to endpoint 0 forever, and the other two
    /// configured keys sit completely idle unless it breaks. That concentrates every
    /// request — and every free-tier rate limit — onto whichever provider happened to
    /// answer first. Advancing here regardless of outcome means three configured keys
    /// each carry roughly a third of the read traffic, which is what actually reduces
    /// the chance of hitting any one provider's limit rather than merely reacting to it
    /// once it has already happened.
    ///
    /// A single atomic counter, so concurrent callers each get a distinct, correctly
    /// incrementing start index rather than racing to read-then-write the same value.
    fn next_start(&self) -> usize {
        use std::sync::atomic::Ordering;
        self.cursor.fetch_add(1, Ordering::Relaxed) % self.endpoint_count()
    }

    /// A read, rotating across the configured endpoints and failing over within one
    /// call if the one it started on does not answer.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let n = self.endpoint_count();
        let start = self.next_start();
        let mut last: Option<anyhow::Error> = None;

        for step in 0..n {
            let idx = (start + step) % n;
            match self.call_on(&self.endpoints[idx], method, &params).await {
                Ok(v) => {
                    if step > 0 {
                        // Announced, because a silent skip hides a provider that has
                        // quietly stopped answering.
                        tracing::warn!(
                            "rpc failed over to endpoint {} of {n} for {method}",
                            idx + 1
                        );
                    }
                    return Ok(v);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("{method}: no endpoints configured")))
            .with_context(|| format!("all {n} endpoint(s) failed"))
    }

    /// # Errors
    /// If the call fails or the returned blockhash is not parseable.
    pub async fn latest_blockhash(&self) -> Result<(Hash, u64)> {
        let r = self
            .call("getLatestBlockhash", json!([{"commitment": "confirmed"}]))
            .await?;
        let bh = r["value"]["blockhash"]
            .as_str()
            .ok_or_else(|| anyhow!("no blockhash in response"))?;
        let valid_until = r["value"]["lastValidBlockHeight"].as_u64().unwrap_or(0);
        Ok((Hash::from_str(bh).context("unparseable blockhash")?, valid_until))
    }

    /// Run the transaction against the node's current state without submitting it.
    ///
    /// `watch` names accounts whose post-execution balances should come back, which is
    /// how the caller checks that the trade actually gained what it was quoted rather
    /// than trusting its own arithmetic.
    ///
    /// # Errors
    /// If the call fails or the response cannot be read.
    pub async fn simulate(&self, tx_base64: &str, watch: &[Pubkey]) -> Result<Simulation> {
        let addresses: Vec<String> = watch.iter().map(ToString::to_string).collect();
        let cfg = json!({
            "encoding": "base64",
            // The transaction is signed, but a simulation that verifies signatures cannot
            // use a replaced blockhash, and a stale blockhash fails for the wrong reason.
            "sigVerify": false,
            "replaceRecentBlockhash": true,
            "commitment": "confirmed",
            "accounts": { "encoding": "base64", "addresses": addresses },
        });
        let r = self.call("simulateTransaction", json!([tx_base64, cfg])).await?;
        let v = &r["value"];

        let err = match &v["err"] {
            Value::Null => None,
            e => Some(e.to_string()),
        };
        let logs = v["logs"]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let mut post_lamports = Vec::new();
        let mut post_token_amounts = Vec::new();
        if let Some(accs) = v["accounts"].as_array() {
            for a in accs {
                post_lamports.push(a["lamports"].as_u64().unwrap_or(0));
                post_token_amounts.push(token_amount_of(a));
            }
        }

        Ok(Simulation {
            err,
            logs,
            units_consumed: v["unitsConsumed"].as_u64(),
            post_lamports,
            post_token_amounts,
        })
    }

    /// # Errors
    /// If the node rejects the transaction.
    pub async fn send(&self, tx_base64: &str, skip_preflight: bool) -> Result<String> {
        let cfg = json!({
            "encoding": "base64",
            // Preflight is a second simulation. Execution has already run one against
            // the same node, so paying for another costs a round trip in a race that is
            // decided in milliseconds.
            "skipPreflight": skip_preflight,
            "maxRetries": 0,
            "preflightCommitment": "confirmed",
        });
        // Deliberately NOT `call`: no failover, and no retry, on this one call. Which
        // endpoint carries it still rotates — `next_start` advances the same shared
        // counter reads use, so sends spread across the configured providers over time
        // exactly as reads do — but once chosen, that single endpoint gets exactly one
        // attempt.
        //
        // A transaction that times out may still have been received. Sending it again
        // to a second endpoint is how the same trade gets submitted twice, and the
        // second copy is not free — it is a second real transaction against a wallet
        // that has already moved. A failed send is reported as failed; deciding what
        // to do about it needs the signature status, not another attempt.
        let idx = self.next_start();
        let params = json!([tx_base64, cfg]);
        let r = self.call_on(&self.endpoints[idx], "sendTransaction", &params).await?;
        r.as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("sendTransaction did not return a signature"))
    }

    /// `Ok(None)` means the node has not seen it yet, which is not the same as failure.
    ///
    /// # Errors
    /// If the call fails.
    pub async fn signature_status(&self, sig: &str) -> Result<Option<SignatureStatus>> {
        let r = self
            .call(
                "getSignatureStatuses",
                json!([[sig], {"searchTransactionHistory": false}]),
            )
            .await?;
        let Some(first) = r["value"].as_array().and_then(|a| a.first()) else {
            return Ok(None);
        };
        if first.is_null() {
            return Ok(None);
        }
        Ok(Some(SignatureStatus {
            confirmations: first["confirmations"].as_u64(),
            err: match &first["err"] {
                Value::Null => None,
                e => Some(e.to_string()),
            },
        }))
    }

    /// Fetch several accounts in one round trip.
    ///
    /// `None` where the account does not exist, which is a normal answer rather than an
    /// error — a tick array that has never been initialised is absent, not broken.
    ///
    /// # Errors
    /// If the call fails or the response is not the expected shape.
    pub async fn accounts(&self, keys: &[Pubkey]) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let addresses: Vec<String> = keys.iter().map(ToString::to_string).collect();
        let r = self
            .call(
                "getMultipleAccounts",
                json!([addresses, {"encoding": "base64", "commitment": "confirmed"}]),
            )
            .await?;
        let arr = r["value"]
            .as_array()
            .ok_or_else(|| anyhow!("getMultipleAccounts returned no account array"))?;
        Ok(arr
            .iter()
            .map(|a| {
                if a.is_null() {
                    return None;
                }
                a["data"]
                    .as_array()
                    .and_then(|d| d.first())
                    .and_then(Value::as_str)
                    .and_then(base64_decode)
            })
            .collect())
    }

    /// Fetch several accounts with their owning programs, in one round trip.
    ///
    /// The owner is what distinguishes a real token account from arbitrary bytes that
    /// happen to be the right length, and fetching it alongside the data rather than in
    /// a second call matters: a verification pass over the whole registry is four round
    /// trips per pool if these are separate, which is both slow and enough traffic to
    /// be rate-limited off a public endpoint part way through.
    ///
    /// # Errors
    /// If the call fails or the response is not the expected shape.
    pub async fn accounts_full(&self, keys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let addresses: Vec<String> = keys.iter().map(ToString::to_string).collect();
        let r = self
            .call(
                "getMultipleAccounts",
                json!([addresses, {"encoding": "base64", "commitment": "confirmed"}]),
            )
            .await?;
        let arr = r["value"]
            .as_array()
            .ok_or_else(|| anyhow!("getMultipleAccounts returned no account array"))?;
        Ok(arr
            .iter()
            .map(|a| {
                if a.is_null() {
                    return None;
                }
                let data = a["data"]
                    .as_array()
                    .and_then(|d| d.first())
                    .and_then(Value::as_str)
                    .and_then(base64_decode)?;
                let owner = a["owner"].as_str().and_then(|o| Pubkey::from_str(o).ok())?;
                Some(Account { owner, data, lamports: a["lamports"].as_u64().unwrap_or(0) })
            })
            .collect())
    }

    /// Every token account `owner` holds under `token_program`.
    ///
    /// Uses `jsonParsed`, which is the one place this client lets the node do the
    /// decoding. Everywhere else the raw bytes are read here on purpose, because a
    /// number that decides a trade should not depend on a node's parser. A balance
    /// shown in a panel is not that, and asking for parsed output avoids fetching and
    /// decoding the mint of every account to learn its decimals.
    ///
    /// # Errors
    /// If the call fails or the response is not the expected shape.
    pub async fn token_accounts(
        &self,
        owner: &Pubkey,
        token_program: &Pubkey,
    ) -> Result<Vec<TokenHolding>> {
        let r = self
            .call(
                "getTokenAccountsByOwner",
                json!([
                    owner.to_string(),
                    { "programId": token_program.to_string() },
                    { "encoding": "jsonParsed", "commitment": "confirmed" }
                ]),
            )
            .await?;
        let arr = r["value"]
            .as_array()
            .ok_or_else(|| anyhow!("getTokenAccountsByOwner returned no array"))?;
        Ok(arr
            .iter()
            .filter_map(|a| {
                let info = &a["account"]["data"]["parsed"]["info"];
                let mint = info["mint"].as_str()?.to_string();
                // `amount` is a decimal string because it does not fit a JSON number.
                let amount = info["tokenAmount"]["amount"].as_str()?.parse().ok()?;
                let decimals = u8::try_from(info["tokenAmount"]["decimals"].as_u64()?).ok()?;
                Some(TokenHolding { mint, amount, decimals })
            })
            .collect())
    }

    /// The owner program of an account, which is how a mint's token program is known.
    ///
    /// # Errors
    /// If the call fails.
    pub async fn owner_of(&self, key: &Pubkey) -> Result<Option<Pubkey>> {
        let r = self
            .call(
                "getAccountInfo",
                json!([key.to_string(), {"encoding": "base64", "commitment": "confirmed"}]),
            )
            .await?;
        let Some(owner) = r["value"]["owner"].as_str() else {
            return Ok(None);
        };
        Ok(Pubkey::from_str(owner).ok())
    }

    /// # Errors
    /// If the call fails.
    pub async fn balance(&self, who: &Pubkey) -> Result<u64> {
        let r = self
            .call("getBalance", json!([who.to_string(), {"commitment": "confirmed"}]))
            .await?;
        Ok(r["value"].as_u64().unwrap_or(0))
    }
}

/// One SPL holding, with the decimals needed to render it.
///
/// The decimals come from the node's parsed output rather than from a registry, so an
/// unfamiliar mint renders correctly instead of being shown at a guessed scale — nine
/// decimals applied to a USDC balance is off by a thousand.
#[derive(Debug, Clone)]
pub struct TokenHolding {
    pub mint: String,
    pub amount: u64,
    pub decimals: u8,
}

/// An account as the node returned it.
#[derive(Debug, Clone)]
pub struct Account {
    /// The program that owns it. For a token account, a token program.
    pub owner: Pubkey,
    pub data: Vec<u8>,
    pub lamports: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignatureStatus {
    pub confirmations: Option<u64>,
    pub err: Option<String>,
}

impl SignatureStatus {
    #[must_use]
    pub fn landed_cleanly(&self) -> bool {
        self.err.is_none()
    }
}

/// Pull the SPL amount out of a returned account, if it is a token account at all.
///
/// The node hands back the raw account; a token account's amount is a little-endian u64
/// at offset 64 of its 165-byte layout. Anything else is not a token account and gets
/// `None` rather than a misread number.
fn token_amount_of(account: &Value) -> Option<u64> {
    const SPL_TOKEN_ACCOUNT_LEN: usize = 165;
    const AMOUNT_OFFSET: usize = 64;

    let data = account["data"].as_array()?;
    let b64 = data.first()?.as_str()?;
    let raw = base64_decode(b64)?;
    if raw.len() < SPL_TOKEN_ACCOUNT_LEN {
        return None;
    }
    let bytes: [u8; 8] = raw[AMOUNT_OFFSET..AMOUNT_OFFSET + 8].try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Minimal base64, so the crate does not take a dependency for two call sites.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, c) in TABLE.iter().enumerate() {
        rev[*c as usize] = u8::try_from(i).ok()?;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = rev[c as usize];
        if v == 255 {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

/// Decode, for tests in sibling modules that need to inspect their own output.
#[cfg(test)]
#[must_use]
pub fn base64_decode_for_test(s: &str) -> Option<Vec<u8>> {
    base64_decode(s)
}

/// Base64 encode, for putting a serialised transaction on the wire.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_including_both_padding_cases() {
        for case in [
            &b""[..],
            &b"f"[..],
            &b"fo"[..],
            &b"foo"[..],
            &b"foob"[..],
            &b"fooba"[..],
            &b"foobar"[..],
        ] {
            let enc = base64_encode(case);
            let dec = base64_decode(&enc).expect("own output must decode");
            assert_eq!(dec, case, "round trip failed for {case:?} via {enc}");
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_rejects_characters_outside_the_alphabet() {
        assert!(base64_decode("!!!!").is_none());
    }

    /// A short account is not a token account, and guessing would produce a number that
    /// looks plausible and is not a balance.
    #[test]
    fn a_non_token_account_yields_no_amount_rather_than_a_wrong_one() {
        let short = json!({"data": [base64_encode(&[0u8; 16]), "base64"]});
        assert_eq!(token_amount_of(&short), None);
        assert_eq!(token_amount_of(&json!({})), None);
    }

    #[test]
    fn a_token_account_amount_is_read_from_the_right_offset() {
        let mut raw = vec![0u8; 165];
        raw[64..72].copy_from_slice(&1_234_567_890u64.to_le_bytes());
        let acc = json!({"data": [base64_encode(&raw), "base64"]});
        assert_eq!(token_amount_of(&acc), Some(1_234_567_890));
    }

    /// Failover exists for reads. It must never apply to a send: a transaction that
    /// timed out may still have been received, and a second copy is a second real
    /// trade against a wallet that has already moved.
    #[test]
    fn send_is_pinned_to_one_endpoint_while_reads_are_not() {
        let src = include_str!("rpc.rs");
        let send_fn = &src[src.find("pub async fn send(").expect("send exists")..];
        let body = &send_fn[..send_fn.find("\n    }").expect("body ends")];
        assert!(
            body.contains("call_on("),
            "send must use the single-endpoint path"
        );
        assert!(
            !body.contains("self.call(\""),
            "send must not use the failing-over path"
        );
    }

    /// The actual point of rotation: it advances on every call, including calls that
    /// go on to succeed. The previous version only advanced on failure, which left one
    /// endpoint carrying all traffic forever once it had answered once — the other two
    /// configured keys sitting idle unless the first one broke.
    #[test]
    fn rotation_advances_on_every_call_not_only_on_failure() {
        let rpc = Rpc::with_fallbacks(vec![
            "https://a.example".into(),
            "https://b.example".into(),
            "https://c.example".into(),
        ])
        .unwrap();
        let seq: Vec<usize> = (0..6).map(|_| rpc.next_start()).collect();
        assert_eq!(seq, vec![0, 1, 2, 0, 1, 2], "three endpoints must cycle evenly: {seq:?}");
    }

    /// A single configured endpoint must not panic the modulo, and must keep handing
    /// back the only index there is.
    #[test]
    fn rotation_with_one_endpoint_always_returns_it() {
        let rpc = Rpc::with_fallbacks(vec!["https://only.example".into()]).unwrap();
        for _ in 0..5 {
            assert_eq!(rpc.next_start(), 0);
        }
    }

    /// Concurrent callers must each get a distinct, correctly-wrapping index rather
    /// than racing to read-then-write the same counter and duplicating one.
    #[tokio::test]
    async fn concurrent_rotation_still_visits_every_endpoint_evenly() {
        let rpc = std::sync::Arc::new(
            Rpc::with_fallbacks(vec!["https://a.example".into(), "https://b.example".into()])
                .unwrap(),
        );
        let mut tasks = Vec::new();
        for _ in 0..40 {
            let r = rpc.clone();
            tasks.push(tokio::spawn(async move { r.next_start() }));
        }
        let mut counts = [0usize; 2];
        for t in tasks {
            counts[t.await.unwrap()] += 1;
        }
        assert_eq!(counts[0] + counts[1], 40, "every call must land on a real index");
        assert_eq!(counts[0], counts[1], "an even split across the two endpoints: {counts:?}");
    }

    #[test]
    fn a_client_needs_at_least_one_endpoint() {
        assert!(Rpc::with_fallbacks(vec![]).is_err());
        assert!(Rpc::with_fallbacks(vec!["   ".into(), String::new()]).is_err());
        // Blanks are dropped, not counted.
        let r = Rpc::with_fallbacks(vec!["https://a.example".into(), "  ".into()]).unwrap();
        assert_eq!(r.endpoint_count(), 1);
    }

    #[test]
    fn endpoints_keep_their_order_and_duplicates_are_the_callers_business() {
        let r = Rpc::with_fallbacks(vec![
            " https://one.example ".into(),
            "https://two.example".into(),
        ])
        .unwrap();
        assert_eq!(r.endpoint_count(), 2);
        assert_eq!(r.endpoints[0], "https://one.example", "whitespace is trimmed");
    }

    /// End to end, not just unit-level: a real connection failure against a URL
    /// carrying a fake credential must not surface that credential anywhere in the
    /// resulting error's text, including through `{:#}` which walks the whole chain.
    ///
    /// Port 1 is reserved and nothing listens there, so this fails fast without
    /// needing the network to be reachable or unreachable in any particular way.
    #[tokio::test]
    async fn a_real_connection_failure_does_not_leak_the_endpoints_key() {
        let rpc = Rpc::with_fallbacks(vec![
            "http://127.0.0.1:1/?api-key=SUPER-SECRET-VALUE".to_string(),
        ])
        .unwrap();
        let err = rpc.balance(&Pubkey::new_unique()).await.unwrap_err();
        let full = format!("{err:#}");
        assert!(
            !full.contains("SUPER-SECRET-VALUE"),
            "the key leaked into a real connection error: {full}"
        );
    }
}
