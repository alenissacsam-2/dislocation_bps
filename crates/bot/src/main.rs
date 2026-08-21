//! cryptobot — Solana arbitrage research system.
//!
//! Paper mode is the default. Live *trading* requires two independent switches:
//! `mode = "live"` in the config **and** `CRYPTOBOT_ALLOW_LIVE=1` in the environment.
//! Live *data* is a separate, read-only setting and is on by default.
//!
//! See `docs/superpowers/specs/` for the design and `docs/research/` for the numbers.

mod live;
mod registry;
mod sim;

use cb_core::config::{Config, FeedSource, Mode};
use cb_feed::WsFeed;
use cb_server::{routes, Event, EventBus, RouteRow};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SLOT_MS: u64 = 200;
const LISTEN: &str = "0.0.0.0:8787";

/// How often the whole cycle graph is re-priced. Two sweeps per slot: fast enough
/// that a measurement is never more than half a block stale, slow enough that the
/// scan cost stays invisible.
const SWEEP_INTERVAL: Duration = Duration::from_millis(200);

/// How often token valuations are rebuilt from pool state. Only used for sizing, and
/// a token's dollar value does not move meaningfully inside ten seconds.
const USD_REFRESH: Duration = Duration::from_secs(10);

/// Profit above which we assume a faster searcher has already seen the same cycle.
const CONTESTED_USD: f64 = 0.01;
/// Share of profit a contested cycle has to give up as a tip to win the bundle.
const CONTESTED_TIP_SHARE: f64 = 0.60;
/// Median Jito tip floor, in SOL.
const JITO_TIP_FLOOR_SOL: f64 = 0.000_007_5;
/// Solana base transaction fee, in SOL.
const BASE_FEE_SOL: f64 = 0.000_005;

/// What the last sweep saw, handed to the status heartbeat.
#[derive(Debug, Clone, Default)]
struct SweepSummary {
    best: Option<live::EdgeRow>,
    evaluated_total: u64,
    sweep_us: u64,
    pools_ready: usize,
    venues: usize,
    sol_price_usd: f64,
    slot: u64,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cb_server=info,cb_feed=info".into()),
        )
        .init();

    let cfg = Config::load("config.toml").unwrap_or_else(|_| {
        tracing::warn!("no config.toml found — using defaults (paper mode, live data)");
        Config {
            mode: Mode::Paper,
            feed: FeedSource::Live,
            rpc_http_url: "https://api.mainnet-beta.solana.com".into(),
            rpc_ws_url: "wss://api.mainnet-beta.solana.com".into(),
            min_profit_lamports: 0,
            max_position_lamports: 20_000_000,
            capital_usd: 5.0,
            fee_buffer_usd: 0.20,
            max_hops: 3,
        }
    });

    // The guard. Both switches, or nothing happens.
    if cfg.is_live_enabled() {
        anyhow::bail!(
            "live execution is not implemented yet; refusing to start in live mode. \
             Set mode = \"paper\" in config.toml."
        );
    }
    if matches!(cfg.mode, Mode::Live) {
        tracing::warn!(
            "config requests live mode but {} is not set — staying in paper mode",
            cb_core::config::LIVE_ENV_VAR
        );
    }

    let bus = EventBus::new();
    let addr: SocketAddr = LISTEN.parse()?;

    match cfg.feed {
        FeedSource::Simulated => spawn_simulated(bus.clone()),
        FeedSource::Live => spawn_live(bus.clone(), &cfg).await?,
    }

    tracing::info!("mode: PAPER — no transaction will be signed or sent");
    tracing::info!("dashboard: http://127.0.0.1:8787");

    routes::serve(addr, routes::state(bus, "paper"), "dashboard/dist").await
}

fn spawn_simulated(bus: EventBus) {
    tracing::warn!("feed: SIMULATED — synthetic reserves, real pricing maths");
    tokio::spawn(async move {
        let mut market = sim::Market::new(0xC0FFEE);
        let mut ticker = tokio::time::interval(Duration::from_millis(SLOT_MS));
        let mut n: u32 = 0;
        loop {
            ticker.tick().await;
            market.tick(&bus);
            n = n.wrapping_add(1);
            if n % 10 == 0 {
                market.status(&bus, true);
            }
        }
    });
}

async fn spawn_live(bus: EventBus, cfg: &Config) -> anyhow::Result<()> {
    let registry = registry::Registry::embedded()?;

    // Report the universe before any data arrives, so the run's headline constraint
    // is on the record even if the feed never connects.
    let dupe_count = registry.duplicate_pairs().len();
    let round_trips: Vec<(String, f64)> = registry.cheapest_round_trips();
    let cheapest_bps = round_trips.first().map_or(f64::INFINITY, |(_, b)| *b);
    tracing::info!(
        "universe: {} pools, {} mints, ~{} subscriptions",
        registry.pools.len(),
        registry.mints.len(),
        registry.subscription_estimate()
    );
    tracing::info!("{dupe_count} pairs are quoted by more than one venue — direct round trips");
    for (pair, bps) in round_trips.iter().take(5) {
        tracing::info!("  cheapest round trip  {pair:<16} {bps:>6.2} bps of fees");
    }
    for p in registry.pools.iter().take(4) {
        tracing::info!(
            "  deepest cheap pool   {:<16} {:>7} {:<8} ${:>12.0} tvl",
            p.label,
            p.dex.tag(),
            live::fee_label(p.fee_ppm_hint),
            p.tvl_usd
        );
    }
    tracing::info!("feed: LIVE mainnet via {}", cfg.rpc_ws_url);

    let mut market = live::LiveMarket::bootstrap(&cfg.rpc_http_url, registry).await?;
    tracing::info!(
        "watching {} pools across {} venues; {} priceable at start",
        market.pool_count(),
        market.venue_count(),
        market.ready_count()
    );

    let feed = WsFeed::new(cfg.rpc_ws_url.clone());
    let stats = std::sync::Arc::clone(&feed.stats);
    let mut rx = feed.spawn(market.subscriptions.clone());

    // What the scanner last saw, for the status heartbeat to report. A plain mutex is
    // fine: nothing holds it across an await.
    let shared = std::sync::Arc::new(std::sync::Mutex::new(SweepSummary::default()));
    let shared_for_status = std::sync::Arc::clone(&shared);

    let status_bus = bus.clone();
    let status_stats = std::sync::Arc::clone(&stats);
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(2));
        loop {
            t.tick().await;
            let (updates, reconnects, dropped, last_slot, _) = status_stats.snapshot();
            let last_ms = status_stats.last_update_ms.load(Ordering::Relaxed);
            let stale_for = if last_ms == 0 { u64::MAX } else { now_ms().saturating_sub(last_ms) };
            let s = shared_for_status.lock().map(|g| g.clone()).unwrap_or_default();
            let best = s.best.as_ref();

            status_bus.publish(Event::Status {
                mode: "paper · live mainnet".into(),
                // Consider the feed live only if something arrived in the last 30s.
                connected: stale_for < 30_000,
                slot: last_slot.max(s.slot),
                slot_lag: 0,
                pools_tracked: s.pools_ready,
                sol_price_usd: s.sol_price_usd,
                uptime_secs: started.elapsed().as_secs(),
                updates,
                dropped,
                reconnects,
                stalls: status_stats.stalls.load(Ordering::Relaxed),
                data_age_secs: if stale_for == u64::MAX { 0 } else { stale_for / 1000 },
                best_edge_bps: best.map_or(0.0, |b| b.edge_bps),
                best_route: best.map_or_else(String::new, |b| b.route.clone()),
                best_hops: best.map_or(0, |b| b.hops),
                best_fee_bps: best.map_or(0.0, |b| b.fee_bps),
                cycles_evaluated: s.evaluated_total,
                venues: s.venues,
                duplicate_pairs: dupe_count,
                cheapest_round_trip_bps: if cheapest_bps.is_finite() { cheapest_bps } else { 0.0 },
                sweep_us: s.sweep_us,
            });
        }
    });

    let tradable_usd = cfg.tradable_usd();
    let max_hops = cfg.max_hops;
    tracing::info!("capital: ${tradable_usd:.2} tradable, cycles up to {max_hops} hops");

    tokio::spawn(async move {
        let mut next_id: u64 = 1;
        let mut evaluated_total: u64 = 0;
        // The sweep is a full pass over the cycle graph, not a reaction to one pool.
        // At this graph size it costs well under a millisecond, so running it on a
        // timer is cheaper than reasoning about which updates could matter — and it
        // has no blind spot for pools in the middle of a triangle.
        let mut sweep_timer = tokio::time::interval(SWEEP_INTERVAL);
        // Token valuations move slowly and are only used for sizing; rebuilding the
        // index every sweep would be work spent on a number that has not changed.
        let mut usd_timer = tokio::time::interval(USD_REFRESH);

        loop {
            tokio::select! {
                received = rx.recv() => {
                    match received {
                        Some(update) => { market.apply(&update, &bus); }
                        None => {
                            tracing::error!("feed channel closed — no further updates");
                            break;
                        }
                    }
                }
                _ = usd_timer.tick() => market.rebuild_usd_index(),
                _ = sweep_timer.tick() => {
                    let sweep = market.sweep(tradable_usd, max_hops);
                    evaluated_total = evaluated_total.saturating_add(sweep.evaluated);

                    if let Ok(mut g) = shared.lock() {
                        *g = SweepSummary {
                            // Report the *current* best, not an all-time high-water
                            // mark: a record from ten minutes ago describes nothing.
                            best: sweep.best().cloned(),
                            evaluated_total,
                            sweep_us: sweep.duration_us,
                            pools_ready: market.ready_count(),
                            venues: market.venue_count(),
                            sol_price_usd: market.sol_price_usd().unwrap_or(0.0),
                            slot: sweep.slot,
                        };
                    }

                    if !sweep.rows.is_empty() {
                        bus.publish(Event::Routes {
                            rows: sweep.rows.iter().map(RouteRow::from).collect(),
                            evaluated: sweep.evaluated,
                            sweep_us: sweep.duration_us,
                            slot: sweep.slot,
                            ts_ms: now_ms(),
                        });
                    }

                    let sol_price = market.sol_price_usd().unwrap_or(0.0);
                    for opp in sweep.opportunities {
                        let id = next_id;
                        next_id += 1;

                        // Uncontested cycles pay the tip floor; contested ones get bid
                        // up to most of the profit. A cycle worth more than a cent on
                        // a major pair will have been seen by faster searchers too.
                        let contested = opp.gross_profit_usd > CONTESTED_USD;
                        let est_tip_usd = if contested {
                            opp.gross_profit_usd * CONTESTED_TIP_SHARE
                        } else {
                            JITO_TIP_FLOOR_SOL * sol_price
                        };
                        let base_fee_usd = BASE_FEE_SOL * sol_price;
                        let net = opp.gross_profit_usd - est_tip_usd - base_fee_usd;

                        let skipped = if net <= 0.0 {
                            Some("net negative after tip".to_string())
                        } else if contested {
                            Some("contested — would lose the race".to_string())
                        } else {
                            None
                        };

                        bus.publish(Event::Opportunity {
                            id,
                            route: opp.route.clone(),
                            venues: opp.venues.clone(),
                            hops: opp.hops,
                            edge_bps: opp.edge_bps,
                            dislocation_bps: opp.edge_bps + opp.fee_bps,
                            fee_bps: opp.fee_bps,
                            optimal_size_usd: opp.optimal_size_usd,
                            capped_size_usd: opp.size_usd,
                            capital_reach_pct: if opp.optimal_size_usd > 0.0 {
                                100.0 * opp.size_usd / opp.optimal_size_usd
                            } else {
                                100.0
                            },
                            gross_profit_usd: opp.gross_profit_usd,
                            profit_at_optimal_usd: opp.profit_at_optimal_usd,
                            est_tip_usd,
                            net_profit_usd: net,
                            contested,
                            skipped_reason: skipped.clone(),
                            slot: opp.slot,
                            ts_ms: now_ms(),
                        });

                        if skipped.is_none() {
                            // Paper mode: record what would have happened, sign nothing.
                            bus.publish(Event::Execution {
                                id,
                                opportunity_id: id,
                                paper: true,
                                landed: true,
                                realised_usd: net,
                                tip_paid_usd: est_tip_usd,
                                latency_ms: 0,
                                signature: None,
                                reason: Some("paper — uncontested, assumed landed".into()),
                                ts_ms: now_ms(),
                            });
                        }
                    }
                }
            }
        }
    });

    Ok(())
}
