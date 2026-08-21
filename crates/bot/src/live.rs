//! Live mainnet market across four venues: bootstrap over HTTP RPC, then keep
//! current from the WebSocket feed.
//!
//! # Two shapes of pool, two costs to watch them
//!
//! Orca Whirlpool and Raydium CLMM keep `liquidity` and `sqrt_price` in the pool
//! account, so **one** subscription tracks a pool completely. Raydium AMM v4 keeps its
//! reserves in two separate SPL vaults, so it needs **three** — and the three can be
//! observed at different slots, which is why the decoder rejects a vault balance below
//! its own recorded uncollected fees rather than quoting a torn read.
//!
//! Raydium CLMM adds a wrinkle: its fee lives in a shared `AmmConfig` account, not the
//! pool. Configs are resolved once at bootstrap and cached, because they only change by
//! governance — but they are read from chain, never assumed.
//!
//! # Why the search sweeps rather than reacts
//!
//! An earlier version searched only cycles containing whichever pool had just changed.
//! That is cheaper, and wrong in a specific way: a cycle SOL → A → B → SOL contains a
//! pool (A/B) that does not trade SOL, so a change to *that* pool made the cycle
//! profitable invisibly. With eighty pools a full sweep from every base mint costs
//! well under a millisecond, so the loop sweeps on a timer and the blind spot is gone.

use anyhow::{Context, Result};
use cb_core::types::{Dex, PoolId, PoolState, Pubkey32};
use cb_dex::{orca_whirlpool, raydium_clmm, raydium_cpmm, raydium_v4};
use cb_feed::AccountUpdate;
use cb_scanner::multi::{find_from_base, survey_from_base};
use cb_scanner::store::PoolStore;
use cb_server::{Event, EventBus, RouteRow};
use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::registry::{pk, Registry};

/// USD anchors. Everything else is priced by walking the pool graph out from these.
///
/// Only these two. Pinning any token whose ticker merely *looks* like a dollar — USX,
/// USD1, JupUSD — at $1 would be assuming the thing we would be trying to measure if
/// we ever routed through it. Those get priced from pools like any other token.
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
const WSOL: &str = "So11111111111111111111111111111111111111112";

/// How many hops out from the anchors the USD index will walk.
const USD_INDEX_ROUNDS: usize = 4;

/// Rows kept on the dashboard leaderboard.
const LEADERBOARD_ROWS: usize = 12;

/// What the bot knows about one watched pool beyond its current state.
enum Venue {
    /// Everything needed is in the pool account, including the fee.
    Whirlpool,
    /// Everything except the fee, which comes from a shared config account.
    RaydiumClmm { trade_fee_ppm: u32 },
    /// Reserves live in two vaults, tracked separately and combined on read.
    RaydiumV4 {
        info: Box<raydium_v4::AmmInfo>,
        base_amount: Option<u64>,
        quote_amount: Option<u64>,
    },
    /// Raydium's newer constant-product program. Vaults like v4, fee like CLMM.
    RaydiumCpmm {
        pool: Box<raydium_cpmm::CpmmPool>,
        trade_fee_ppm: u32,
        vault_0_amount: Option<u64>,
        vault_1_amount: Option<u64>,
    },
}

struct Watch {
    label: String,
    dex: Dex,
    venue: Venue,
    slot: u64,
}

impl Watch {
    /// Rebuild a vault-backed pool's state from whatever balances we currently hold.
    ///
    /// Returns `None` for a concentrated pool — its state goes straight into the store
    /// the moment its account decodes — and for a vault-backed pool whose vaults we
    /// have not seen yet. A pool priced from one vault and a guess is worse than no
    /// pool at all.
    fn vault_state(&self, addr: Pubkey32) -> Option<PoolState> {
        match &self.venue {
            Venue::RaydiumV4 { info, base_amount, quote_amount } => {
                raydium_v4::to_pool_state(addr, info, (*base_amount)?, (*quote_amount)?, self.slot)
                    .ok()
            }
            Venue::RaydiumCpmm { pool, trade_fee_ppm, vault_0_amount, vault_1_amount } => {
                raydium_cpmm::to_pool_state(
                    addr,
                    pool,
                    (*vault_0_amount)?,
                    (*vault_1_amount)?,
                    *trade_fee_ppm,
                    self.slot,
                )
                .ok()
            }
            _ => None,
        }
    }

    /// Record a vault balance. `first` selects vault 0 / the base vault.
    fn set_vault(&mut self, first: bool, amount: u64) {
        match &mut self.venue {
            Venue::RaydiumV4 { base_amount, quote_amount, .. } => {
                *(if first { base_amount } else { quote_amount }) = Some(amount);
            }
            Venue::RaydiumCpmm { vault_0_amount, vault_1_amount, .. } => {
                *(if first { vault_0_amount } else { vault_1_amount }) = Some(amount);
            }
            _ => {}
        }
    }
}

/// One route, ready for the dashboard.
#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub route: String,
    pub venues: String,
    pub hops: usize,
    pub edge_bps: f64,
    pub dislocation_bps: f64,
    pub fee_bps: f64,
    pub depth_usd: f64,
    pub slot: u64,
}

impl From<&EdgeRow> for RouteRow {
    fn from(r: &EdgeRow) -> Self {
        RouteRow {
            route: r.route.clone(),
            venues: r.venues.clone(),
            hops: r.hops,
            edge_bps: r.edge_bps,
            dislocation_bps: r.dislocation_bps,
            fee_bps: r.fee_bps,
            depth_usd: r.depth_usd,
            slot: r.slot,
        }
    }
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
    pub edge_bps: f64,
    pub fee_bps: f64,
    pub slot: u64,
}

/// The result of one full pass over the cycle graph.
#[derive(Debug, Clone, Default)]
pub struct Sweep {
    pub evaluated: u64,
    pub rows: Vec<EdgeRow>,
    pub opportunities: Vec<LiveOpportunity>,
    pub duration_us: u64,
    pub slot: u64,
}

impl Sweep {
    #[must_use]
    pub fn best(&self) -> Option<&EdgeRow> {
        self.rows.first()
    }
}

pub struct LiveMarket {
    pub registry: Registry,
    watches: HashMap<Pubkey32, Watch>,
    /// vault address -> (pool address, is_base_vault)
    vault_index: HashMap<Pubkey32, (Pubkey32, bool)>,
    pub store: PoolStore,
    pub subscriptions: Vec<Pubkey32>,
    /// USD per *whole* token, rebuilt from live pools rather than assumed.
    usd: HashMap<Pubkey32, f64>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl LiveMarket {
    /// Fetch every pool in the registry, resolve fees and vaults, and build the
    /// initial state.
    pub async fn bootstrap(rpc_http: &str, registry: Registry) -> Result<Self> {
        let client = reqwest::Client::new();

        let addresses: Vec<String> =
            registry.pools.iter().map(|p| bs58::encode(p.address).into_string()).collect();
        let accounts = get_multiple_accounts(&client, rpc_http, &addresses).await?;

        let mut watches: HashMap<Pubkey32, Watch> = HashMap::new();
        let mut vault_index = HashMap::new();
        let mut vault_b58: Vec<String> = Vec::new();
        let mut config_b58: Vec<String> = Vec::new();
        let mut clmm_pending: Vec<(Pubkey32, Pubkey32, Vec<u8>)> = Vec::new(); // pool, config, data
        let mut cpmm_config_b58: Vec<String> = Vec::new();
        let mut cpmm_pending: Vec<(Pubkey32, Pubkey32)> = Vec::new(); // pool, config
        let store = PoolStore::new();
        let mut failures: Vec<String> = Vec::new();

        for (entry, account) in registry.pools.iter().zip(accounts.iter()) {
            let Some(data) = account else {
                failures.push(format!("{}: account not found", entry.label));
                continue;
            };
            let addr = entry.address;

            match entry.dex {
                Dex::OrcaWhirlpool => match orca_whirlpool::to_pool_state(addr, data, 0) {
                    Ok(ps) => {
                        store.upsert(ps);
                        watches.insert(
                            addr,
                            Watch {
                                label: entry.label.clone(),
                                dex: entry.dex,
                                venue: Venue::Whirlpool,
                                slot: 0,
                            },
                        );
                    }
                    Err(e) => failures.push(format!("{}: {e:#}", entry.label)),
                },
                Dex::RaydiumClmm => match raydium_clmm::decode(data) {
                    Ok(p) => {
                        config_b58.push(bs58::encode(p.amm_config).into_string());
                        clmm_pending.push((addr, p.amm_config, data.clone()));
                        watches.insert(
                            addr,
                            Watch {
                                label: entry.label.clone(),
                                dex: entry.dex,
                                // Filled in once the config resolves.
                                venue: Venue::RaydiumClmm { trade_fee_ppm: u32::MAX },
                                slot: 0,
                            },
                        );
                    }
                    Err(e) => failures.push(format!("{}: {e:#}", entry.label)),
                },
                Dex::RaydiumAmmV4 => match raydium_v4::decode_amm_info(data) {
                    Ok(info) => {
                        vault_index.insert(info.base_vault, (addr, true));
                        vault_index.insert(info.quote_vault, (addr, false));
                        vault_b58.push(bs58::encode(info.base_vault).into_string());
                        vault_b58.push(bs58::encode(info.quote_vault).into_string());
                        watches.insert(
                            addr,
                            Watch {
                                label: entry.label.clone(),
                                dex: entry.dex,
                                venue: Venue::RaydiumV4 {
                                    info: Box::new(info),
                                    base_amount: None,
                                    quote_amount: None,
                                },
                                slot: 0,
                            },
                        );
                    }
                    Err(e) => failures.push(format!("{}: {e:#}", entry.label)),
                },
                Dex::RaydiumCpmm => match raydium_cpmm::decode(data) {
                    Ok(p) => {
                        vault_index.insert(p.vault_0, (addr, true));
                        vault_index.insert(p.vault_1, (addr, false));
                        vault_b58.push(bs58::encode(p.vault_0).into_string());
                        vault_b58.push(bs58::encode(p.vault_1).into_string());
                        cpmm_config_b58.push(bs58::encode(p.amm_config).into_string());
                        cpmm_pending.push((addr, p.amm_config));
                        watches.insert(
                            addr,
                            Watch {
                                label: entry.label.clone(),
                                dex: entry.dex,
                                venue: Venue::RaydiumCpmm {
                                    pool: Box::new(p),
                                    // Filled in once the config resolves.
                                    trade_fee_ppm: u32::MAX,
                                    vault_0_amount: None,
                                    vault_1_amount: None,
                                },
                                slot: 0,
                            },
                        );
                    }
                    Err(e) => failures.push(format!("{}: {e:#}", entry.label)),
                },
                Dex::PumpSwap => failures.push(format!("{}: pumpswap not wired live", entry.label)),
            }
        }

        // Resolve Raydium CLMM fees from their shared config accounts.
        config_b58.sort();
        config_b58.dedup();
        let mut fee_by_config: HashMap<Pubkey32, u32> = HashMap::new();
        if !config_b58.is_empty() {
            let configs = get_multiple_accounts(&client, rpc_http, &config_b58).await?;
            for (b58, data) in config_b58.iter().zip(configs.iter()) {
                let Some(data) = data else { continue };
                match raydium_clmm::decode_trade_fee_ppm(data) {
                    Ok(fee) => {
                        fee_by_config.insert(pk(b58)?, fee);
                    }
                    Err(e) => failures.push(format!("config {b58}: {e:#}")),
                }
            }
        }
        for (addr, config, data) in clmm_pending {
            let Some(&fee) = fee_by_config.get(&config) else {
                watches.remove(&addr);
                failures.push(format!("{}: fee config unresolved", bs58::encode(addr).into_string()));
                continue;
            };
            match raydium_clmm::to_pool_state(addr, &data, fee, 0) {
                Ok(ps) => {
                    store.upsert(ps);
                    if let Some(w) = watches.get_mut(&addr) {
                        w.venue = Venue::RaydiumClmm { trade_fee_ppm: fee };
                    }
                }
                Err(e) => {
                    let label =
                        watches.get(&addr).map_or_else(String::new, |w| w.label.clone());
                    watches.remove(&addr);
                    failures.push(format!("{label}: {e:#}"));
                }
            }
        }

        // Resolve Raydium CP-Swap fees. A different program from CLMM with a different
        // config layout, so it needs its own round trip rather than sharing the above.
        cpmm_config_b58.sort();
        cpmm_config_b58.dedup();
        if !cpmm_config_b58.is_empty() {
            let configs = get_multiple_accounts(&client, rpc_http, &cpmm_config_b58).await?;
            let mut fee_by_config: HashMap<Pubkey32, u32> = HashMap::new();
            for (b58, data) in cpmm_config_b58.iter().zip(configs.iter()) {
                let Some(data) = data else { continue };
                match raydium_cpmm::decode_trade_fee_ppm(data) {
                    Ok(fee) => {
                        fee_by_config.insert(pk(b58)?, fee);
                    }
                    Err(e) => failures.push(format!("cpmm config {b58}: {e:#}")),
                }
            }
            for (addr, config) in cpmm_pending {
                match fee_by_config.get(&config) {
                    Some(&fee) => {
                        if let Some(Venue::RaydiumCpmm { trade_fee_ppm, .. }) =
                            watches.get_mut(&addr).map(|w| &mut w.venue)
                        {
                            *trade_fee_ppm = fee;
                        }
                    }
                    None => {
                        let label = watches.get(&addr).map_or_else(String::new, |w| w.label.clone());
                        watches.remove(&addr);
                        failures.push(format!("{label}: cpmm fee config unresolved"));
                    }
                }
            }
        }

        // Seed vault balances so the constant-product pools are priceable before any
        // update arrives.
        if !vault_b58.is_empty() {
            let vaults = get_multiple_accounts(&client, rpc_http, &vault_b58).await?;
            for (b58, data) in vault_b58.iter().zip(vaults.iter()) {
                let Some(data) = data else { continue };
                let Ok(amount) = raydium_v4::decode_token_amount(data) else { continue };
                let Some(&(pool, first)) = vault_index.get(&pk(b58)?) else { continue };
                if let Some(w) = watches.get_mut(&pool) {
                    w.set_vault(first, amount);
                }
            }
            for (addr, w) in &watches {
                if let Some(ps) = w.vault_state(*addr) {
                    store.upsert(ps);
                }
            }
        }

        anyhow::ensure!(!watches.is_empty(), "no pools decoded — cannot start");

        for f in &failures {
            tracing::warn!("registry pool dropped — {f}");
        }

        let mut subscriptions: Vec<Pubkey32> = watches.keys().copied().collect();
        subscriptions.extend(vault_index.keys().copied());

        let mut market =
            Self { registry, watches, vault_index, store, subscriptions, usd: HashMap::new() };
        market.rebuild_usd_index();

        tracing::info!(
            "bootstrapped {} of {} pools ({} priceable, {} failed) across {} accounts",
            market.watches.len(),
            market.registry.pools.len(),
            market.store.len(),
            failures.len(),
            market.subscriptions.len()
        );
        Ok(market)
    }

    /// Apply one account update. Returns the pool whose state changed, if any.
    pub fn apply(&mut self, u: &AccountUpdate, bus: &EventBus) -> Option<PoolState> {
        let state = if let Some(w) = self.watches.get_mut(&u.pubkey) {
            w.slot = w.slot.max(u.slot);
            match &mut w.venue {
                Venue::Whirlpool => orca_whirlpool::to_pool_state(u.pubkey, &u.data, u.slot).ok()?,
                Venue::RaydiumClmm { trade_fee_ppm } => {
                    raydium_clmm::to_pool_state(u.pubkey, &u.data, *trade_fee_ppm, u.slot).ok()?
                }
                Venue::RaydiumV4 { info, .. } => {
                    // The pool account itself moved: fees and uncollected amounts
                    // change, and a stale copy corrupts the reserve calculation.
                    *info = Box::new(raydium_v4::decode_amm_info(&u.data).ok()?);
                    self.watches.get(&u.pubkey)?.vault_state(u.pubkey)?
                }
                Venue::RaydiumCpmm { pool, .. } => {
                    // Same reasoning: the accrued-fee counters live in this account and
                    // are subtracted from the vault balances to get the real reserve.
                    *pool = Box::new(raydium_cpmm::decode(&u.data).ok()?);
                    self.watches.get(&u.pubkey)?.vault_state(u.pubkey)?
                }
            }
        } else if let Some(&(pool, first)) = self.vault_index.get(&u.pubkey) {
            let amount = raydium_v4::decode_token_amount(&u.data).ok()?;
            let w = self.watches.get_mut(&pool)?;
            w.slot = w.slot.max(u.slot);
            w.set_vault(first, amount);
            self.watches.get(&pool)?.vault_state(pool)?
        } else {
            return None;
        };

        self.store.upsert(state);

        let (label, dex) = self
            .watches
            .get(&state.id.0)
            .map_or_else(|| (String::new(), state.dex), |w| (w.label.clone(), w.dex));

        if let Some(price) = self.ui_price(&state) {
            bus.publish(Event::PoolUpdate {
                pool: bs58::encode(state.id.0).into_string(),
                dex: format!("{} {}", dex.tag(), fee_label(state.fee_ppm)),
                pair: label,
                price,
                reserve_a: self.whole(&state.mint_a, state.reserve_a()),
                reserve_b: self.whole(&state.mint_b, state.reserve_b()),
                slot: state.slot,
                ts_ms: now_ms(),
            });
        }

        Some(state)
    }

    /// One full pass over every cycle from every base mint.
    pub fn sweep(&self, tradable_usd: f64, max_hops: usize) -> Sweep {
        let started = Instant::now();
        let snap = self.store.snapshot();
        let mut rows: Vec<EdgeRow> = Vec::new();
        let mut opportunities: Vec<LiveOpportunity> = Vec::new();
        let mut evaluated: u64 = 0;

        for base in &self.registry.base_mints {
            let Some(usd_per_unit) = self.usd_per_base_unit(base) else { continue };
            if usd_per_unit <= 0.0 {
                continue;
            }
            let max_in = (tradable_usd / usd_per_unit) as u128;

            for sc in survey_from_base(&snap, base, max_hops) {
                evaluated += 1;
                rows.push(EdgeRow {
                    route: self.route_label(&sc.cycle.mints),
                    venues: self.venue_label(&sc.cycle.pools),
                    hops: sc.cycle.hops(),
                    edge_bps: sc.edge_bps,
                    dislocation_bps: sc.dislocation_bps(),
                    fee_bps: sc.cycle.fee_bps(),
                    depth_usd: sc
                        .cycle
                        .legs
                        .first()
                        .map_or(0.0, |l| l.max_in.min(u128::from(u64::MAX)) as f64 * usd_per_unit),
                    slot: sc.cycle.slot(&snap),
                });
            }

            if max_in == 0 {
                continue;
            }
            for p in find_from_base(&snap, base, max_hops, max_in) {
                opportunities.push(LiveOpportunity {
                    route: self.route_label(&p.cycle.mints),
                    venues: self.venue_label(&p.cycle.pools),
                    hops: p.cycle.hops(),
                    size_usd: p.capped_in as f64 * usd_per_unit,
                    optimal_size_usd: p.optimal_in as f64 * usd_per_unit,
                    gross_profit_usd: p.profit as f64 * usd_per_unit,
                    profit_at_optimal_usd: p.profit_at_optimal as f64 * usd_per_unit,
                    edge_bps: cb_core::path::marginal_edge_bps(&p.cycle.legs).unwrap_or(0.0),
                    fee_bps: p.cycle.fee_bps(),
                    slot: p.cycle.slot(&snap),
                });
            }
        }

        rows.sort_by(|a, b| b.edge_bps.total_cmp(&a.edge_bps));
        rows.truncate(LEADERBOARD_ROWS);
        opportunities.sort_by(|a, b| b.gross_profit_usd.total_cmp(&a.gross_profit_usd));

        Sweep {
            evaluated,
            rows,
            opportunities,
            duration_us: started.elapsed().as_micros() as u64,
            slot: snap.newest_slot(),
        }
    }

    /// Rebuild USD-per-whole-token by walking out from the stablecoin anchors.
    ///
    /// Each round prices any still-unvalued mint through whichever pool connecting it
    /// to an already-valued one holds the most dollars. Depth is the tie-break because
    /// a thin pool's mid price is the easiest number in the market to push around, and
    /// a wrong valuation here silently rescales every trade size that follows.
    pub fn rebuild_usd_index(&mut self) {
        let snap = self.store.snapshot();
        let mut usd: HashMap<Pubkey32, f64> = HashMap::new();
        for anchor in [USDC, USDT] {
            if let Ok(mint) = pk(anchor) {
                usd.insert(mint, 1.0);
            }
        }

        for _ in 0..USD_INDEX_ROUNDS {
            // mint -> (depth of the pool that priced it, price)
            let mut candidate: HashMap<Pubkey32, (f64, f64)> = HashMap::new();
            for p in snap.pools() {
                let Some(spot) = p.spot_price() else { continue };
                if spot <= 0.0 || !spot.is_finite() {
                    continue;
                }
                let da = i32::from(self.registry.decimals(&p.mint_a));
                let db = i32::from(self.registry.decimals(&p.mint_b));
                // Whole units of B per whole unit of A.
                let b_per_a = spot * 10f64.powi(da - db);

                let whole_a = self.whole(&p.mint_a, p.reserve_a());
                let whole_b = self.whole(&p.mint_b, p.reserve_b());

                if let Some(&ub) = usd.get(&p.mint_b) {
                    if !usd.contains_key(&p.mint_a) {
                        let depth = whole_b * ub * 2.0;
                        let price = b_per_a * ub;
                        let e = candidate.entry(p.mint_a).or_insert((0.0, 0.0));
                        if depth > e.0 && price.is_finite() && price > 0.0 {
                            *e = (depth, price);
                        }
                    }
                }
                if let Some(&ua) = usd.get(&p.mint_a) {
                    if !usd.contains_key(&p.mint_b) {
                        let depth = whole_a * ua * 2.0;
                        let price = ua / b_per_a;
                        let e = candidate.entry(p.mint_b).or_insert((0.0, 0.0));
                        if depth > e.0 && price.is_finite() && price > 0.0 {
                            *e = (depth, price);
                        }
                    }
                }
            }
            if candidate.is_empty() {
                break;
            }
            for (mint, (_, price)) in candidate {
                usd.insert(mint, price);
            }
        }

        self.usd = usd;
    }

    /// USD value of one *base unit* of `mint`.
    #[must_use]
    pub fn usd_per_base_unit(&self, mint: &Pubkey32) -> Option<f64> {
        let whole = *self.usd.get(mint)?;
        Some(whole * 10f64.powi(-i32::from(self.registry.decimals(mint))))
    }

    /// SOL price in USD, derived from live pools like every other token.
    #[must_use]
    pub fn sol_price_usd(&self) -> Option<f64> {
        self.usd.get(&pk(WSOL).ok()?).copied()
    }

    /// Base units converted to whole tokens, for display.
    fn whole(&self, mint: &Pubkey32, amount: u128) -> f64 {
        amount as f64 * 10f64.powi(-i32::from(self.registry.decimals(mint)))
    }

    /// Price of token A in whole units of token B — what a human reads as "the price".
    fn ui_price(&self, p: &PoolState) -> Option<f64> {
        let spot = p.spot_price()?;
        let da = i32::from(self.registry.decimals(&p.mint_a));
        let db = i32::from(self.registry.decimals(&p.mint_b));
        let v = spot * 10f64.powi(da - db);
        v.is_finite().then_some(v)
    }

    fn route_label(&self, mints: &[Pubkey32]) -> String {
        mints.iter().map(|m| self.registry.symbol(m)).collect::<Vec<_>>().join(" → ")
    }

    fn venue_label(&self, pools: &[PoolId]) -> String {
        pools
            .iter()
            .map(|id| {
                let dex = self.watches.get(&id.0).map_or("?", |w| w.dex.tag());
                let fee = self.store.get(id).map_or_else(String::new, |p| fee_label(p.fee_ppm));
                format!("{dex} {fee}")
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    #[must_use]
    pub fn pool_count(&self) -> usize {
        self.watches.len()
    }

    /// Pools currently priceable. Raydium v4 pools missing a vault balance are not.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.store.len()
    }

    /// Distinct venues contributing at least one priceable pool.
    #[must_use]
    pub fn venue_count(&self) -> usize {
        let mut v: Vec<Dex> = self.store.snapshot().pools().iter().map(|p| p.dex).collect();
        v.sort();
        v.dedup();
        v.len()
    }
}

/// Render a parts-per-million fee the way a trader reads it.
#[must_use]
pub fn fee_label(fee_ppm: u32) -> String {
    let bps = f64::from(fee_ppm) / 100.0;
    if (bps.fract()).abs() < 0.005 {
        format!("{bps:.0}bp")
    } else {
        format!("{bps:.2}bp")
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
    use cb_core::types::PoolState;

    fn market_with(pools: Vec<PoolState>) -> LiveMarket {
        let store = PoolStore::new();
        for p in pools {
            store.upsert(p);
        }
        let mut m = LiveMarket {
            registry: Registry::embedded().unwrap(),
            watches: HashMap::new(),
            vault_index: HashMap::new(),
            store,
            subscriptions: Vec::new(),
            usd: HashMap::new(),
        };
        m.rebuild_usd_index();
        m
    }

    fn mint(s: &str) -> Pubkey32 {
        pk(s).unwrap()
    }

    /// Real Orca SOL/USDC state: 758.6e12 liquidity at tick -23953, spacing 4.
    fn sol_usdc(id: u8, sqrt_price: u128, fee_ppm: u32) -> PoolState {
        use cb_core::clmm;
        use cb_core::types::PoolMath;
        let (lo, hi) = clmm::bounds(-23953, 4).unwrap();
        PoolState {
            id: PoolId([id; 32]),
            dex: Dex::OrcaWhirlpool,
            mint_a: mint(WSOL),
            mint_b: mint(USDC),
            math: PoolMath::Concentrated {
                liquidity: 758_634_162_063_829,
                sqrt_price_x64: sqrt_price,
                sqrt_lo_x64: lo,
                sqrt_hi_x64: hi,
            },
            fee_ppm,
            slot: 100,
        }
    }

    #[test]
    fn fee_labels_read_the_way_a_trader_says_them() {
        assert_eq!(fee_label(100), "1bp");
        assert_eq!(fee_label(400), "4bp");
        assert_eq!(fee_label(2500), "25bp");
        assert_eq!(fee_label(30_000), "300bp");
        // A tier that is not a whole basis point must not be rounded into a lie.
        assert_eq!(fee_label(150), "1.50bp");
    }

    #[test]
    fn base64_decoder_matches_known_vectors() {
        assert_eq!(decode_b64("SGVsbG8=").unwrap(), b"Hello");
        assert_eq!(decode_b64("").unwrap(), Vec::<u8>::new());
    }

    /// The USD index must derive SOL's price from pools rather than a constant, and
    /// land somewhere a human would recognise.
    #[test]
    fn sol_is_priced_from_live_pool_state_not_a_hardcoded_number() {
        let m = market_with(vec![sol_usdc(1, 5_569_625_019_338_410_820, 400)]);
        let sol = m.sol_price_usd().expect("SOL must be priced from the SOL/USDC pool");
        assert!((sol - 91.0).abs() < 2.0, "expected roughly $91, got {sol}");

        // And per base unit: one lamport is a billionth of that.
        let per_unit = m.usd_per_base_unit(&mint(WSOL)).unwrap();
        assert!((per_unit * 1e9 - sol).abs() < 1e-6);
    }

    #[test]
    fn stablecoin_anchors_are_worth_a_dollar_and_nothing_else_is_assumed() {
        let m = market_with(vec![sol_usdc(1, 5_569_625_019_338_410_820, 400)]);
        assert_eq!(m.usd.get(&mint(USDC)), Some(&1.0));
        assert_eq!(m.usd.get(&mint(USDT)), Some(&1.0));
        // A token no pool connects to the anchors has no price at all — better than
        // a fabricated one.
        assert_eq!(m.usd_per_base_unit(&[42u8; 32]), None);
    }

    /// The measurement this whole build exists to make: two venues on one pair, and
    /// the sweep reporting how far apart they are and what crossing them costs.
    #[test]
    fn a_sweep_reports_dislocation_and_fees_separately() {
        // Same pool state, one priced 20 bps higher than the other.
        let cheap = sol_usdc(1, 5_569_625_019_338_410_820, 100);
        let dear = sol_usdc(2, 5_575_194_644_357_749_445, 200); // +10bps on sqrt = +20bps on price
        let m = market_with(vec![cheap, dear]);

        let sweep = m.sweep(4.80, 3);
        assert!(sweep.evaluated > 0, "a two-venue pair must produce cycles");

        let best = sweep.best().expect("there must be a best row");
        assert_eq!(best.hops, 2, "the cheapest loop is a direct round trip");
        assert!((best.fee_bps - 3.0).abs() < 0.05, "1bp + 2bp = 3bps, got {}", best.fee_bps);
        assert!(
            best.dislocation_bps > 15.0,
            "a 20 bp price gap must show up as one, got {}",
            best.dislocation_bps
        );
        assert!((best.dislocation_bps - best.fee_bps - best.edge_bps).abs() < 1e-6);
        assert!(best.edge_bps > 0.0, "20 bps of gap must clear 3 bps of fees");
    }

    #[test]
    fn a_profitable_sweep_sizes_within_our_capital() {
        let m = market_with(vec![
            sol_usdc(1, 5_569_625_019_338_410_820, 100),
            sol_usdc(2, 5_575_194_644_357_749_445, 200),
        ]);
        let sweep = m.sweep(4.80, 3);
        let opp = sweep.opportunities.first().expect("a clearing cycle must be sized");

        assert!(opp.size_usd <= 4.81, "must not size past our capital, got {}", opp.size_usd);
        assert!(opp.gross_profit_usd > 0.0);
        assert!(
            opp.optimal_size_usd > opp.size_usd,
            "the unconstrained optimum on a $24M pool must dwarf $5"
        );
    }

    #[test]
    fn two_agreeing_venues_produce_a_negative_edge_not_silence() {
        // Identical prices: the only thing left is the fee, so the edge is exactly
        // minus the fee. Reporting that is the point — it says the venues are
        // efficient, which is a different finding from having no route at all.
        let m = market_with(vec![
            sol_usdc(1, 5_569_625_019_338_410_820, 100),
            sol_usdc(2, 5_569_625_019_338_410_820, 200),
        ]);
        let sweep = m.sweep(4.80, 3);
        let best = sweep.best().expect("a route must still be reported");
        assert!(best.edge_bps < 0.0);
        assert!(best.dislocation_bps.abs() < 0.1, "prices agree, so no dislocation");
        assert!((best.edge_bps + best.fee_bps).abs() < 0.1, "the shortfall is entirely fees");
        assert!(sweep.opportunities.is_empty(), "nothing clears");
    }

    #[test]
    fn an_empty_market_sweeps_without_panicking() {
        let m = market_with(vec![]);
        let sweep = m.sweep(4.80, 3);
        assert_eq!(sweep.evaluated, 0);
        assert!(sweep.best().is_none());
        assert!(sweep.opportunities.is_empty());
    }

    #[test]
    fn depth_is_reported_in_dollars_and_exceeds_our_account() {
        let m = market_with(vec![
            sol_usdc(1, 5_569_625_019_338_410_820, 100),
            sol_usdc(2, 5_575_194_644_357_749_445, 200),
        ]);
        let best = m.sweep(4.80, 3).rows.into_iter().next().unwrap();
        assert!(best.depth_usd > 5.0, "a tick that cannot hold $5 is useless to us");
    }
}
