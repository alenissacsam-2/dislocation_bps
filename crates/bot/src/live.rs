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
use cb_dex::{meteora_damm_v2, meteora_dlmm, orca_whirlpool, pumpswap, raydium_clmm, raydium_cpmm, raydium_v4};
use cb_executor::encode::{to_pubkey, to_raw};
use cb_executor::pda::meteora_bin_array;
use cb_feed::AccountUpdate;
use cb_scanner::multi::{survey_and_find, SurveyedCycle};
use cb_scanner::store::PoolStore;
use cb_server::{Event, EventBus, RouteRow};
use std::collections::HashMap;
use std::sync::Arc;
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

/// Bin arrays either side of a binned pool's active one that the watcher subscribes to.
///
/// Five accounts per pool, covering 350 bins. Wide enough that the price staying inside
/// it is the normal case, narrow enough that a handful of these pools does not double
/// the subscription budget. See [`Venue::MeteoraDlmm`] for what happens when the price
/// leaves it.
const BIN_WINDOW: i64 = 2;

/// Slots a pool may go unheard-from before the sweep stops quoting it.
///
/// # Why this is deliberately loose
///
/// The obvious setting is tight — a minute or two — on the reasoning that a dropped
/// subscription should be caught fast. Measured, that is the wrong trade. `reconcile()`
/// already re-reads every watched account every 180 s and folds it back in, which
/// refreshes each pool's slot whether or not it traded. So a tight guard spends most
/// of its time excluding pools that the last reconcile *just proved correct*, purely
/// for the crime of being quiet since.
///
/// At 300 slots that cost was measured directly: 37% of sweeps dropped ~40 of 84
/// pools, and cycles priced fell from ~1260 to ~600. Losing a third of the cycle graph
/// understates every rate the instrument reports, and understates it invisibly — which
/// is a worse failure for a measurement than the 60 s of extra staleness it was buying
/// against a bound reconcile already holds at 180 s.
///
/// So this is set above the reconcile cadence: it fires only once reconcile itself has
/// stopped repairing, which is the failure that genuinely has no other backstop. In
/// normal operation it excludes nothing, and a non-zero `stale_excluded` becomes a real
/// signal rather than routine noise. `the_staleness_guard_does_not_fire_while_reconcile_is_working`
/// in `main.rs` pins that relationship so tightening one without the other fails a test.
pub const MAX_STALE_LAG_SLOTS: u64 = 1800;

/// What the bot knows about one watched pool beyond its current state.
enum Venue {
    /// Everything needed is in the pool account, including the fee.
    Whirlpool,
    /// Same: self-contained, and quotable across the pool's whole range rather than
    /// one tick of it.
    MeteoraDammV2,
    /// Everything except the fee, which comes from a shared config account.
    RaydiumClmm { trade_fee_ppm: u32 },
    /// Reserves live in two vaults, tracked separately and combined on read.
    RaydiumV4 {
        info: Box<raydium_v4::AmmInfo>,
        base_amount: Option<u64>,
        quote_amount: Option<u64>,
    },
    /// Meteora DLMM. The price is in the pool account and the depth is in separate bin
    /// accounts, so a window of those is subscribed to alongside it and kept here.
    ///
    /// # Why a window and not just the one array that matters
    ///
    /// Only the array holding the active bin is strictly needed, but which array that is
    /// moves with the price, and the subscription list is fixed when the feed starts. A
    /// window two arrays either side covers 350 bins — 3.5% of price on a
    /// one-basis-point pool — and when the price does leave it the pool simply stops
    /// being priceable until `reconcile` repairs it, which is the honest failure rather
    /// than a quote against bins nobody read.
    ///
    /// # Why each account carries its own slot
    ///
    /// The pool account and its bin arrays are separate subscriptions, and a swap writes
    /// both — but they arrive as two messages. Rebuilding on the first of them publishes a
    /// *fresh* active bin against *stale* bin contents, which is a torn read: the price has
    /// moved and the depth behind it has not caught up. The quote that comes out is too good,
    /// which is the only direction that costs money.
    ///
    /// So the state is stamped with the **older** of the two slots. Nothing is suppressed and
    /// nothing is guessed; the read is simply labelled as old as its oldest part, and the
    /// lag gate in `main.rs` — which already refuses anything priced more than
    /// `MAX_EXECUTABLE_LEG_LAG_SLOTS` behind the feed's own head — does the rest.
    MeteoraDlmm {
        pair: Box<meteora_dlmm::LbPair>,
        /// Slot the pool account was last seen at.
        pair_slot: u64,
        /// Each held bin array and the slot it arrived at.
        arrays: HashMap<i64, (u64, meteora_dlmm::BinArray)>,
    },
    /// PumpSwap. Vaults like v4; a fee that moves with the coin's market cap, so it is
    /// recomputed from the fee program's tiers every time a reserve moves. See
    /// `cb_dex::pumpswap`.
    PumpSwap {
        pool: Box<pumpswap::PumpPool>,
        base_amount: Option<u64>,
        quote_amount: Option<u64>,
        /// The base mint's supply, read at start; with the reserves it is the market cap.
        supply: u64,
        /// Whether pump.fun's migration created the pool, which picks tiered fees.
        pump: bool,
        /// The fee program's tiers, shared by every PumpSwap pool. `None` until read.
        fees: Option<Arc<pumpswap::FeeConfig>>,
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
            Venue::MeteoraDlmm { pair, pair_slot, arrays } => {
                // Only the arrays actually received. The decoder refuses unless the
                // active bin is among them, which is what makes a price that has walked
                // out of the subscribed window show up as a pool dropping out of the
                // sweep rather than as a quote against bins nobody read.
                //
                // The slot is the older of the pool account and the array holding the
                // active bin, because those two are what a quote is actually made of. See
                // the variant's own note for why taking the newer of them is a torn read.
                let active = meteora_dlmm::bin_array_index(pair.active_id);
                let bins_slot = arrays.get(&active).map(|(slot, _)| *slot)?;
                let held: Vec<meteora_dlmm::BinArray> =
                    arrays.values().map(|(_, a)| a.clone()).collect();
                meteora_dlmm::state_from(addr, pair, &held, (*pair_slot).min(bins_slot)).ok()
            }
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
            Venue::PumpSwap { pool, base_amount, quote_amount, supply, pump, fees } => {
                let (base, quote) = ((*base_amount)?, (*quote_amount)?);
                let cap = pumpswap::market_cap(*supply, base, pool.effective_quote(quote));
                let bps = fees.as_ref()?.fees(*pump, cap, false).total_bps();
                pumpswap::to_pool_state(addr, pool, base, quote, bps, self.slot).ok()
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
            Venue::PumpSwap { base_amount, quote_amount, .. } => {
                *(if first { base_amount } else { quote_amount }) = Some(amount);
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
    /// Identity of the loop itself, unchanged by which mint it was entered at. The
    /// printed `route` differs between the two entries; this does not.
    pub cycle_key: String,
    pub venues: String,
    pub hops: usize,
    pub size_usd: f64,
    pub optimal_size_usd: f64,
    pub gross_profit_usd: f64,
    pub profit_at_optimal_usd: f64,
    /// What this same cycle would have paid a book of each size in
    /// `cb_ledger::CAPITAL_LADDER_USD`, in USD, before tip.
    ///
    /// Priced against the live legs at the moment of detection rather than scaled from
    /// `profit_at_optimal_usd`: profit is concave in size and capped by the cycle's
    /// tightest tick, so the rungs are separate facts. Where they stop rising is where
    /// depth, not capital, became the constraint.
    pub profit_at_capital_usd: [f64; 3],
    pub edge_bps: f64,
    pub fee_bps: f64,
    /// Everything execution needs to rebuild this cycle as instructions.
    ///
    /// Built here, from the legs that produced the quote, because re-deriving it later
    /// from the recorded USD figures would be a guess about which pools were involved —
    /// and a swap against the wrong pool is not a rounding error. `None` when a leg
    /// would not quote at the chosen size, which is the same condition that makes the
    /// opportunity untradeable anyway.
    pub plan: Option<crate::execute::CyclePlan>,
    pub slot: u64,
    /// Slots between the freshest and stalest leg of this loop.
    ///
    /// A dislocation is a claim that two venues disagree *at one moment*. This is how
    /// far the claim actually reached: zero means every leg came from the same slot,
    /// and anything larger means part of the reported gap is the market having moved
    /// between two observations rather than two venues disagreeing. You cannot trade
    /// against a price that has already gone, so a large spread is a reason to distrust
    /// the edge beside it rather than to celebrate it.
    pub slot_spread: u64,
}

/// One pool, one direction, and what we claim it will pay — ready to be checked
/// against an outside opinion.
#[derive(Debug, Clone)]
pub struct AuditProbe {
    pub pair: String,
    pub label: String,
    pub venue: String,
    /// The pool being probed. When the router's own route runs through this exact
    /// account and still quotes less output, the disagreement cannot be blamed on it
    /// having priced a different pool — which makes it the strongest evidence a
    /// decode fault can produce.
    pub pool_b58: String,
    pub from_b58: String,
    pub to_b58: String,
    pub amount_in: u128,
    pub amount_out: u128,
}

/// What a reconciliation pass found.
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    pub checked: usize,
    /// Pools whose on-chain state differed from what the feed had delivered. A
    /// persistently non-zero count means the WebSocket is dropping updates.
    pub drifted: usize,
    pub missing: usize,
    pub undecodable: usize,
    pub slot: u64,
}

/// The result of one full pass over the cycle graph.
#[derive(Debug, Clone, Default)]
pub struct Sweep {
    pub evaluated: u64,
    /// Cycles that cleared their fees **and** can absorb the capital. Counted over
    /// every cycle priced, not over the truncated leaderboard.
    pub clearing: u64,
    /// The leaderboard, grouped: tradeable rows first, then rate-only rows, each
    /// group ordered by edge. A display list — `best` and `tradeable` are computed
    /// over everything and never depend on this truncation.
    pub rows: Vec<EdgeRow>,
    /// Highest marginal rate anywhere, whether or not any size fits behind it.
    /// A diagnostic, not a headline.
    pub best: Option<EdgeRow>,
    /// Highest rate among cycles deep enough to take the capital. `None` when nothing
    /// qualifies — which is a real answer and must never be rendered as zero.
    pub tradeable: Option<EdgeRow>,
    pub opportunities: Vec<LiveOpportunity>,
    pub duration_us: u64,
    pub slot: u64,
    /// Pools left out of this pass for lagging too far behind the newest slot.
    pub stale_excluded: usize,
    /// Depth a cycle needed to count as tradeable here, in USD.
    pub tradeable_min_usd: f64,
}

impl Sweep {
    /// The marginal leader: best rate, ignoring whether anything can be traded at it.
    #[must_use]
    pub fn best(&self) -> Option<&EdgeRow> {
        self.best.as_ref()
    }

    /// The best rate that actually has the capital's worth of depth behind it.
    #[must_use]
    pub fn tradeable(&self) -> Option<&EdgeRow> {
        self.tradeable.as_ref()
    }
}

pub struct LiveMarket {
    pub registry: Registry,
    rpc_http: String,
    client: reqwest::Client,
    watches: HashMap<Pubkey32, Watch>,
    /// vault address -> (pool address, is_base_vault)
    vault_index: HashMap<Pubkey32, (Pubkey32, bool)>,
    /// bin array address -> (pool address, array index)
    bin_index: HashMap<Pubkey32, (Pubkey32, i64)>,
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
        let (accounts, boot_slot) = get_multiple_accounts(&client, rpc_http, &addresses).await?;

        let mut watches: HashMap<Pubkey32, Watch> = HashMap::new();
        let mut vault_index = HashMap::new();
        let mut bin_index: HashMap<Pubkey32, (Pubkey32, i64)> = HashMap::new();
        let mut bin_b58: Vec<String> = Vec::new();
        let mut vault_b58: Vec<String> = Vec::new();
        let mut config_b58: Vec<String> = Vec::new();
        let mut clmm_pending: Vec<(Pubkey32, Pubkey32, Vec<u8>)> = Vec::new(); // pool, config, data
        let mut cpmm_config_b58: Vec<String> = Vec::new();
        let mut cpmm_pending: Vec<(Pubkey32, Pubkey32)> = Vec::new(); // pool, config
        let mut pump_pending: Vec<(Pubkey32, Pubkey32)> = Vec::new(); // pool, base mint
        let wsol_raw = pk("So11111111111111111111111111111111111111112")?;
        let store = PoolStore::new();
        let mut failures: Vec<String> = Vec::new();

        for (entry, account) in registry.pools.iter().zip(accounts.iter()) {
            let Some(data) = account else {
                failures.push(format!("{}: account not found", entry.label));
                continue;
            };
            let addr = entry.address;

            match entry.dex {
                Dex::OrcaWhirlpool => match orca_whirlpool::to_pool_state(addr, data, boot_slot) {
                    Ok(ps) => {
                        store.upsert(ps);
                        watches.insert(
                            addr,
                            Watch {
                                label: entry.label.clone(),
                                dex: entry.dex,
                                venue: Venue::Whirlpool,
                                slot: boot_slot,
                            },
                        );
                    }
                    Err(e) => failures.push(format!("{}: {e:#}", entry.label)),
                },
                Dex::MeteoraDammV2 => match meteora_damm_v2::to_pool_state(addr, data, boot_slot)
                {
                    Ok(ps) => {
                        store.upsert(ps);
                        watches.insert(
                            addr,
                            Watch {
                                label: entry.label.clone(),
                                dex: entry.dex,
                                venue: Venue::MeteoraDammV2,
                                slot: boot_slot,
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
                                slot: boot_slot,
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
                                slot: boot_slot,
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
                                slot: boot_slot,
                            },
                        );
                    }
                    Err(e) => failures.push(format!("{}: {e:#}", entry.label)),
                },
                Dex::MeteoraDlmm => match meteora_dlmm::decode(data) {
                    Ok(pair) => {
                        let program = cb_executor::encode::pk(meteora_dlmm::PROGRAM_ID);
                        let pool_key = to_pubkey(&addr);
                        let centre = meteora_dlmm::bin_array_index(pair.active_id);
                        for d in -BIN_WINDOW..=BIN_WINDOW {
                            let ix = centre + d;
                            let key = to_raw(&meteora_bin_array(&pool_key, ix, &program));
                            bin_index.insert(key, (addr, ix));
                            bin_b58.push(bs58::encode(key).into_string());
                        }
                        watches.insert(
                            addr,
                            Watch {
                                label: entry.label.clone(),
                                dex: entry.dex,
                                venue: Venue::MeteoraDlmm {
                                    pair: Box::new(pair),
                                    pair_slot: boot_slot,
                                    arrays: HashMap::new(),
                                },
                                slot: boot_slot,
                            },
                        );
                    }
                    Err(e) => failures.push(format!("{}: {e:#}", entry.label)),
                },
                Dex::PumpSwap => match pumpswap::decode_pool(data) {
                    // Refused until their fee splits have been checked against the
                    // program the way ordinary pools were.
                    Ok(p) if p.is_mayhem_mode || p.is_cashback_coin => failures.push(format!(
                        "{}: a mayhem-mode or cashback PumpSwap pool, whose fees have not been \
                         verified",
                        entry.label
                    )),
                    Ok(p) if cb_executor::venue::pumpswap::is_pump_pool(&p) && p.quote_mint != wsol_raw => {
                        failures.push(format!(
                            "{}: a pump pool quoted in something other than SOL, whose \
                             stable-coin fee tiers have not been verified",
                            entry.label
                        ));
                    }
                    Ok(p) => {
                        let pump = cb_executor::venue::pumpswap::is_pump_pool(&p);
                        vault_index.insert(p.base_vault, (addr, true));
                        vault_index.insert(p.quote_vault, (addr, false));
                        vault_b58.push(bs58::encode(p.base_vault).into_string());
                        vault_b58.push(bs58::encode(p.quote_vault).into_string());
                        pump_pending.push((addr, p.base_mint));
                        watches.insert(
                            addr,
                            Watch {
                                label: entry.label.clone(),
                                dex: entry.dex,
                                venue: Venue::PumpSwap {
                                    pool: Box::new(p),
                                    base_amount: None,
                                    quote_amount: None,
                                    supply: 0,
                                    pump,
                                    fees: None,
                                },
                                slot: boot_slot,
                            },
                        );
                    }
                    Err(e) => failures.push(format!("{}: {e:#}", entry.label)),
                },
            }
        }

        // Resolve Raydium CLMM fees from their shared config accounts.
        config_b58.sort();
        config_b58.dedup();
        let mut fee_by_config: HashMap<Pubkey32, u32> = HashMap::new();
        if !config_b58.is_empty() {
            let (configs, _) = get_multiple_accounts(&client, rpc_http, &config_b58).await?;
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
            match raydium_clmm::to_pool_state(addr, &data, fee, boot_slot, now_ms() / 1_000) {
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
            let (configs, _) = get_multiple_accounts(&client, rpc_http, &cpmm_config_b58).await?;
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

        // PumpSwap's fee tiers, shared, and each coin's supply, which with the reserves
        // is the market cap that picks a tier.
        if !pump_pending.is_empty() {
            let mut keys = vec![bs58::encode(cb_executor::venue::pumpswap::fee_config()).into_string()];
            keys.extend(pump_pending.iter().map(|(_, mint)| bs58::encode(mint).into_string()));
            let (got, _) = get_multiple_accounts(&client, rpc_http, &keys).await?;
            let cfg = got
                .first()
                .and_then(Option::as_ref)
                .and_then(|d| pumpswap::decode_fee_config(d).ok())
                .map(Arc::new);
            for ((addr, _), mint) in pump_pending.iter().zip(got.iter().skip(1)) {
                let supply = mint
                    .as_ref()
                    .and_then(|d| d.get(36..44))
                    .and_then(|b| b.try_into().ok())
                    .map(u64::from_le_bytes);
                match (cfg.clone(), supply) {
                    (Some(c), Some(n)) => {
                        if let Some(Venue::PumpSwap { supply, fees, .. }) =
                            watches.get_mut(addr).map(|w| &mut w.venue)
                        {
                            *supply = n;
                            *fees = Some(c);
                        }
                    }
                    _ => {
                        let label = watches.get(addr).map_or_else(String::new, |w| w.label.clone());
                        watches.remove(addr);
                        failures.push(format!("{label}: pumpswap fee config or coin supply unreadable"));
                    }
                }
            }
        }

        // Seed vault balances so the constant-product pools are priceable before any
        // update arrives.
        if !vault_b58.is_empty() {
            let (vaults, _) = get_multiple_accounts(&client, rpc_http, &vault_b58).await?;
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

        // Seed the bin arrays, so a binned pool is priceable before any update arrives.
        //
        // A pool whose active array comes back missing is not dropped: bin arrays are
        // created when somebody opens a position in that range, so an absent one is a
        // fact about the market rather than an error, and the decoder already refuses to
        // quote without the array that matters.
        if !bin_b58.is_empty() {
            let (arrays, _) = get_multiple_accounts(&client, rpc_http, &bin_b58).await?;
            for (b58, data) in bin_b58.iter().zip(arrays.iter()) {
                let Some(data) = data else { continue };
                let Ok(decoded) = meteora_dlmm::decode_bin_array(data) else { continue };
                let Some(&(pool, ix)) = bin_index.get(&pk(b58)?) else { continue };
                if let Some(Venue::MeteoraDlmm { arrays, .. }) =
                    watches.get_mut(&pool).map(|w| &mut w.venue)
                {
                    arrays.insert(ix, (boot_slot, decoded));
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
        subscriptions.extend(bin_index.keys().copied());

        let mut market = Self {
            registry,
            rpc_http: rpc_http.to_string(),
            client,
            watches,
            vault_index,
            bin_index,
            store,
            subscriptions,
            usd: HashMap::new(),
        };
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

    /// How many pools currently price.
    #[must_use]
    pub fn store_len(&self) -> usize {
        self.store.len()
    }

    /// Apply one account update. Returns the pool whose state changed, if any.
    pub fn apply(&mut self, u: &AccountUpdate, bus: &EventBus) -> Option<PoolState> {
        let state = if let Some(w) = self.watches.get_mut(&u.pubkey) {
            w.slot = w.slot.max(u.slot);
            match &mut w.venue {
                Venue::Whirlpool => orca_whirlpool::to_pool_state(u.pubkey, &u.data, u.slot).ok()?,
                Venue::MeteoraDammV2 => {
                    meteora_damm_v2::to_pool_state(u.pubkey, &u.data, u.slot).ok()?
                }
                Venue::RaydiumClmm { trade_fee_ppm } => {
                    raydium_clmm::to_pool_state(
                        u.pubkey,
                        &u.data,
                        *trade_fee_ppm,
                        u.slot,
                        now_ms() / 1_000,
                    )
                    .ok()?
                }
                Venue::MeteoraDlmm { pair, pair_slot, .. } => {
                    // The active bin lives in this account and decides which of the
                    // arrays already held is the one that prices the pool, so a stale
                    // copy would price it from the wrong bin.
                    **pair = meteora_dlmm::decode(&u.data).ok()?;
                    *pair_slot = u.slot;
                    self.watches.get(&u.pubkey)?.vault_state(u.pubkey)?
                }
                Venue::RaydiumV4 { info, .. } => {
                    // The pool account itself moved: fees and uncollected amounts
                    // change, and a stale copy corrupts the reserve calculation.
                    **info = raydium_v4::decode_amm_info(&u.data).ok()?;
                    self.watches.get(&u.pubkey)?.vault_state(u.pubkey)?
                }
                Venue::RaydiumCpmm { pool, .. } => {
                    // Same reasoning: the accrued-fee counters live in this account and
                    // are subtracted from the vault balances to get the real reserve.
                    **pool = raydium_cpmm::decode(&u.data).ok()?;
                    self.watches.get(&u.pubkey)?.vault_state(u.pubkey)?
                }
                Venue::PumpSwap { pool, .. } => {
                    // The virtual quote reserve lives in this account.
                    **pool = pumpswap::decode_pool(&u.data).ok()?;
                    self.watches.get(&u.pubkey)?.vault_state(u.pubkey)?
                }
            }
        } else if let Some(&(pool, first)) = self.vault_index.get(&u.pubkey) {
            let amount = raydium_v4::decode_token_amount(&u.data).ok()?;
            let w = self.watches.get_mut(&pool)?;
            w.slot = w.slot.max(u.slot);
            w.set_vault(first, amount);
            self.watches.get(&pool)?.vault_state(pool)?
        } else if let Some(&(pool, ix)) = self.bin_index.get(&u.pubkey) {
            let array = meteora_dlmm::decode_bin_array(&u.data).ok()?;
            let w = self.watches.get_mut(&pool)?;
            w.slot = w.slot.max(u.slot);
            if let Venue::MeteoraDlmm { arrays, .. } = &mut w.venue {
                arrays.insert(ix, (u.slot, array));
            }
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
    ///
    /// # Two searches, two different questions
    ///
    /// `survey_from_base` prices every cycle at infinitesimal size: that is a *rate*,
    /// and it is what the leaderboard and the ledger's edge statistics are built from.
    /// `find_from_base` sizes cycles against real capital and real tick depth, and
    /// only its results ever become fills.
    ///
    /// These disagree far more than they look like they should, which is why
    /// [`Sweep::tradeable`] exists beside [`Sweep::best`]. A cycle whose downstream leg
    /// is parked at the end of its tick has an enormous marginal rate with nearly no
    /// capacity behind it; ranked on rate alone it leads the board while being
    /// untradeable. Publishing only the marginal maximum overstated this instrument's
    /// headline edge for its whole history before the two were split apart.
    /// `tradable_usd` caps how much any one trade may deploy. `min_depth_usd` is the
    /// smallest trade worth carrying, and so the depth a cycle must have before its rate
    /// counts as an opportunity rather than a quote.
    ///
    /// These were one argument until the book outgrew the pools. Requiring a cycle to
    /// absorb the *whole* balance is only equivalent to requiring it to absorb a
    /// worthwhile trade while the balance is small; past that it starts discarding
    /// cycles a larger account could trade perfectly well, just not all at once.
    pub fn sweep(&self, tradable_usd: f64, min_depth_usd: f64, max_hops: usize) -> Sweep {
        let started = Instant::now();
        // Pools we have not heard from in a while are dropped rather than quoted.
        let (snap, stale_excluded) = self.store.snapshot_fresh(MAX_STALE_LAG_SLOTS);
        // Every surveyed cycle, by base mint, with the two numbers the leaderboard sorts
        // and filters on. Labels are strings built per cycle, and only the handful of
        // rows actually shown need them — building them for every cycle every sweep was
        // most of a sweep's allocations.
        let mut surveyed: Vec<(f64, Vec<SurveyedCycle>)> = Vec::new();
        let mut light: Vec<(usize, usize, f64, f64)> = Vec::new(); // base, cycle, edge, depth
        let mut opportunities: Vec<LiveOpportunity> = Vec::new();
        let mut evaluated: u64 = 0;

        for base in &self.registry.base_mints {
            let Some(usd_per_unit) = self.usd_per_base_unit(base) else { continue };
            if usd_per_unit <= 0.0 {
                continue;
            }
            let max_in = (tradable_usd / usd_per_unit) as u128;
            let (cycles, found) = survey_and_find(&snap, base, max_hops, max_in);
            let b = surveyed.len();
            for (i, sc) in cycles.iter().enumerate() {
                evaluated += 1;
                // The bottleneck across every leg, in dollars — not the entry pool's own
                // reserve, which is almost never the binding one. A constant-product leg
                // is unbounded in principle and so is capped at its input reserve;
                // infinite liquidity is not a thing.
                let depth_usd = cb_core::path::cycle_depth_base(&sc.cycle.legs) as f64 * usd_per_unit;
                light.push((b, i, sc.edge_bps, depth_usd));
            }
            surveyed.push((usd_per_unit, cycles));

            for p in found {
                // Priced here, while the legs are the ones that produced the detection.
                // Re-deriving it later from the recorded USD figures would be a guess:
                // the curve's shape lives in the legs, not in its optimum.
                let mut ladder = [0.0f64; 3];
                for (rung, out) in cb_ledger::CAPITAL_LADDER_USD.iter().zip(&mut ladder) {
                    let units = (rung / usd_per_unit) as u128;
                    *out = cb_core::path::profit_at_capital(&p.cycle.legs, units) as f64
                        * usd_per_unit;
                }

                // Quote each leg in turn at the size actually chosen. `chain_quote`
                // returns only the final amount, and execution needs the intermediate
                // ones: each hop's floor is derived from its own quote, and a floor
                // taken from the wrong leg is a floor the pool cannot meet.
                let plan = (|| {
                    let mut leg_out = Vec::with_capacity(p.cycle.legs.len());
                    let mut amt = p.capped_in;
                    for leg in &p.cycle.legs {
                        let out = leg.quote(amt)?;
                        leg_out.push(out);
                        amt = out;
                    }
                    let pools: Vec<_> = p
                        .cycle
                        .pools
                        .iter()
                        .map(|id| snap.get(id).map(|st| (id.0, st.dex)))
                        .collect::<Option<Vec<_>>>()?;
                    // Configuration, not price: carried so execution can re-price a
                    // concentrated leg against fresh state without a round trip for a
                    // number a swap cannot change. See `CyclePlan::fee_ppm`. A Raydium
                    // CLMM pool's store fee includes its dynamic part, which does
                    // change, so its config fee is carried instead and the dynamic part
                    // is read again from the fresh account.
                    let fee_ppm: Vec<u32> = p
                        .cycle
                        .pools
                        .iter()
                        .map(|id| {
                            let st = snap.get(id)?;
                            Some(match self.watches.get(&id.0).map(|w| &w.venue) {
                                Some(Venue::RaydiumClmm { trade_fee_ppm }) => *trade_fee_ppm,
                                _ => st.fee_ppm,
                            })
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Some(crate::execute::CyclePlan {
                        pools,
                        mints: p.cycle.mints.clone(),
                        amount_in: p.capped_in,
                        leg_out,
                        fee_ppm,
                    })
                })();

                opportunities.push(LiveOpportunity {
                    route: self.route_label(&p.cycle.mints),
                    cycle_key: p.cycle.canonical_key(),
                    plan,
                    venues: self.venue_label(&p.cycle.pools),
                    hops: p.cycle.hops(),
                    size_usd: p.capped_in as f64 * usd_per_unit,
                    optimal_size_usd: p.optimal_in as f64 * usd_per_unit,
                    gross_profit_usd: p.profit as f64 * usd_per_unit,
                    profit_at_optimal_usd: p.profit_at_optimal as f64 * usd_per_unit,
                    profit_at_capital_usd: ladder,
                    edge_bps: cb_core::path::marginal_edge_bps(&p.cycle.legs).unwrap_or(0.0),
                    fee_bps: p.cycle.fee_bps(),
                    slot: p.cycle.slot(&snap),
                    slot_spread: p.cycle.slot_spread(&snap),
                });
            }
        }

        // Counted before truncation: the old count was capped by the size of the
        // leaderboard, so a market with fifty clearing cycles reported twelve.
        let clearing = light
            .iter()
            .filter(|(_, _, edge, depth)| *edge > 0.0 && *depth >= min_depth_usd)
            .count() as u64;

        light.sort_by(|a, b| b.2.total_cmp(&a.2));
        let row = |&(b, i, edge_bps, depth_usd): &(usize, usize, f64, f64)| {
            let sc = &surveyed[b].1[i];
            EdgeRow {
                route: self.route_label(&sc.cycle.mints),
                venues: self.venue_label(&sc.cycle.pools),
                hops: sc.cycle.hops(),
                edge_bps,
                dislocation_bps: sc.dislocation_bps(),
                fee_bps: sc.cycle.fee_bps(),
                depth_usd,
                slot: sc.cycle.slot(&snap),
            }
        };
        let best = light.first().map(row);
        let tradeable = light.iter().find(|l| l.3 >= min_depth_usd).map(row);

        // Keep the head of both groups. The board shows what can be traded above what
        // is only a rate, and neither group can truncate the other out of existence.
        let mut kept: Vec<EdgeRow> = Vec::with_capacity(LEADERBOARD_ROWS * 2);
        kept.extend(light.iter().filter(|l| l.3 >= min_depth_usd).take(LEADERBOARD_ROWS).map(row));
        kept.extend(light.iter().filter(|l| l.3 < min_depth_usd).take(LEADERBOARD_ROWS).map(row));

        opportunities.sort_by(|a, b| b.gross_profit_usd.total_cmp(&a.gross_profit_usd));

        Sweep {
            evaluated,
            clearing,
            rows: kept,
            best,
            tradeable,
            opportunities,
            duration_us: started.elapsed().as_micros() as u64,
            slot: snap.newest_slot(),
            stale_excluded,
            tradeable_min_usd: min_depth_usd,
        }
    }

    /// Re-read every watched account over HTTP and fold it back into the store.
    ///
    /// # Why this is not paranoia
    ///
    /// For an AMM, *no update means no change*: the account only moves when someone
    /// swaps it, so an old slot on a quiet pool is a correct price, not a stale one.
    /// That is exactly what makes a silently dropped subscription so dangerous — it
    /// looks identical to a pool nobody is trading, and there is no way to tell them
    /// apart from the WebSocket stream alone.
    ///
    /// Re-reading the whole set on a timer settles it. Returns how many pools came
    /// back different from what the feed had, which is a direct measurement of how
    /// much the WebSocket is missing rather than an assumption about it.
    pub async fn reconcile(&mut self) -> Result<ReconcileReport> {
        self.reread(None).await
    }

    /// Re-read these pools now, with their vaults and bin arrays, instead of waiting for
    /// the next reconcile.
    ///
    /// For after an attempt has shown the feed's copy of a pool to be wrong. On
    /// 2026-09-24 one Meteora DLMM pool's dropped update left it priced about 50 bps
    /// off for minutes, and every cycle through it was detected, fetched and refused in
    /// turn — the same phantom, re-attempted under a different partner venue each sweep
    /// until the reconcile timer came round.
    ///
    /// # Errors
    /// If the chain cannot be reached; the store is then left as it was.
    pub async fn refresh(&mut self, pools: &[Pubkey32]) -> Result<ReconcileReport> {
        let only: std::collections::HashSet<Pubkey32> = pools.iter().copied().collect();
        self.reread(Some(&only)).await
    }

    async fn reread(
        &mut self,
        only: Option<&std::collections::HashSet<Pubkey32>>,
    ) -> Result<ReconcileReport> {
        let wanted = |pool: &Pubkey32| only.is_none_or(|set| set.contains(pool));
        let addresses: Vec<String> = self
            .watches
            .keys()
            .filter(|k| wanted(k))
            .map(|k| bs58::encode(k).into_string())
            .collect();
        if addresses.is_empty() {
            return Ok(ReconcileReport::default());
        }
        let (accounts, slot) =
            get_multiple_accounts(&self.client, &self.rpc_http, &addresses).await?;

        // Vault balances first, so a constant-product pool is rebuilt from a coherent
        // set rather than a fresh pool account against last week's vault.
        let vault_b58: Vec<String> = self
            .vault_index
            .iter()
            .filter(|(_, (pool, _))| wanted(pool))
            .map(|(k, _)| bs58::encode(k).into_string())
            .collect();
        if !vault_b58.is_empty() {
            let (vaults, _) =
                get_multiple_accounts(&self.client, &self.rpc_http, &vault_b58).await?;
            for (b58, data) in vault_b58.iter().zip(vaults.iter()) {
                let Some(data) = data else { continue };
                let Ok(amount) = raydium_v4::decode_token_amount(data) else { continue };
                let Ok(key) = pk(b58) else { continue };
                let Some(&(pool, first)) = self.vault_index.get(&key) else { continue };
                if let Some(w) = self.watches.get_mut(&pool) {
                    w.slot = w.slot.max(slot);
                    w.set_vault(first, amount);
                }
            }
        }

        // Bin arrays next, for the same reason and with the same ordering: a binned pool
        // must be rebuilt from a coherent set rather than from a fresh pool account
        // against bins read minutes ago.
        let bin_b58: Vec<String> = self
            .bin_index
            .iter()
            .filter(|(_, (pool, _))| wanted(pool))
            .map(|(k, _)| bs58::encode(k).into_string())
            .collect();
        if !bin_b58.is_empty() {
            let (arrays, _) =
                get_multiple_accounts(&self.client, &self.rpc_http, &bin_b58).await?;
            for (b58, data) in bin_b58.iter().zip(arrays.iter()) {
                let Some(data) = data else { continue };
                let Ok(decoded) = meteora_dlmm::decode_bin_array(data) else { continue };
                let Ok(key) = pk(b58) else { continue };
                let Some(&(pool, ix)) = self.bin_index.get(&key) else { continue };
                if let Some(w) = self.watches.get_mut(&pool) {
                    w.slot = w.slot.max(slot);
                    if let Venue::MeteoraDlmm { arrays, .. } = &mut w.venue {
                        arrays.insert(ix, (slot, decoded));
                    }
                }
            }
        }

        let mut report = ReconcileReport { checked: addresses.len(), slot, ..Default::default() };
        for (b58, data) in addresses.iter().zip(accounts.iter()) {
            let Ok(addr) = pk(b58) else { continue };
            let Some(data) = data else {
                report.missing += 1;
                continue;
            };
            let before = self.store.get(&PoolId(addr));
            let Some(state) = self.decode_at(addr, data, slot) else {
                report.undecodable += 1;
                continue;
            };
            // Compare the priced state, not the slot: only a changed price matters.
            let changed = before.is_none_or(|b| {
                b.math != state.math || b.fee_ppm != state.fee_ppm
            });
            if changed {
                report.drifted += 1;
            }
            self.store.upsert(state);
        }
        Ok(report)
    }

    /// Decode one watched account at `slot`, using whatever venue it belongs to.
    fn decode_at(&mut self, addr: Pubkey32, data: &[u8], slot: u64) -> Option<PoolState> {
        let w = self.watches.get_mut(&addr)?;
        w.slot = w.slot.max(slot);
        match &mut w.venue {
            Venue::Whirlpool => orca_whirlpool::to_pool_state(addr, data, slot).ok(),
            Venue::MeteoraDammV2 => meteora_damm_v2::to_pool_state(addr, data, slot).ok(),
            Venue::RaydiumClmm { trade_fee_ppm } => {
                raydium_clmm::to_pool_state(addr, data, *trade_fee_ppm, slot, now_ms() / 1_000).ok()
            }
            Venue::MeteoraDlmm { pair, pair_slot, .. } => {
                **pair = meteora_dlmm::decode(data).ok()?;
                *pair_slot = slot;
                self.watches.get(&addr)?.vault_state(addr)
            }
            Venue::RaydiumV4 { info, .. } => {
                **info = raydium_v4::decode_amm_info(data).ok()?;
                self.watches.get(&addr)?.vault_state(addr)
            }
            Venue::RaydiumCpmm { pool, .. } => {
                **pool = raydium_cpmm::decode(data).ok()?;
                self.watches.get(&addr)?.vault_state(addr)
            }
            Venue::PumpSwap { pool, .. } => {
                **pool = pumpswap::decode_pool(data).ok()?;
                self.watches.get(&addr)?.vault_state(addr)
            }
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

    /// One probe per pool per direction, sized at roughly `usd` of the input token.
    ///
    /// Each carries what *we* say the pool will pay, so an outside quote for the same
    /// swap can be compared against it directly. See `verify` in the binary for why
    /// the comparison is one-sided.
    #[must_use]
    pub fn audit_probes(&self, usd: f64) -> Vec<AuditProbe> {
        let snap = self.store.snapshot();
        let mut out = Vec::new();
        for p in snap.pools() {
            let label = self.watches.get(&p.id.0).map_or_else(String::new, |w| w.label.clone());
            for (from, to) in [(p.mint_a, p.mint_b), (p.mint_b, p.mint_a)] {
                let Some(per_unit) = self.usd_per_base_unit(&from) else { continue };
                if per_unit <= 0.0 {
                    continue;
                }
                let amount_in = (usd / per_unit) as u128;
                if amount_in == 0 {
                    continue;
                }
                let Some(leg) = p.leg_for_input(&from) else { continue };
                let Some(amount_out) = leg.quote(amount_in) else { continue };
                out.push(AuditProbe {
                    pair: format!(
                        "{}->{}",
                        self.registry.symbol(&from),
                        self.registry.symbol(&to)
                    ),
                    label: label.clone(),
                    venue: format!("{} {}", p.dex.tag(), fee_label(p.fee_ppm)),
                    pool_b58: bs58::encode(p.id.0).into_string(),
                    from_b58: bs58::encode(from).into_string(),
                    to_b58: bs58::encode(to).into_string(),
                    amount_in,
                    amount_out,
                });
            }
        }
        out.sort_by(|a, b| a.pair.cmp(&b.pair).then_with(|| a.venue.cmp(&b.venue)));
        out
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

/// `getMultipleAccounts`, returning raw account data in request order plus the slot
/// the RPC answered at.
///
/// The slot matters: a pool seeded at slot 0 looks infinitely stale forever, and any
/// freshness check built on it is meaningless.
async fn get_multiple_accounts(
    client: &reqwest::Client,
    url: &str,
    keys: &[String],
) -> Result<(Vec<Option<Vec<u8>>>, u64)> {
    // The RPC caps this at 100 keys per call.
    let mut out = Vec::with_capacity(keys.len());
    let mut context_slot = 0u64;
    for chunk in keys.chunks(100) {
        // `processed`, to match the subscription. The default is `finalized`, which
        // is ~13 slots behind — so a reconciliation read would routinely come back
        // *older* than what the WebSocket had already delivered, and every one of
        // those would be counted as the feed having drifted. The drift number is only
        // worth having if both sides are looking at the same point in the chain.
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getMultipleAccounts",
            "params": [chunk, {"encoding": "base64", "commitment": "processed"}]
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
        if let Some(slot) =
            resp.get("result").and_then(|r| r.get("context")).and_then(|c| c.get("slot")).and_then(serde_json::Value::as_u64)
        {
            context_slot = context_slot.max(slot);
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
    Ok((out, context_slot))
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
            rpc_http: String::new(),
            client: reqwest::Client::new(),
            watches: HashMap::new(),
            vault_index: HashMap::new(),
            bin_index: HashMap::new(),
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

    /// A binned pool is two subscriptions, and a quote is made of both. The state must
    /// therefore be stamped with the **older** of them.
    ///
    /// # The failure this stops
    ///
    /// A swap writes the pool account and the bin arrays it touched in the same slot, but
    /// they arrive as two messages. Rebuilding on whichever lands first publishes a fresh
    /// active bin against bin contents from before the swap — the price has moved and the
    /// depth behind it has not. That quote is better than the pool will honour, which is
    /// the one direction that costs money rather than opportunities.
    ///
    /// Taking the older slot does not suppress the state or guess at it. It labels the read
    /// as old as its oldest part, which is what the lag gate in `main.rs` already knows how
    /// to refuse.
    #[test]
    fn a_binned_pool_is_as_stale_as_its_stalest_account() {
        fn b64(s: &str) -> Vec<u8> {
            let table = |c: u8| -> i32 {
                match c {
                    b'A'..=b'Z' => i32::from(c - b'A'),
                    b'a'..=b'z' => i32::from(c - b'a') + 26,
                    b'0'..=b'9' => i32::from(c - b'0') + 52,
                    b'+' => 62,
                    b'/' => 63,
                    _ => -1,
                }
            };
            let (mut acc, mut bits, mut out) = (0i32, 0, Vec::new());
            for &c in s.trim().as_bytes() {
                let v = table(c);
                if v < 0 {
                    continue;
                }
                acc = (acc << 6) | v;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    out.push(u8::try_from((acc >> bits) & 0xFF).unwrap_or(0));
                }
            }
            out
        }

        let pool_bytes = b64(include_str!("../../dex/tests/fixtures/meteora_dlmm_pair.b64"));
        let array_bytes =
            b64(include_str!("../../dex/tests/fixtures/meteora_dlmm_bin_array.b64"));
        let pair = meteora_dlmm::decode(&pool_bytes).expect("the fixture decodes");
        let array = meteora_dlmm::decode_bin_array(&array_bytes).expect("the fixture decodes");
        let addr = array.lb_pair;
        let index = meteora_dlmm::bin_array_index(pair.active_id);

        let watch = |pair_slot: u64, array_slot: u64| Watch {
            label: "SOL/USDC".into(),
            dex: Dex::MeteoraDlmm,
            venue: Venue::MeteoraDlmm {
                pair: Box::new(pair),
                pair_slot,
                arrays: HashMap::from([(index, (array_slot, array.clone()))]),
            },
            // Deliberately newer than either. The watch's own slot says when we last
            // heard anything at all; it is not what the quote was made of.
            slot: 1_000,
        };

        // Both fresh: the state is as fresh as they are.
        let both = watch(900, 900).vault_state(addr).expect("a coherent read prices");
        assert_eq!(both.slot, 900);

        // Pool account moved, bins have not caught up — the torn read.
        let torn = watch(900, 850).vault_state(addr).expect("still prices");
        assert_eq!(torn.slot, 850, "a fresh price over stale depth must report the stale slot");

        // And the other way round, which is equally torn.
        let other = watch(850, 900).vault_state(addr).expect("still prices");
        assert_eq!(other.slot, 850);

        // The watch's own slot never leaks into the quote.
        assert!(torn.slot < 1_000 && both.slot < 1_000);

        // And with no array for the active bin there is nothing to price at all.
        let blind = Watch {
            label: "SOL/USDC".into(),
            dex: Dex::MeteoraDlmm,
            venue: Venue::MeteoraDlmm {
                pair: Box::new(pair),
                pair_slot: 900,
                arrays: HashMap::new(),
            },
            slot: 900,
        };
        assert!(blind.vault_state(addr).is_none());
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

        let sweep = m.sweep(4.80, 4.80, 3);
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
        let sweep = m.sweep(4.80, 4.80, 3);
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
        let sweep = m.sweep(4.80, 4.80, 3);
        let best = sweep.best().expect("a route must still be reported");
        assert!(best.edge_bps < 0.0);
        assert!(best.dislocation_bps.abs() < 0.1, "prices agree, so no dislocation");
        assert!((best.edge_bps + best.fee_bps).abs() < 0.1, "the shortfall is entirely fees");
        assert!(sweep.opportunities.is_empty(), "nothing clears");
    }

    #[test]
    fn an_empty_market_sweeps_without_panicking() {
        let m = market_with(vec![]);
        let sweep = m.sweep(4.80, 4.80, 3);
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
        let best = m.sweep(4.80, 4.80, 3).rows.into_iter().next().unwrap();
        assert!(best.depth_usd > 5.0, "a tick that cannot hold $5 is useless to us");
    }

    /// Same pool, far less liquidity behind it. The price — and so the marginal rate —
    /// is unchanged, because a rate is a ratio of reserves and ratios do not scale.
    /// Only the depth moves.
    fn thinned(mut p: PoolState, divisor: u128) -> PoolState {
        use cb_core::types::PoolMath;
        if let PoolMath::Concentrated { liquidity, .. } = &mut p.math {
            *liquidity /= divisor;
        }
        p
    }

    fn at_slot(mut p: PoolState, slot: u64) -> PoolState {
        p.slot = slot;
        p
    }

    #[test]
    fn the_headline_edge_ignores_cycles_that_cannot_absorb_the_capital() {
        // A 20 bps gap across two venues that between them hold a couple of cents.
        // The rate is real and the opportunity is not: nobody can put $4.80 through it.
        // Ranked on rate alone this leads the board, which is exactly how a 1156 bps
        // "edge" that never filled once got onto the dashboard for hours.
        let m = market_with(vec![
            thinned(sol_usdc(1, 5_569_625_019_338_410_820, 100), 1_000_000_000),
            thinned(sol_usdc(2, 5_575_194_644_357_749_445, 200), 1_000_000_000),
        ]);
        let sweep = m.sweep(4.80, 4.80, 3);

        let best = sweep.best().expect("the rate is still measured and still reported");
        assert!(best.edge_bps > 0.0, "the marginal rate really is positive");
        assert!(
            best.depth_usd < 4.80,
            "the whole point of the fixture is that it is too thin, got ${}",
            best.depth_usd
        );
        assert!(
            sweep.tradeable().is_none(),
            "nothing here can take the capital, so there is no tradeable headline"
        );
        assert_eq!(sweep.clearing, 0, "a rate with no size behind it does not clear");
    }

    #[test]
    fn a_cycle_deep_enough_to_trade_becomes_the_headline() {
        let m = market_with(vec![
            sol_usdc(1, 5_569_625_019_338_410_820, 100),
            sol_usdc(2, 5_575_194_644_357_749_445, 200),
        ]);
        let sweep = m.sweep(4.80, 4.80, 3);

        let tradeable = sweep.tradeable().expect("a $24M pair can absorb $4.80");
        assert!(tradeable.depth_usd >= 4.80);
        assert!(tradeable.edge_bps > 0.0);
        assert_eq!(
            tradeable.route,
            sweep.best().unwrap().route,
            "when the leader is deep, both numbers name the same route"
        );
        assert!(sweep.clearing > 0);
    }

    #[test]
    fn depth_is_the_bottleneck_leg_not_the_one_we_enter_through() {
        // Enter through the deep pool, exit through the thin one. The reported depth
        // has to describe the exit, which is the leg that actually binds.
        let m = market_with(vec![
            sol_usdc(1, 5_569_625_019_338_410_820, 100),
            thinned(sol_usdc(2, 5_575_194_644_357_749_445, 200), 100_000),
        ]);
        let sweep = m.sweep(4.80, 4.80, 3);
        let best = sweep.best().expect("a route must be reported");
        let deep_alone = m.sweep(0.0, 0.0, 3);
        let unthinned_depth = deep_alone
            .rows
            .iter()
            .map(|r| r.depth_usd)
            .fold(0.0f64, f64::max);
        assert!(
            best.depth_usd < unthinned_depth,
            "the thin leg must pull the cycle's depth down, got ${} against ${}",
            best.depth_usd,
            unthinned_depth
        );
    }

    #[test]
    fn a_pool_that_has_gone_quiet_is_left_out_of_the_sweep_and_counted() {
        // One venue heard from recently, one silent for far longer than the guard
        // tolerates. A cycle needs both, so excluding the stale one leaves no cycle —
        // which is the correct outcome: no quote at all beats a quote against a price
        // that may have moved without us.
        let fresh = at_slot(sol_usdc(1, 5_569_625_019_338_410_820, 100), 100_000);
        let silent = at_slot(
            sol_usdc(2, 5_575_194_644_357_749_445, 200),
            100_000 - (MAX_STALE_LAG_SLOTS + 1),
        );
        let m = market_with(vec![fresh, silent]);
        let sweep = m.sweep(4.80, 4.80, 3);

        assert_eq!(sweep.stale_excluded, 1, "the lagging pool must be counted, not hidden");
        assert_eq!(sweep.evaluated, 0, "one venue alone makes no round trip");
        assert!(sweep.best().is_none());
    }

    #[test]
    fn a_market_that_is_merely_slow_is_not_treated_as_stale() {
        // Both pools equally old. Nothing lags anything, so nothing is excluded — the
        // guard measures relative lag, not absolute age, precisely so a quiet market
        // does not silently stop being measured.
        let m = market_with(vec![
            at_slot(sol_usdc(1, 5_569_625_019_338_410_820, 100), 5),
            at_slot(sol_usdc(2, 5_575_194_644_357_749_445, 200), 5),
        ]);
        let sweep = m.sweep(4.80, 4.80, 3);
        assert_eq!(sweep.stale_excluded, 0);
        assert!(sweep.evaluated > 0, "a slow market must still be swept");
    }
}
