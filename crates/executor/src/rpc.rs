//! A small JSON-RPC client, carrying only the five calls execution needs.
//!
//! Not `solana-client`. That crate brings a large dependency tree and a blocking/async
//! split this project does not need, and the rest of the codebase already talks to
//! mainnet over `reqwest` and `tokio-tungstenite` directly. Five methods is less code
//! than the adapter would be.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use solana_sdk::{hash::Hash, pubkey::Pubkey};
use std::str::FromStr;
use std::time::Duration;

/// Solana's own default. A request that has not answered in this long has lost the race
/// it was submitted for, whatever it eventually says.
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Rpc {
    url: String,
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
        Ok(Self {
            url: url.into(),
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let resp: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method} request failed"))?
            .json()
            .await
            .with_context(|| format!("{method} returned something that is not JSON"))?;

        if let Some(e) = resp.get("error") {
            // The node's own message is more useful than anything wrapped around it.
            bail!("{method} failed: {e}");
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{method} returned neither a result nor an error"))
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
        let r = self.call("sendTransaction", json!([tx_base64, cfg])).await?;
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

    /// # Errors
    /// If the call fails.
    pub async fn balance(&self, who: &Pubkey) -> Result<u64> {
        let r = self
            .call("getBalance", json!([who.to_string(), {"commitment": "confirmed"}]))
            .await?;
        Ok(r["value"].as_u64().unwrap_or(0))
    }
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
}
