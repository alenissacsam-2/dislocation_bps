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
    /// Smallest trade worth making, in USD, and so the depth a cycle must have before
    /// it counts as tradeable rather than as a quoted rate.
    ///
    /// # Why this is not just `capital_usd`
    ///
    /// A book does not have to deploy all of itself in one loop. Requiring a cycle to
    /// absorb the entire balance conflated "I cannot trade this" with "I cannot put
    /// everything into this", and it only looked correct while the balance was $5 —
    /// raise the book and every genuinely tradeable shallow cycle silently drops off
    /// the headline.
    ///
    /// What actually sets the floor is fixed cost. A trade has to clear the transaction
    /// that carries it, so the minimum viable size is roughly `tx cost ÷ edge`: on
    /// Solana at ~$0.001 a transaction and a few bps of edge, a few dollars; on an L2
    /// paying cents in gas, hundreds; on Ethereum L1, six figures. This one number is
    /// most of why the same strategy is worth running on one chain and not another.
    #[serde(default = "default_min_trade")]
    pub min_trade_usd: f64,
    /// Longest cycle to search. 3 finds triangles, which is where the real cycles
    /// live among the majors.
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,

    /// How far below each hop's quote its output floor is set, in basis points.
    ///
    /// # This must be smaller than the edge, divided by the hop count
    ///
    /// A route only builds if its last floor exceeds its first input, so for `n` hops
    /// with edge `e` the constraint is `(1 + e)(1 - s)^n > 1`, which for small numbers
    /// is **`s < e / n`**. A 3 bp edge over two hops leaves room for 1.5 bp per hop and
    /// no more.
    ///
    /// The first version of this was 30, on the reasoning that a wide floor is cautious
    /// and the route's own loss-check would stop anything pointless. That is backwards:
    /// a floor this wide makes the loss-check *unsatisfiable*, and a live dry run
    /// refused every single cycle at −25 to −28 bps guaranteed. Nothing can be traded
    /// with a tolerance larger than the profit being chased.
    ///
    /// The consequence of a tight floor is that an adverse tick reverts the transaction
    /// rather than filling it badly. That costs the fee, which is the correct price to
    /// pay when the alternative is a fill that loses more than the trade was worth.
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u32,

    /// Priority bid, in micro-lamports per compute unit. Zero pays the base fee only.
    #[serde(default)]
    pub priority_micro_lamports: u64,

    /// Simulate every trade and submit none.
    ///
    /// **Defaults to true, and that is not a placeholder.** `mode = "live"` arms the
    /// machinery: keys are loaded, routes are built, transactions are signed and run
    /// against live state. This is the switch that decides whether the last step
    /// happens. Turning it off is the moment real money can move, and it is separate
    /// from `mode` so that arming live execution and spending are two decisions rather
    /// than one.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,

    // --- risk limits -------------------------------------------------------
    //
    // These live in the same file the application's Risk Limits panel writes, and are
    // loaded here so the bot actually obeys them. It previously built
    // `cb_executor::risk::Limits::default()` and ignored the file entirely — so a
    // deliberately tiny `max_position_usd = 0.1` was silently a $25 cap, and a control
    // the operator had set to protect themselves did nothing. A safety limit that is
    // quietly inert is worse than an absent one, because it is believed.
    #[serde(default = "default_max_position_usd")]
    pub max_position_usd: f64,
    #[serde(default = "default_max_daily_loss_usd")]
    pub max_daily_loss_usd: f64,
    #[serde(default = "default_min_net_profit_usd")]
    pub min_net_profit_usd: f64,
    #[serde(default = "default_max_slippage_bps")]
    pub max_slippage_bps: f64,
    #[serde(default = "default_max_consecutive_failures")]
    pub max_consecutive_failures: u32,
    #[serde(default = "default_max_daily_trades")]
    pub max_daily_trades: u32,
}

fn default_capital() -> f64 {
    100.0
}
fn default_fee_buffer() -> f64 {
    0.20
}
/// Ten dollars. At Solana's ~$0.001 base fee plus a tip, a $10 trade at 3 bps nets
/// roughly three times what carrying it costs; below that the transaction eats the
/// edge. Deliberately a config value rather than a constant, because it is the number
/// that has to change first when this instrument is pointed at another chain.
fn default_min_trade() -> f64 {
    10.0
}
fn default_max_hops() -> usize {
    3
}
/// One basis point. See the field's own documentation: this is bounded above by the
/// edge divided by the hop count, and the edge here is single digits.
fn default_slippage_bps() -> u32 {
    1
}
/// True. The only safe default for a field whose false value spends money.
fn default_dry_run() -> bool {
    true
}

// Defaults matching `cb_executor::risk::Limits::default()`. Deliberately timid: the
// cost of them being too tight is a missed trade, the cost of them being too loose is
// a drained wallet.
fn default_max_position_usd() -> f64 {
    25.0
}
fn default_max_daily_loss_usd() -> f64 {
    5.0
}
fn default_min_net_profit_usd() -> f64 {
    0.01
}
fn default_max_slippage_bps() -> f64 {
    30.0
}
fn default_max_consecutive_failures() -> u32 {
    3
}
fn default_max_daily_trades() -> u32 {
    500
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

    /// Depth a cycle needs before it counts as tradeable, in USD.
    ///
    /// The minimum viable trade, never more than the book: a $5 account cannot use a
    /// $10 floor, and clamping here means the threshold degrades to "everything I have"
    /// on a small balance instead of silently reporting nothing as tradeable.
    #[must_use]
    pub fn tradeable_depth_usd(&self) -> f64 {
        self.min_trade_usd.min(self.tradable_usd())
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
            min_trade_usd: 10.0,
            max_hops: 3,
            slippage_bps: 30,
            priority_micro_lamports: 0,
            dry_run: true,
            max_position_usd: 25.0,
            max_daily_loss_usd: 5.0,
            min_net_profit_usd: 0.01,
            max_slippage_bps: 30.0,
            max_consecutive_failures: 3,
            max_daily_trades: 500,
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

    /// The bug this pair exists to prevent: a cycle deep enough to trade must not stop
    /// counting as tradeable merely because the book grew. Depth is a property of the
    /// pool, and what it has to clear is the smallest worthwhile trade, not the balance.
    #[test]
    fn raising_the_book_does_not_raise_the_depth_a_cycle_must_have() {
        let mut c = cfg(Mode::Paper);
        c.min_trade_usd = 10.0;
        c.capital_usd = 100.0;
        assert!((c.tradeable_depth_usd() - 10.0).abs() < 1e-9);
        c.capital_usd = 10_000.0;
        assert!(
            (c.tradeable_depth_usd() - 10.0).abs() < 1e-9,
            "a $19-deep cycle is still tradeable to a large account; it just cannot take all of it"
        );
    }

    #[test]
    fn a_book_smaller_than_the_minimum_trade_falls_back_to_the_book() {
        // Otherwise a $5 run with a $10 floor reports nothing as tradeable and looks
        // like a market with no depth, which is a statement about the config.
        let mut c = cfg(Mode::Paper);
        c.min_trade_usd = 10.0;
        c.capital_usd = 5.0;
        assert!((c.tradeable_depth_usd() - 4.80).abs() < 1e-9);
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

    /// The last thing between an armed live config and a real transaction.
    ///
    /// A config written without the key — by an older build, by hand, by a partial
    /// write — must read as dry. The failure direction of getting this wrong is
    /// spending money nobody asked to spend, so the default is not a convenience.
    #[test]
    fn a_config_that_does_not_mention_dry_run_is_a_dry_run() {
        let cfg: Config = toml::from_str(
            "mode = \"live\"
rpc_ws_url = \"wss://x\"
min_profit_lamports = 0
             max_position_lamports = 1
",
        )
        .expect("a config without the optional keys must still parse");
        assert!(cfg.dry_run, "a config with no dry_run key must not submit transactions");
        assert_eq!(cfg.priority_micro_lamports, 0);

        // The slippage default must satisfy `s < edge / hops`, or no route can ever
        // build. Asserted as the property rather than the number, because the number is
        // only correct while the edge is what it is — and 30 bps, the first value used
        // here, refused every cycle in a live dry run.
        // Two hops, which is the common cycle and the one the cheapest round trips use.
        // A three-hop cycle needs proportionally more edge to clear the same per-hop
        // floor, and at this default a 3 bp edge over three hops sits exactly on the
        // boundary — which is a fact about the strategy, not a misconfiguration.
        let plausible_edge_bps = 3.0;
        let hops = 2.0;
        assert!(
            f64::from(cfg.slippage_bps) < plausible_edge_bps / hops,
            "slippage of {} bps cannot be satisfied by a {plausible_edge_bps} bp edge over              {hops} hops — every route would refuse as a guaranteed loss",
            cfg.slippage_bps
        );

        // The risk limits must come from the file too, not from Default. The bot ignored
        // them entirely once, so a deliberately tiny position cap was silently $25.
        assert_eq!(cfg.max_position_usd, 25.0, "absent means the documented default");
        assert_eq!(cfg.min_net_profit_usd, 0.01);
    }

    /// The limits the application's panel writes must actually be read back.
    #[test]
    fn risk_limits_are_loaded_from_the_file_rather_than_defaulted() {
        let cfg: Config = toml::from_str(
            "rpc_ws_url = \"wss://x\"
min_profit_lamports = 0
max_position_lamports = 1
             max_position_usd = 0.1
max_daily_trades = 7
min_net_profit_usd = 0.002
",
        )
        .expect("parses");
        assert_eq!(cfg.max_position_usd, 0.1, "a 0.1 cap must not silently become 25");
        assert_eq!(cfg.max_daily_trades, 7);
        assert_eq!(cfg.min_net_profit_usd, 0.002);
    }

    /// And the value is honoured when it *is* written, in both directions — a default
    /// that ignored the file would be worse than no default.
    #[test]
    fn an_explicit_dry_run_setting_is_honoured_both_ways() {
        let live: Config = toml::from_str(
            "rpc_ws_url = \"wss://x\"
min_profit_lamports = 0
max_position_lamports = 1
             dry_run = false
",
        )
        .unwrap();
        assert!(!live.dry_run);

        let dry: Config = toml::from_str(
            "rpc_ws_url = \"wss://x\"
min_profit_lamports = 0
max_position_lamports = 1
             dry_run = true
",
        )
        .unwrap();
        assert!(dry.dry_run);
    }

    /// The file a fresh install is seeded from must load.
    ///
    /// `cb_desk::paths::ensure_ready` writes `config.example.toml` into the data
    /// directory on first launch, so a key that does not parse — or a comment block
    /// that accidentally swallows one — breaks every new installation and nothing in
    /// the workspace would otherwise notice. Embedded rather than read at run time so
    /// the check is against the file that ships.
    #[test]
    fn the_example_config_shipped_to_new_installs_actually_loads() {
        const EXAMPLE: &str = include_str!("../../../config.example.toml");
        let cfg: Config = toml::from_str(EXAMPLE).expect("config.example.toml must parse");

        // And the defaults it ships with are the safe ones. A seeded config that armed
        // anything would arm it on a machine whose owner has not opened the app yet.
        assert_eq!(cfg.mode, Mode::Paper, "a seeded config must be paper");
        assert!(cfg.dry_run, "a seeded config must be a dry run");
        assert!(!cfg.is_live_enabled_with(Some("1")), "paper must ignore the env switch");
    }
}
