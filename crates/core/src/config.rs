//! Configuration, and the two-switch guard that gates live trading.
//!
//! Live trading requires BOTH `mode = "live"` in the config file AND the environment
//! variable `CRYPTOBOT_ALLOW_LIVE=1`. Neither alone is sufficient. This is deliberate:
//! no single accidental edit, merge, or stray config file can start spending money.

use serde::{Deserialize, Serialize};

/// Environment variable that forms the second half of the live-trading guard.
pub const LIVE_ENV_VAR: &str = "CRYPTOBOT_ALLOW_LIVE";

fn default_rpc_http() -> String {
    "https://api.mainnet-beta.solana.com".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Detect and record only. Never signs or sends. The default.
    #[default]
    Paper,
    /// Sign and submit real transactions. Requires the env switch as well.
    Live,
}

/// Where pool state comes from. Orthogonal to [`Mode`]: reading live mainnet data is
/// read-only and safe, and says nothing about whether we would sign anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FeedSource {
    /// Real mainnet accounts over WebSocket. The default — a research instrument
    /// pointed at synthetic data measures nothing.
    #[default]
    Live,
    /// Synthetic market, for exercising the pipeline offline.
    Simulated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub feed: FeedSource,
    #[serde(default = "default_rpc_http")]
    pub rpc_http_url: String,
    pub rpc_ws_url: String,
    /// Minimum gross profit, in lamports, for an opportunity to be recorded as actionable.
    pub min_profit_lamports: u64,
    /// Hard ceiling on trade size, independent of what the optimiser suggests.
    pub max_position_lamports: u64,
    /// Total working capital in USD. The binding constraint on every trade size.
    #[serde(default = "default_capital")]
    pub capital_usd: f64,
    /// Held back from `capital_usd` for fees and account rent; not tradable.
    #[serde(default = "default_fee_buffer")]
    pub fee_buffer_usd: f64,
    /// Longest cycle to search. 3 finds triangles, which is where the real cycles
    /// live among the majors.
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,
}

fn default_capital() -> f64 {
    5.0
}
fn default_fee_buffer() -> f64 {
    0.20
}
fn default_max_hops() -> usize {
    3
}

impl Config {
    /// Load from a TOML file, with `CRYPTOBOT_` prefixed environment overrides.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        use figment::{
            providers::{Env, Format, Toml},
            Figment,
        };
        Ok(Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("CRYPTOBOT_"))
            .extract()?)
    }

    /// Capital actually available to trade, after the fee/rent buffer.
    #[must_use]
    pub fn tradable_usd(&self) -> f64 {
        (self.capital_usd - self.fee_buffer_usd).max(0.0)
    }

    /// True only if BOTH switches are set. Reads the real environment.
    #[must_use]
    pub fn is_live_enabled(&self) -> bool {
        self.is_live_enabled_with(std::env::var(LIVE_ENV_VAR).ok().as_deref())
    }

    /// Testable core of the guard. `env` is the value of [`LIVE_ENV_VAR`], if set.
    ///
    /// Only the exact string `1` counts — not `true`, not `yes`. Narrow by design, so
    /// that a vaguely-truthy value left in a shell profile cannot arm live trading.
    #[must_use]
    pub fn is_live_enabled_with(&self, env: Option<&str>) -> bool {
        matches!(self.mode, Mode::Live) && env == Some("1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: Mode) -> Config {
        Config {
            mode,
            feed: FeedSource::Simulated,
            rpc_http_url: "https://x".into(),
            rpc_ws_url: "wss://x".into(),
            min_profit_lamports: 1000,
            max_position_lamports: 10_000_000,
            capital_usd: 5.0,
            fee_buffer_usd: 0.20,
            max_hops: 3,
        }
    }

    #[test]
    fn paper_mode_is_never_live_regardless_of_env() {
        let c = cfg(Mode::Paper);
        assert!(!c.is_live_enabled_with(Some("1")), "paper config must ignore the env switch");
        assert!(!c.is_live_enabled_with(None));
    }

    #[test]
    fn live_mode_requires_the_env_switch_too() {
        let c = cfg(Mode::Live);
        assert!(!c.is_live_enabled_with(None), "config alone must not enable live");
        assert!(!c.is_live_enabled_with(Some("0")));
        assert!(!c.is_live_enabled_with(Some("true")), "only the exact string 1 counts");
        assert!(c.is_live_enabled_with(Some("1")), "both switches set must enable live");
    }

    #[test]
    fn default_mode_is_paper() {
        assert_eq!(Mode::default(), Mode::Paper);
    }

    #[test]
    fn tradable_capital_excludes_the_fee_buffer() {
        let mut c = cfg(Mode::Paper);
        c.capital_usd = 5.0;
        c.fee_buffer_usd = 0.20;
        assert!((c.tradable_usd() - 4.80).abs() < 1e-9);
    }

    #[test]
    fn tradable_capital_never_goes_negative() {
        // A buffer larger than the balance means nothing to trade, not a negative size.
        let mut c = cfg(Mode::Paper);
        c.capital_usd = 0.10;
        c.fee_buffer_usd = 0.20;
        assert_eq!(c.tradable_usd(), 0.0);
    }

    #[test]
    fn default_feed_is_live_because_synthetic_data_measures_nothing() {
        assert_eq!(FeedSource::default(), FeedSource::Live);
    }

    #[test]
    fn feed_source_is_independent_of_live_trading() {
        // Reading mainnet is read-only. A live feed must never imply live execution.
        let mut c = cfg(Mode::Paper);
        c.feed = FeedSource::Live;
        assert!(!c.is_live_enabled_with(Some("1")));
    }
}
