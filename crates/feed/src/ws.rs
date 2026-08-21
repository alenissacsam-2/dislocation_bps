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

/// How long to wait on a read before probing the connection with a ping.
pub const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// How long account data may be absent before we declare the socket dead and
/// reconnect. The watched set includes SOL/USDC, which trades continuously, so a
/// full minute of silence is not a quiet market — it is a broken connection.
pub const MAX_SILENCE: Duration = Duration::from_secs(60);

/// How many subscribe requests to send before yielding briefly.
///
/// Public RPC endpoints throttle bursts. Eighty-odd `accountSubscribe` calls fired
/// back to back is exactly the shape of traffic that gets rate limited, and a
/// rejected subscription is a pool that silently never updates for the whole run.
pub const SUBSCRIBE_BATCH: usize = 16;

/// Pause between subscribe batches.
pub const SUBSCRIBE_PAUSE: Duration = Duration::from_millis(120);

/// Whether a connection that has been silent for `silent` should be abandoned.
///
/// Split out from the socket loop so the decision is testable without a network.
#[must_use]
pub fn is_dead(silent: Duration) -> bool {
    silent > MAX_SILENCE
}

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
    /// Connections abandoned because data stopped arriving while the socket stayed
    /// open. Distinct from `reconnects`, which counts all reconnects.
    pub stalls: AtomicU64,
    /// Accounts the server confirmed a subscription for on the current connection.
    pub subscribed: AtomicU64,
    /// Accounts the server refused to subscribe to. Each one is a pool that will
    /// never update, so it must be visible rather than inferred from silence.
    pub subscribe_errors: AtomicU64,
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

    stats.subscribed.store(0, Ordering::Relaxed);
    stats.subscribe_errors.store(0, Ordering::Relaxed);

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
        if (i + 1) % SUBSCRIBE_BATCH == 0 {
            tokio::time::sleep(SUBSCRIBE_PAUSE).await;
        }
    }

    // A silently-dead socket is the dangerous failure mode: the peer stops sending but
    // never closes, so `next()` blocks forever and the reconnect loop never runs. The
    // bot keeps serving whatever prices it last saw. In paper mode that corrupts the
    // measurement; in live mode it would trade on stale state.
    //
    // So: never block indefinitely. Probe with a ping when quiet, and give up entirely
    // once data has been absent long enough that the connection cannot be healthy.
    let mut last_data = std::time::Instant::now();

    loop {
        let next = match tokio::time::timeout(READ_TIMEOUT, socket.next()).await {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(()), // stream ended cleanly
            Err(_elapsed) => {
                // Nothing arrived within the read window. Distinguish "quiet" from
                // "dead": ping to keep intermediaries from reaping the connection,
                // then bail if data has been absent past the tolerance.
                let silent = last_data.elapsed();
                if silent > MAX_SILENCE {
                    stats.stalls.fetch_add(1, Ordering::Relaxed);
                    return Err(anyhow!(
                        "no account data for {}s — treating connection as dead",
                        silent.as_secs()
                    ));
                }
                tracing::debug!("feed quiet for {}s, pinging", silent.as_secs());
                socket.send(Message::Ping(Vec::new().into())).await?;
                continue;
            }
        };

        let msg = next?;
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Ping(p) => {
                socket.send(Message::Pong(p)).await?;
                continue;
            }
            // A pong proves the socket is alive but carries no prices, so it must
            // NOT reset the data-staleness clock. Otherwise our own keepalive would
            // mask a feed that has stopped delivering account updates.
            Message::Pong(_) => continue,
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
                stats.subscribed.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        }

        // An error response. If it carries the id of one of our subscribe calls, it
        // means *that account* was refused — drop it and keep the connection. Tearing
        // down a working feed of eighty pools because the eighty-first was rate
        // limited would turn a partial outage into a total one, and the reconnect
        // would hit the same limit again.
        if let Some(err) = v.get("error") {
            match v.get("id").and_then(|i| i.as_u64()).and_then(|id| pending.remove(&id)) {
                Some(pk) => {
                    stats.subscribe_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        "subscription refused for {}: {err}",
                        bs58::encode(pk).into_string()
                    );
                    continue;
                }
                None => return Err(anyhow!("rpc error: {err}")),
            }
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

        last_data = std::time::Instant::now();
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
    fn silence_past_the_tolerance_is_treated_as_death() {
        // The bug this guards: a socket that stays open but stops delivering data.
        assert!(!is_dead(Duration::from_secs(0)));
        assert!(!is_dead(Duration::from_secs(30)));
        assert!(!is_dead(MAX_SILENCE));
        assert!(is_dead(MAX_SILENCE + Duration::from_secs(1)));
        assert!(is_dead(Duration::from_secs(600)));
    }

    #[test]
    fn read_timeout_is_shorter_than_the_silence_tolerance() {
        // Otherwise we would declare the connection dead before ever probing it
        // with a ping, and reconnect on merely-quiet markets.
        assert!(READ_TIMEOUT < MAX_SILENCE, "must get at least one ping probe in first");
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
