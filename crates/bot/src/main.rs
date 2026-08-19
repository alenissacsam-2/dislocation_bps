//! cryptobot — Solana arbitrage research system.
//!
//! Paper mode is the default. Live trading requires two independent switches:
//! `mode = "live"` in the config **and** `CRYPTOBOT_ALLOW_LIVE=1` in the environment.
//!
//! See `docs/superpowers/specs/` for the design and `docs/research/` for the numbers
//! that motivated it.

mod sim;

use cb_core::config::{Config, Mode};
use cb_server::{routes, EventBus};
use std::net::SocketAddr;
use std::time::Duration;

/// Slot cadence. Solana is mid-rollout from 400ms to 200ms slots (SIMD-0525), so this
/// is a floor we can tighten as the network does.
const SLOT_MS: u64 = 200;
const STATUS_EVERY: u32 = 10;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cb_server=info".into()),
        )
        .init();

    let cfg = Config::load("config.toml").unwrap_or_else(|_| {
        tracing::warn!("no config.toml found — using paper defaults");
        Config {
            mode: Mode::Paper,
            rpc_ws_url: "wss://api.mainnet-beta.solana.com".into(),
            min_profit_lamports: 1_000_000,
            max_position_lamports: 20_000_000,
        }
    });

    // The guard. Both switches, or nothing happens.
    if cfg.is_live_enabled() {
        tracing::error!("LIVE MODE IS ARMED — real funds are at risk");
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
    let addr: SocketAddr = "127.0.0.1:8787".parse()?;

    // The market loop. Note it holds no handle to the server and never awaits it:
    // the dashboard is a lossy observer and cannot stall this task.
    let sim_bus = bus.clone();
    tokio::spawn(async move {
        let mut market = sim::Market::new(0xC0FFEE);
        let mut ticker = tokio::time::interval(Duration::from_millis(SLOT_MS));
        let mut n: u32 = 0;
        loop {
            ticker.tick().await;
            market.tick(&sim_bus);
            n = n.wrapping_add(1);
            if n % STATUS_EVERY == 0 {
                market.status(&sim_bus, true);
            }
        }
    });

    tracing::info!("mode: PAPER — no transaction will be signed or sent");
    tracing::info!("open http://{addr}");

    routes::serve(addr, routes::state(bus, "paper"), "dashboard/dist").await
}
