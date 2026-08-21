//! Live mainnet market: bootstrap pool state over HTTP RPC, then keep it current
//! from the WebSocket feed.
//!
//! A Raydium v4 pool needs **three** accounts to price: the `AmmInfo` (for mints,
//! fees and uncollected-fee amounts) and its two SPL vaults (for reserves). We
//! subscribe to all three and rebuild the pool whenever any of them changes.

use anyhow::{Context, Result};
use cb_core::types::{PoolId, PoolState, Pubkey32};
use cb_dex::raydium_v4;
use cb_feed::AccountUpdate;
use cb_scanner::multi::{find_cycles, survey_cycles};
use cb_scanner::store::PoolStore;
use cb_server::{Event, EventBus};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Raydium v4 pools to watch. Verified against mainnet on 2026-08-20.
///
/// Chosen so they overlap: SOL/USDC + RAY/USDC + RAY/SOL forms a closed triangle,
/// which is the shape a real arbitrage cycle takes on major pairs.
pub const WATCHED_POOLS: &[(&str, &str)] = &[
    ("58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2", "SOL/USDC"),
    ("7XawhbbxtsRcQA8KTkHT9f9nc6d69UwqCDh6U5EEbEmX", "SOL/USDT"),
    ("AVs9TA4nWDzfPJE9gGVNJMVhcQy3V9PGazuz33BfG2RA", "RAY/SOL"),
    ("6UmmUiYoBjSrhakAobJw8BvkmJtDVxaeBtbt7rxWo1mg", "RAY/USDC"),
];

/// Known mints, for display only. Never used for routing — routing matches on the
/// mint address, because symbols are trivially spoofable.
pub fn symbol_for(mint: &Pubkey32) -> String {
    let s = bs58::encode(mint).into_string();
    match s.as_str() {
        "So11111111111111111111111111111111111111112" => "SOL".into(),
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => "USDC".into(),
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => "USDT".into(),
        "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R" => "RAY".into(),
        _ => s.chars().take(4).collect(),
    }
}

/// Decimals for the known mints, needed to turn base units into a human price.
fn decimals_for(mint: &Pubkey32) -> u32 {
    match symbol_for(mint).as_str() {
        "SOL" => 9,
        "USDC" | "USDT" => 6,
        "RAY" => 6,
        _ => 9,
    }
}

/// The closest any visible cycle came to clearing.
#[derive(Debug, Clone)]
pub struct BestEdge {
    pub route: String,
    /// Positive is profitable; negative is how far short the cycle falls.
    pub edge_bps: f64,
    pub hops: usize,
    /// Total swap fees along the route, in bps.
    pub fee_bps: f64,
    /// Cycles considered in this pass.
    pub evaluated: u64,
}

/// A cycle found on live data, priced in USD.
#[derive(Debug, Clone)]
pub struct LiveOpportunity {
    pub route: String,
    pub venues: String,
    pub hops: usize,
    pub size_usd: f64,
    pub optimal_size_usd: f64,
    pub gross_profit_usd: f64,
    pub profit_at_optimal_usd: f64,
    pub slot: u64,
}

struct Watch {
    label: String,
    info: raydium_v4::AmmInfo,
    base_amount: Option<u64>,
    quote_amount: Option<u64>,
    slot: u64,
}

impl Watch {
    /// Build a `PoolState` once both vault balances are known.
    fn pool_state(&self, addr: Pubkey32) -> Option<PoolState> {
        let (b, q) = (self.base_amount?, self.quote_amount?);
        raydium_v4::to_pool_state(addr, &self.info, b, q, self.slot).ok()
    }

    fn price(&self, addr: Pubkey32) -> Option<f64> {
        let p = self.pool_state(addr)?;
        let bd = decimals_for(&p.mint_a);
        let qd = decimals_for(&p.mint_b);
        let base = p.reserve_a as f64 / 10f64.powi(bd as i32);
        let quote = p.reserve_b as f64 / 10f64.powi(qd as i32);
        if base > 0.0 {
            Some(quote / base)
        } else {
            None
        }
    }
}

pub struct LiveMarket {
    watches: HashMap<Pubkey32, Watch>,
    /// vault address -> (pool address, is_base_vault)
    vault_index: HashMap<Pubkey32, (Pubkey32, bool)>,
    pub store: PoolStore,
    pub subscriptions: Vec<Pubkey32>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn pk(s: &str) -> Result<Pubkey32> {
    let v = bs58::decode(s).into_vec().with_context(|| format!("bad base58: {s}"))?;
    let arr: [u8; 32] = v.as_slice().try_into().with_context(|| format!("not 32 bytes: {s}"))?;
    Ok(arr)
}

impl LiveMarket {
    /// Fetch pool accounts and their vaults, and build the initial state.
    pub async fn bootstrap(rpc_http: &str) -> Result<Self> {
        let client = reqwest::Client::new();

        let pool_keys: Vec<Pubkey32> =
            WATCHED_POOLS.iter().map(|(a, _)| pk(a)).collect::<Result<_>>()?;
        let pool_b58: Vec<String> = WATCHED_POOLS.iter().map(|(a, _)| (*a).to_string()).collect();

        let pool_accounts = get_multiple_accounts(&client, rpc_http, &pool_b58).await?;

        let mut watches = HashMap::new();
        let mut vault_index = HashMap::new();
        let mut vault_b58 = Vec::new();

        for (i, data) in pool_accounts.iter().enumerate() {
            let Some(data) = data else {
                tracing::warn!("pool {} not found on chain, skipping", pool_b58[i]);
                continue;
            };
            let info = match raydium_v4::decode_amm_info(data) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("pool {} did not decode: {e:#}", pool_b58[i]);
                    continue;
                }
            };
            let addr = pool_keys[i];
            vault_index.insert(info.base_vault, (addr, true));
            vault_index.insert(info.quote_vault, (addr, false));
            vault_b58.push(bs58::encode(info.base_vault).into_string());
            vault_b58.push(bs58::encode(info.quote_vault).into_string());

            watches.insert(
                addr,
                Watch {
                    label: WATCHED_POOLS[i].1.to_string(),
                    info,
                    base_amount: None,
                    quote_amount: None,
                    slot: 0,
                },
            );
        }

        anyhow::ensure!(!watches.is_empty(), "no watched pools decoded — cannot start");

        // Seed the vault balances so we have a complete picture before any update.
        let vault_accounts = get_multiple_accounts(&client, rpc_http, &vault_b58).await?;
        for (i, data) in vault_accounts.iter().enumerate() {
            let Some(data) = data else { continue };
            let Ok(amount) = raydium_v4::decode_token_amount(data) else { continue };
            let vault = pk(&vault_b58[i])?;
            if let Some(&(pool, is_base)) = vault_index.get(&vault) {
                if let Some(w) = watches.get_mut(&pool) {
                    if is_base {
                        w.base_amount = Some(amount);
                    } else {
                        w.quote_amount = Some(amount);
                    }
                }
            }
        }

        let store = PoolStore::new();
        for (addr, w) in &watches {
            if let Some(ps) = w.pool_state(*addr) {
                store.upsert(ps);
            }
        }

        // Subscribe to every pool account and every vault: any of the three changing
        // changes the price.
        let mut subscriptions: Vec<Pubkey32> = watches.keys().copied().collect();
        subscriptions.extend(vault_index.keys().copied());

        tracing::info!(
            "bootstrapped {} pools, {} accounts to watch",
            watches.len(),
            subscriptions.len()
        );

        Ok(Self { watches, vault_index, store, subscriptions })
    }

    /// Human labels for the dashboard, in a stable order.
    pub fn labels(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .watches
            .iter()
            .map(|(addr, w)| (bs58::encode(addr).into_string(), w.label.clone()))
            .collect();
        v.sort();
        v
    }

    /// Apply one account update. Returns the pool whose state changed, if any.
    pub fn apply(&mut self, u: &AccountUpdate, bus: &EventBus) -> Option<PoolState> {
        let pool_addr = if self.watches.contains_key(&u.pubkey) {
            // The AmmInfo itself changed — re-decode, since fees and uncollected
            // amounts move and stale ones corrupt the reserve calculation.
            match raydium_v4::decode_amm_info(&u.data) {
                Ok(info) => {
                    if let Some(w) = self.watches.get_mut(&u.pubkey) {
                        w.info = info;
                        w.slot = w.slot.max(u.slot);
                    }
                    u.pubkey
                }
                Err(_) => return None,
            }
        } else if let Some(&(pool, is_base)) = self.vault_index.get(&u.pubkey) {
            let amount = raydium_v4::decode_token_amount(&u.data).ok()?;
            let w = self.watches.get_mut(&pool)?;
            if is_base {
                w.base_amount = Some(amount);
            } else {
                w.quote_amount = Some(amount);
            }
            w.slot = w.slot.max(u.slot);
            pool
        } else {
            return None;
        };

        let w = self.watches.get(&pool_addr)?;
        let state = w.pool_state(pool_addr)?;
        self.store.upsert(state);

        if let Some(price) = w.price(pool_addr) {
            let bd = decimals_for(&state.mint_a);
            let qd = decimals_for(&state.mint_b);
            bus.publish(Event::PoolUpdate {
                pool: bs58::encode(pool_addr).into_string(),
                dex: "Raydium v4".into(),
                pair: w.label.clone(),
                price,
                reserve_a: state.reserve_a as f64 / 10f64.powi(bd as i32),
                reserve_b: state.reserve_b as f64 / 10f64.powi(qd as i32),
                slot: state.slot,
                ts_ms: now_ms(),
            });
        }

        Some(state)
    }

    /// Best cycle edge currently visible through `changed`, in bps — profitable or not.
    ///
    /// Returns the route and its edge. A persistently negative number is the real
    /// research finding: it says how tightly these venues are arbitraged and therefore
    /// whether faster infrastructure could ever pay for itself.
    pub fn best_edge(&self, changed: &PoolState, max_hops: usize) -> Option<BestEdge> {
        let mut best: Option<BestEdge> = None;
        let mut count = 0u64;
        for base in [changed.mint_a, changed.mint_b] {
            for sc in survey_cycles(&self.store, &base, changed, max_hops) {
                count += 1;
                let route = sc.cycle.mints.iter().map(symbol_for).collect::<Vec<_>>().join("→");
                // Total fee drag of the route. Comparing this against the edge shows
                // whether fees or price efficiency is what stops the cycle clearing.
                let fee_bps: f64 = sc.cycle.legs.iter().map(|l| f64::from(l.fee_bps)).sum();
                if best.as_ref().is_none_or(|b| sc.edge_bps > b.edge_bps) {
                    best = Some(BestEdge {
                        route,
                        edge_bps: sc.edge_bps,
                        hops: sc.cycle.hops(),
                        fee_bps,
                        evaluated: 0,
                    });
                }
            }
        }
        if let Some(b) = best.as_mut() {
            b.evaluated = count;
        }
        best
    }

    #[must_use]
    pub fn pool_count(&self) -> usize {
        self.watches.len()
    }

    /// Pools that have both vault balances and are therefore priceable.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.watches
            .iter()
            .filter(|(addr, w)| w.pool_state(**addr).is_some())
            .count()
    }

    /// USD value of one base unit of `mint`, derived from live pool state.
    ///
    /// Stablecoins are pinned at $1 — close enough for sizing, and it avoids a
    /// circular dependency on a pool that quotes them. SOL is read from the live
    /// SOL/USDC pool rather than a hardcoded price, so it tracks the market.
    pub fn usd_per_base_unit(&self, mint: &Pubkey32) -> Option<f64> {
        let sym = symbol_for(mint);
        let dec = decimals_for(mint) as i32;
        let unit = 10f64.powi(-dec);
        match sym.as_str() {
            "USDC" | "USDT" => Some(unit),
            "SOL" => self.sol_price_usd().map(|p| unit * p),
            _ => {
                // Price anything else through a pool that quotes it against a token
                // we can already value.
                for (addr, w) in &self.watches {
                    let Some(ps) = w.pool_state(*addr) else { continue };
                    let (other, price) = if ps.mint_a == *mint {
                        (ps.mint_b, w.price(*addr)?)
                    } else if ps.mint_b == *mint {
                        (ps.mint_a, 1.0 / w.price(*addr)?)
                    } else {
                        continue;
                    };
                    // `price` is other-per-mint in whole-token terms.
                    let other_dec = decimals_for(&other) as i32;
                    let other_usd = match symbol_for(&other).as_str() {
                        "USDC" | "USDT" => Some(10f64.powi(-other_dec)),
                        "SOL" => self.sol_price_usd().map(|p| 10f64.powi(-other_dec) * p),
                        _ => None,
                    }?;
                    let other_whole_usd = other_usd * 10f64.powi(other_dec);
                    return Some(unit * price * other_whole_usd);
                }
                None
            }
        }
    }

    /// SOL price in USD, from the live SOL/USDC pool.
    pub fn sol_price_usd(&self) -> Option<f64> {
        for (addr, w) in &self.watches {
            if w.label == "SOL/USDC" {
                return w.price(*addr);
            }
        }
        None
    }

    /// Search for profitable cycles through `changed`, priced in USD and capped to
    /// available capital.
    pub fn evaluate(&self, changed: &PoolState, tradable_usd: f64, max_hops: usize) -> Vec<LiveOpportunity> {
        let mut out = Vec::new();
        for base in [changed.mint_a, changed.mint_b] {
            let Some(usd_per_unit) = self.usd_per_base_unit(&base) else { continue };
            if usd_per_unit <= 0.0 {
                continue;
            }
            // Convert the USD cap into base units of whatever token starts the cycle.
            let max_in = (tradable_usd / usd_per_unit) as u128;
            if max_in == 0 {
                continue;
            }

            for p in find_cycles(&self.store, &base, changed, max_hops, max_in) {
                let route: Vec<String> = p
                    .cycle
                    .mints
                    .iter()
                    .map(symbol_for)
                    .collect();
                let venues: Vec<String> =
                    p.cycle.pools.iter().map(|id| self.pool_id_label(id)).collect();

                out.push(LiveOpportunity {
                    route: route.join(" → "),
                    venues: venues.join(" · "),
                    hops: p.cycle.hops(),
                    size_usd: p.capped_in as f64 * usd_per_unit,
                    optimal_size_usd: p.optimal_in as f64 * usd_per_unit,
                    gross_profit_usd: p.profit as f64 * usd_per_unit,
                    profit_at_optimal_usd: p.profit_at_optimal as f64 * usd_per_unit,
                    slot: p.cycle.slot(&self.store),
                });
            }
        }
        out.sort_by(|a, b| b.gross_profit_usd.total_cmp(&a.gross_profit_usd));
        out
    }

    #[must_use]
    pub fn pool_id_label(&self, id: &PoolId) -> String {
        self.watches.get(&id.0).map_or_else(
            || bs58::encode(id.0).into_string()[..6].to_string(),
            |w| w.label.clone(),
        )
    }
}

/// `getMultipleAccounts`, returning raw account data in request order.
async fn get_multiple_accounts(
    client: &reqwest::Client,
    url: &str,
    keys: &[String],
) -> Result<Vec<Option<Vec<u8>>>> {
    // The RPC caps this at 100 keys per call.
    let mut out = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(100) {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getMultipleAccounts",
            "params": [chunk, {"encoding": "base64"}]
        });
        let resp: serde_json::Value = client
            .post(url)
            .json(&body)
            .send()
            .await
            .context("rpc request failed")?
            .json()
            .await
            .context("rpc response was not json")?;

        if let Some(err) = resp.get("error") {
            anyhow::bail!("rpc error: {err}");
        }
        let values = resp
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_array())
            .context("rpc result missing value array")?;

        for v in values {
            let data = v
                .get("data")
                .and_then(|d| d.get(0))
                .and_then(|d| d.as_str())
                .and_then(decode_b64);
            out.push(data);
        }
    }
    Ok(out)
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
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
    fn watched_pool_addresses_are_valid_base58() {
        for (addr, label) in WATCHED_POOLS {
            assert!(pk(addr).is_ok(), "{label} has an invalid address");
        }
    }

    #[test]
    fn symbols_resolve_for_the_majors() {
        let sol = pk("So11111111111111111111111111111111111111112").unwrap();
        let usdc = pk("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();
        assert_eq!(symbol_for(&sol), "SOL");
        assert_eq!(symbol_for(&usdc), "USDC");
        assert_eq!(decimals_for(&sol), 9);
        assert_eq!(decimals_for(&usdc), 6);
    }

    #[test]
    fn unknown_mints_degrade_to_a_short_prefix_not_a_panic() {
        let s = symbol_for(&[9u8; 32]);
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn base64_decoder_matches_known_vectors() {
        assert_eq!(decode_b64("SGVsbG8=").unwrap(), b"Hello");
        assert_eq!(decode_b64("").unwrap(), Vec::<u8>::new());
    }
}
