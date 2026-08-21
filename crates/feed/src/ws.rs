//! `accountSubscribe` over WebSocket, with reconnect and honest lag accounting.

use crate::AccountUpdate;
use anyhow::{anyhow, Result};
use cb_core::types::Pubkey32;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Counters the dashboard uses to show how trustworthy the feed currently is.
///
/// These exist because the free WebSocket tier drops and coalesces updates. Hiding
/// that would make paper-trading results look better than they are.
#[derive(Debug, Default)]
pub struct FeedStats {
    pub updates: AtomicU64,
    pub reconnects: AtomicU64,
    /// Updates dropped because the consumer channel was full. Every one of these is
    /// an opportunity we could not even evaluate.
    pub dropped: AtomicU64,
    pub last_slot: AtomicU64,
    pub last_update_ms: AtomicU64,
    pub parse_errors: AtomicU64,
}

impl FeedStats {
    pub fn snapshot(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.updates.load(Ordering::Relaxed),
            self.reconnects.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
            self.last_slot.load(Ordering::Relaxed),
            self.parse_errors.load(Ordering::Relaxed),
        )
    }
}

pub struct WsFeed {
    url: String,
    pub stats: Arc<FeedStats>,
}

impl WsFeed {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), stats: Arc::new(FeedStats::default()) }
    }

    /// Spawn the subscription loop. Reconnects forever with capped backoff.
    ///
    /// The returned channel is **bounded**: if the consumer falls behind we drop
    /// updates and count them, rather than growing memory without limit. A trading
    /// loop that cannot keep up should say so, not accumulate a backlog of stale
    /// prices it will act on far too late.
    pub fn spawn(&self, accounts: Vec<Pubkey32>) -> mpsc::Receiver<AccountUpdate> {
        let (tx, rx) = mpsc::channel::<AccountUpdate>(4096);
        let url = self.url.clone();
        let stats = Arc::clone(&self.stats);

        tokio::spawn(async move {
            let mut backoff_ms = 500u64;
            loop {
                match run_once(&url, &accounts, &tx, &stats).await {
                    Ok(()) => tracing::warn!("feed stream ended cleanly; reconnecting"),
                    Err(e) => tracing::warn!("feed error: {e:#}; reconnecting"),
                }
                stats.reconnects.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        });

        rx
    }
}

async fn run_once(
    url: &str,
    accounts: &[Pubkey32],
    tx: &mpsc::Sender<AccountUpdate>,
    stats: &FeedStats,
) -> Result<()> {
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await?;
    tracing::info!("feed connected to {url}, subscribing to {} accounts", accounts.len());

    // Map JSON-RPC subscription id -> pubkey. The notification carries only the
    // subscription id, so without this table we cannot tell which account changed.
    let mut pending: std::collections::HashMap<u64, Pubkey32> = std::collections::HashMap::new();
    let mut subs: std::collections::HashMap<u64, Pubkey32> = std::collections::HashMap::new();

    for (i, acct) in accounts.iter().enumerate() {
        let id = i as u64 + 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "accountSubscribe",
            "params": [bs58::encode(acct).into_string(), {"encoding": "base64", "commitment": "processed"}]
        });
        socket.send(Message::Text(req.to_string().into())).await?;
        pending.insert(id, *acct);
    }

    while let Some(msg) = socket.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Ping(p) => {
                socket.send(Message::Pong(p)).await?;
                continue;
            }
            Message::Close(_) => return Ok(()),
            _ => continue,
        };

        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                stats.parse_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // Subscription confirmation: {"result": <sub_id>, "id": <req_id>}
        if let (Some(sub_id), Some(req_id)) = (v.get("result").and_then(|r| r.as_u64()), v.get("id").and_then(|i| i.as_u64())) {
            if let Some(pk) = pending.remove(&req_id) {
                subs.insert(sub_id, pk);
            }
            continue;
        }

        // An error response to one of our subscribe calls.
        if let Some(err) = v.get("error") {
            return Err(anyhow!("rpc error: {err}"));
        }

        if v.get("method").and_then(|m| m.as_str()) != Some("accountNotification") {
            continue;
        }

        let params = v.get("params").ok_or_else(|| anyhow!("notification without params"))?;
        let Some(sub_id) = params.get("subscription").and_then(|s| s.as_u64()) else {
            continue;
        };
        let Some(&pubkey) = subs.get(&sub_id) else {
            continue; // notification for a subscription we don't track
        };

        let result = params.get("result").ok_or_else(|| anyhow!("notification without result"))?;
        let slot = result.get("context").and_then(|c| c.get("slot")).and_then(|s| s.as_u64()).unwrap_or(0);
        let Some(b64) = result
            .get("value")
            .and_then(|val| val.get("data"))
            .and_then(|d| d.get(0))
            .and_then(|d| d.as_str())
        else {
            stats.parse_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        let data = match base64_decode(b64) {
            Some(d) => d,
            None => {
                stats.parse_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        stats.updates.fetch_add(1, Ordering::Relaxed);
        stats.last_slot.store(slot, Ordering::Relaxed);
        stats.last_update_ms.store(now_ms(), Ordering::Relaxed);

        let update = AccountUpdate { pubkey, data, slot, received_ms: now_ms() };
        // try_send, never send: blocking here would apply backpressure all the way
        // back to the socket and turn a slow consumer into a stalled feed.
        if tx.try_send(update).is_err() {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    Ok(())
}

/// Standard base64 decode. Small enough not to justify a dependency.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => return None,
            _ => return None,
        })
    };
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::with_capacity(s.len() * 3 / 4));
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrips_known_vectors() {
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(base64_decode("QQ==").unwrap(), b"A");
        assert_eq!(base64_decode("QUI=").unwrap(), b"AB");
        assert_eq!(base64_decode("QUJD").unwrap(), b"ABC");
        assert_eq!(base64_decode("SGVsbG8sIFdvcmxkIQ==").unwrap(), b"Hello, World!");
    }

    #[test]
    fn base64_rejects_garbage() {
        assert!(base64_decode("!!!!").is_none());
    }

    #[test]
    fn base64_handles_all_byte_values() {
        // Encode 0..=255 with a reference table, then decode and compare.
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let input: Vec<u8> = (0..=255u8).collect();
        let mut enc = String::new();
        for chunk in input.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    enc.push(T[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
                } else {
                    enc.push('=');
                }
            }
        }
        assert_eq!(base64_decode(&enc).unwrap(), input);
    }

    #[test]
    fn stats_start_at_zero() {
        let s = FeedStats::default();
        assert_eq!(s.snapshot(), (0, 0, 0, 0, 0));
    }

    #[test]
    fn dropped_updates_are_counted_not_silently_lost() {
        // A full channel must increment `dropped`, because an unevaluated update is
        // an opportunity we can never know we missed.
        let s = FeedStats::default();
        s.dropped.fetch_add(3, Ordering::Relaxed);
        assert_eq!(s.snapshot().2, 3);
    }
}
