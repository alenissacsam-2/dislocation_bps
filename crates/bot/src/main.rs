//! cryptobot — Solana arbitrage research system.
//!
//! Paper mode is the default. Live *trading* requires two independent switches:
//! `mode = "live"` in the config **and** `CRYPTOBOT_ALLOW_LIVE=1` in the environment.
//! Live *data* is a separate, read-only setting and is on by default.
//!
//! See `docs/superpowers/specs/` for the design and `docs/research/` for the numbers.

mod live;
mod sim;

use cb_core::config::{Config, FeedSource, Mode};
use cb_feed::WsFeed;
use cb_server::{routes, Event, EventBus};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SLOT_MS: u64 = 200;
const LISTEN: &str = "0.0.0.0:8787";

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
    tracing::info!("feed: LIVE mainnet via {}", cfg.rpc_ws_url);

    let mut market = live::LiveMarket::bootstrap(&cfg.rpc_http_url).await?;
    for (addr, label) in market.labels() {
        tracing::info!("  watching {label:<10} {addr}");
    }

    let feed = WsFeed::new(cfg.rpc_ws_url.clone());
    let stats = std::sync::Arc::clone(&feed.stats);
    let mut rx = feed.spawn(market.subscriptions.clone());

    // Shared so the status heartbeat can report what the scanner is seeing.
    let best_edge: std::sync::Arc<std::sync::Mutex<Option<live::BestEdge>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let total_cycles = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cycles_for_status = std::sync::Arc::clone(&total_cycles);
    let edge_for_status = std::sync::Arc::clone(&best_edge);

    // Status heartbeat. Reports feed health honestly — dropped updates are
    // opportunities we could not even evaluate, so they belong on the dashboard.
    let status_bus = bus.clone();
    let status_stats = std::sync::Arc::clone(&stats);
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(2));
        loop {
            t.tick().await;
            let (updates, reconnects, dropped, last_slot, _) = status_stats.snapshot();
            let best = edge_for_status.lock().ok().and_then(|g| g.clone());
            let cycles = cycles_for_status.load(Ordering::Relaxed);
            let last_ms = status_stats.last_update_ms.load(Ordering::Relaxed);
            let stale_for = if last_ms == 0 { u64::MAX } else { now_ms().saturating_sub(last_ms) };
            status_bus.publish(Event::Status {
                mode: "paper · live mainnet".into(),
                // Consider the feed live only if something arrived in the last 30s.
                connected: stale_for < 30_000,
                slot: last_slot,
                slot_lag: 0,
                pools_tracked: 0,
                sol_price_usd: 0.0,
                uptime_secs: started.elapsed().as_secs(),
                updates,
                dropped,
                reconnects,
                best_edge_bps: best.as_ref().map_or(0.0, |b| b.edge_bps),
                best_route: best.as_ref().map_or_else(String::new, |b| b.route.clone()),
                best_hops: best.as_ref().map_or(0, |b| b.hops),
                best_fee_bps: best.as_ref().map_or(0.0, |b| b.fee_bps),
                cycles_evaluated: cycles,
            });
        }
    });

    let tradable_usd = cfg.tradable_usd();
    let max_hops = cfg.max_hops;
    tracing::info!("capital: ${:.2} tradable, cycles up to {max_hops} hops", tradable_usd);

    tokio::spawn(async move {
        let mut next_id: u64 = 1;
        let mut cycles_seen: u64 = 0;
        while let Some(update) = rx.recv().await {
            let Some(changed) = market.apply(&update, &bus) else {
                continue;
            };

            // Record how close the market came, even when nothing clears. Silence is
            // not a measurement; a number is.
            if let Some(be) = market.best_edge(&changed, max_hops) {
                cycles_seen += be.evaluated;
                total_cycles.store(cycles_seen, Ordering::Relaxed);
                if let Ok(mut g) = best_edge.lock() {
                    // Report the *current* best, not an all-time high-water mark:
                    // a stale record from ten minutes ago describes nothing useful.
                    *g = Some(be);
                }
            }

            for opp in market.evaluate(&changed, tradable_usd, max_hops) {
                let id = next_id;
                next_id += 1;

                // Uncontested cycles pay the tip floor; contested ones are bid up to
                // most of the profit. A cycle worth more than a cent on a major pair
                // will have been seen by faster searchers too.
                let contested = opp.gross_profit_usd > 0.01;
                let sol_price = market.sol_price_usd().unwrap_or(0.0);
                let est_tip_usd = if contested {
                    opp.gross_profit_usd * 0.60
                } else {
                    0.0000075 * sol_price // median Jito tip floor
                };
                let base_fee_usd = 0.000005 * sol_price;
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
                    pair: format!("{} [{}]", opp.route, opp.venues),
                    dex_buy: format!("{} hops", opp.hops),
                    dex_sell: "Raydium v4".into(),
                    // Report the capital cost directly: how much of the available
                    // profit our $5 can actually reach.
                    spread_bps: if opp.optimal_size_usd > 0.0 {
                        100.0 * opp.size_usd / opp.optimal_size_usd
                    } else {
                        0.0
                    },
                    optimal_size_usd: opp.optimal_size_usd,
                    capped_size_usd: opp.size_usd,
                    gross_profit_usd: opp.gross_profit_usd,
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
        tracing::error!("feed channel closed — no further updates");
    });

    Ok(())
}
