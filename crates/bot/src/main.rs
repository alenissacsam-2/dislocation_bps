//! cryptobot — Solana arbitrage research system.
//!
//! Paper mode is the default and live trading requires two independent switches.
//! See `docs/superpowers/specs/` for the design.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("cryptobot v{} — scaffold", env!("CARGO_PKG_VERSION"));
    Ok(())
}
