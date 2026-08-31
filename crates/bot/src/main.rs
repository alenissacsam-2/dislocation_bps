//! cryptobot — Solana arbitrage research system.
//!
//! Paper mode is the default. Live *trading* requires two independent switches:
//! `mode = "live"` in the config **and** `CRYPTOBOT_ALLOW_LIVE=1` in the environment.
//! Live *data* is a separate, read-only setting and is on by default.
//!
//! See `docs/superpowers/specs/` for the design and `docs/research/` for the numbers.

mod execute;
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
/// Loopback only.
///
/// This was `0.0.0.0` while the bot lived in WSL, because the browser on the Windows
/// side had to cross the VM boundary to reach it. Nothing crosses a boundary any more:
/// the app and the bot are both Windows processes on one machine. Binding every
/// interface now would put the run's state on the local network for no gain, and it is
/// what made Windows Firewall prompt on first launch.
const LISTEN: &str = "127.0.0.1:8787";

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

/// Where the measurement goes. The dashboard is a window; this is the record.
const LEDGER_PATH: &str = "cryptobot.db";
/// The encrypted key, beside the ledger and the config. Never read without a
/// passphrase, and never written by this binary.
const WALLET_FILE: &str = "keypair-encrypted.json";

/// What this run should call itself, everywhere it says so.
///
/// Derived rather than written out at each site. Every mode string in this binary used
/// to be the literal `"paper"` — in the startup log, in `/api/health`, in the status the
/// window's footer renders, and on every execution event. That was true only because
/// nothing could produce any other mode. Once something can, a hardcoded label is a run
/// that cannot announce what it is doing, and the operator's only indicator agrees with
/// them no matter what is actually happening.
fn mode_label(cfg: &Config) -> &'static str {
    match cfg.mode {
        Mode::Paper => "paper",
        Mode::Live => "live",
    }
}

/// One sweep in this many is written to the ledger. Sweeps run at 5 Hz and the
/// market does not change meaningfully between two of them, so sampling at 1 Hz
/// keeps a day of running to ~86k rows while losing nothing a mean or a histogram
/// would notice.
const LEDGER_EVERY_N_SWEEPS: u32 = 5;

/// How often every watched account is re-read over HTTP and folded back in.
///
/// For an AMM, no update means no change — so a silently dropped subscription is
/// indistinguishable from a quiet pool by looking at the stream alone. Re-reading the
/// whole set settles it, and the count of pools that came back different is a direct
/// measurement of how much the WebSocket is missing.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(180);

/// Detections of one loop further apart than this many slots are separate
/// opportunities. Slots are the chain's own clock (~400 ms) and are what the ledger
/// records, so this needs no wall-clock and cannot drift.
///
/// The sweep re-detects a standing gap five times a second, so consecutive detections
/// of a live opportunity are about 0.2s apart. Two seconds is comfortably above that
/// and comfortably below the time it takes for a closed gap to reopen.
const EPISODE_GAP_SLOTS: u64 = 5;

/// How long the feed may go silent before the measurement stops recording.
///
/// The staleness guard in `sweep()` compares each pool against the newest slot we
/// hold, which cannot detect the feed dying altogether: when nothing arrives, every
/// pool ages together and none of them ever looks stale. Only a wall clock catches
/// that, and this is it.
///
/// Sweeps keep running so the dashboard stays honest about what it is showing, but
/// nothing is written to the ledger. A measurement that knows its clock has stopped
/// does not go on writing numbers.
const FEED_STALL_SECS: u64 = 5;

/// What the last sweep saw, handed to the status heartbeat.
#[derive(Debug, Clone, Default)]
struct SweepSummary {
    /// Highest marginal rate seen, tradeable or not. A diagnostic.
    best: Option<live::EdgeRow>,
    /// Highest rate with the capital's worth of depth behind it. The headline.
    tradeable: Option<live::EdgeRow>,
    /// Capital a cycle had to be able to absorb to qualify, in USD.
    tradeable_min_usd: f64,
    /// Pools the last sweep dropped for lagging too far behind.
    stale_excluded: usize,
    /// Whether the feed has gone quiet long enough that recording is paused.
    feed_stalled: bool,
    evaluated_total: u64,
    sweep_us: u64,
    pools_ready: usize,
    venues: usize,
    sol_price_usd: f64,
    slot: u64,
    reconcile_drift: usize,
    reconcile_checked: usize,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}


/// Load the key and build the executor. Only ever called with both switches set.
///
/// # The passphrase does not come from the config, the environment, or a file
///
/// It is read from **stdin**, once, at startup. That is not ceremony. A config value
/// would put it in a file the application writes and the repository could swallow; an
/// environment variable is visible to anything that can read this process's environment
/// and is inherited by every child; a file beside the key defeats encrypting the key.
/// Stdin is the one channel that is closed after start-up and never appears in a
/// process listing.
///
/// The consequence is deliberate and worth stating: **`cryptobot-desk` must feed the
/// passphrase to this process, and a `cb-bot` started by hand in live mode will block
/// waiting for one.** A live config on its own cannot trade.
///
/// # Errors
/// If no passphrase arrives, the key is missing, or the passphrase is wrong.
async fn arm_live(cfg: &Config) -> anyhow::Result<execute::Trader> {
    use std::io::BufRead;

    let key_path = std::path::Path::new(WALLET_FILE);
    if !key_path.exists() {
        anyhow::bail!(
            "mode = \"live\" but there is no key at {WALLET_FILE}. Import one in \
             cryptobot-desk under Parameters -> Wallet."
        );
    }

    tracing::info!("live mode: waiting for the wallet passphrase on stdin");
    let mut passphrase = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut passphrase)
        .map_err(|e| anyhow::anyhow!("could not read the passphrase from stdin: {e}"))?;
    let passphrase = passphrase.trim_end_matches(['\r', '\n']).to_string();
    if passphrase.is_empty() {
        anyhow::bail!(
            "no passphrase arrived on stdin. cryptobot-desk supplies it when it starts \
             the bot in live mode; started by hand, pipe it in."
        );
    }

    let sealed = cb_wallet::EncryptedKey::load(key_path)?;
    let wallet = sealed.unseal(&passphrase)?;
    // Dropped here rather than left on the stack for the rest of start-up.
    drop(passphrase);

    let address = wallet.pubkey();
    let endpoints = cfg.http_endpoints();
    if endpoints.len() > 1 {
        tracing::info!(
            "{} RPC endpoints configured; reads fail over between them, sends never do",
            endpoints.len()
        );
    }
    let rpc = cb_executor::rpc::Rpc::with_fallbacks(endpoints)?;
    // From the file, not from Default. The application's Risk Limits panel writes these
    // and the operator expects them to bind.
    let limits = cb_executor::risk::Limits {
        max_position_usd: cfg.max_position_usd,
        max_daily_loss_usd: cfg.max_daily_loss_usd,
        min_net_profit_usd: cfg.min_net_profit_usd,
        max_slippage_bps: cfg.max_slippage_bps,
        max_consecutive_failures: cfg.max_consecutive_failures,
        max_daily_trades: cfg.max_daily_trades,
    };
    limits.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::info!(
        "risk limits from config.toml: max position ${:.2}, min net ${:.4}, daily loss ${:.2}, {} consecutive failures, {} trades/day",
        limits.max_position_usd,
        limits.min_net_profit_usd,
        limits.max_daily_loss_usd,
        limits.max_consecutive_failures,
        limits.max_daily_trades
    );
    let opts = execute::TradeOptions {
        slippage_bps: cfg.slippage_bps,
        priority_micro_lamports: cfg.priority_micro_lamports,
        compute_units: 400_000,
        dry_run: cfg.dry_run,
        create_token_accounts: true,
        wsol: cb_executor::route::WsolPolicy::WrapAndClose,
    };
    let exec = cb_executor::Executor::new(wallet, rpc, limits, cfg.dry_run)?;

    if cfg.dry_run {
        tracing::warn!(
            "LIVE ARMED, DRY RUN: {address} will build and simulate real transactions and \
             submit none. Set dry_run = false in config.toml to spend."
        );
    } else {
        tracing::error!(
            "LIVE ARMED, SUBMITTING: {address} will sign and send real transactions. \
             Every trade is still simulated first and abandoned unless the simulated \
             balance clears the profit floor."
        );
    }
    // A cycle longer than this cannot be executed atomically, so searching for them in
    // live mode finds opportunities that can only be refused. Worth saying out loud:
    // the leaderboard will show cycles the executor will decline, and that is the packet
    // limit rather than a defect.
    if cfg.max_hops > execute::MAX_EXECUTABLE_HOPS {
        tracing::warn!(
            "max_hops = {} but only {} hops fit in one transaction — longer cycles will \
             be found, priced, and refused at build time",
            cfg.max_hops,
            execute::MAX_EXECUTABLE_HOPS
        );
    }

    let trader = execute::Trader::new(exec, opts);
    tracing::info!(
        "live executor armed for {} — {} bps slippage floor, {} priority",
        trader.address(),
        cfg.slippage_bps,
        cfg.priority_micro_lamports
    );
    Ok(trader)
}


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Reading the measurement should not require starting a feed, a socket, or a
    // browser. `cb-bot --report [path]` prints what has been recorded and exits.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--verify") {
        let cfg = Config::load("config.toml")?;
        return verify(&cfg).await;
    }
    if args.iter().any(|a| a == "--report") {
        let path = args.iter().position(|a| a == "--report").and_then(|i| args.get(i + 1));
        return report(path.map_or(LEDGER_PATH, String::as_str));
    }

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
            rpc_http_fallbacks: Vec::new(),
            rpc_ws_url: "wss://api.mainnet-beta.solana.com".into(),
            min_profit_lamports: 0,
            max_position_lamports: 20_000_000,
            capital_usd: 100.0,
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
    });

    // The guard.
    //
    // This used to refuse `mode = "live"` outright, because execution did not exist and
    // the binary linked nothing that could sign. Both of those are now false: `crates/
    // execute.rs` builds real swap instructions and this binary links `cb-executor`,
    // `cb-wallet` and `solana-sdk`. The strongest property this project had — that no
    // argument about flags could produce a signature, because the code was absent — is
    // gone, and what replaces it has to be checked here rather than asserted.
    //
    // Three things must all be true before a key is ever loaded, and each is owned by a
    // different party: the config (the application writes it), the environment (the
    // operator sets it, and the application deliberately does not), and the passphrase
    // (only the person who chose it has it). Two of the three are not enough.
    let trader = if matches!(cfg.mode, Mode::Live) {
        let armed = std::env::var(cb_core::config::LIVE_ENV_VAR)
            .map(|v| v == "1")
            .unwrap_or(false);
        if !armed {
            anyhow::bail!(
                "config asks for mode = \"live\" but {} is not set to 1. That switch lives \
                 outside this application on purpose and it does not set it for you. \
                 Refusing to start half-armed: a process that believes it is trading and \
                 is not is worse than one that will not start.",
                cb_core::config::LIVE_ENV_VAR
            );
        }
        Some(arm_live(&cfg).await?)
    } else {
        None
    };

    let bus = EventBus::new();
    let addr: SocketAddr = LISTEN.parse()?;

    match cfg.feed {
        FeedSource::Simulated => spawn_simulated(bus.clone()),
        FeedSource::Live => spawn_live(bus.clone(), &cfg, trader).await?,
    }

    // Derived, not asserted. This line said "no transaction will be signed or sent"
    // unconditionally, which was true while execution did not exist and became a lie the
    // moment it did — printed, in capitals, in the one mode where it mattered. It is the
    // same failure as the hardcoded "paper" indicators, in the same file, found by
    // reading the log of the first live-armed run rather than by a test.
    match (&cfg.mode, cfg.dry_run) {
        (Mode::Paper, _) => {
            tracing::info!("mode: PAPER — no transaction will be signed or sent");
        }
        (Mode::Live, true) => {
            tracing::warn!(
                "mode: LIVE, dry_run = true — transactions will be built, signed and \
                 simulated against live state, and none will be submitted"
            );
        }
        (Mode::Live, false) => {
            tracing::error!(
                "mode: LIVE, dry_run = false — transactions will be SUBMITTED. Every one \
                 is simulated first and abandoned unless the simulated balance clears the \
                 profit floor, but money can move from here"
            );
        }
    }
    tracing::info!("api: http://127.0.0.1:8787 — the window is cryptobot-desk");

    // The app reads history from this same ledger directly, rather than through the
    // endpoint below, which is what lets it show the run after this process is gone.
    routes::serve(addr, routes::state_with_ledger(bus, mode_label(&cfg), LEDGER_PATH)).await
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

async fn spawn_live(
    bus: EventBus,
    cfg: &Config,
    trader: Option<execute::Trader>,
) -> anyhow::Result<()> {
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
    tracing::info!("feed: LIVE mainnet via {}", cb_core::redact::redact_endpoint(&cfg.rpc_ws_url));

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
    // Captured before the task takes ownership. `&'static str` so this costs nothing.
    let status_mode = mode_label(cfg);
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(2));
        loop {
            t.tick().await;
            let (updates, reconnects, dropped, last_slot, _) = status_stats.snapshot();
            let last_ms = status_stats.last_update_ms.load(Ordering::Relaxed);
            let stale_for = if last_ms == 0 { u64::MAX } else { now_ms().saturating_sub(last_ms) };
            let s = shared_for_status.lock().map(|g| g.clone()).unwrap_or_default();
            let best = s.best.as_ref();
            let tradeable = s.tradeable.as_ref();

            status_bus.publish(Event::Status {
                mode: format!("{status_mode} · live mainnet"),
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
                // Deliberately `None`, not `0.0`, when nothing qualifies: a zero here
                // would read as a market sitting exactly at break-even.
                tradeable_edge_bps: tradeable.map(|t| t.edge_bps),
                tradeable_route: tradeable.map_or_else(String::new, |t| t.route.clone()),
                tradeable_min_usd: s.tradeable_min_usd,
                stale_excluded: s.stale_excluded,
                feed_stalled: s.feed_stalled,
                cycles_evaluated: s.evaluated_total,
                venues: s.venues,
                duplicate_pairs: dupe_count,
                cheapest_round_trip_bps: if cheapest_bps.is_finite() { cheapest_bps } else { 0.0 },
                sweep_us: s.sweep_us,
                subscribed: status_stats.subscribed.load(Ordering::Relaxed),
                subscribe_errors: status_stats.subscribe_errors.load(Ordering::Relaxed),
                reconcile_drift: s.reconcile_drift,
                reconcile_checked: s.reconcile_checked,
            });
        }
    });

    let tradable_usd = cfg.tradable_usd();
    let min_depth_usd = cfg.tradeable_depth_usd();
    let max_hops = cfg.max_hops;
    tracing::info!(
        "capital: ${tradable_usd:.2} tradable, ${min_depth_usd:.2} minimum trade, \
         cycles up to {max_hops} hops"
    );
    tracing::info!(
        "capital ladder: every opportunity is also priced at {} — what a larger book \
         would have taken from the same moment",
        cb_ledger::CAPITAL_LADDER_USD
            .iter()
            .map(|r| format!("${r:.0}"))
            .collect::<Vec<_>>()
            .join(" / ")
    );

    // A run that keeps no record measures nothing. Failing to open it is not fatal —
    // the dashboard still works — but it is loud, because a silent loss of the
    // measurement is the one failure that would waste the whole run.
    let ledger = match cb_ledger::Ledger::open(LEDGER_PATH) {
        Ok(l) => {
            tracing::info!("recording measurements to {LEDGER_PATH} (cb-bot --report to read)");
            Some(l)
        }
        Err(e) => {
            tracing::error!("could not open {LEDGER_PATH}: {e:#} — this run will not be recorded");
            None
        }
    };

    // Captured before the sweep task takes ownership: `cfg` is a borrow and cannot cross
    // into a 'static task. Always true today, because a live config is refused at
    // startup — but derived rather than written as `true`, so that the flag stops being
    // correct by accident the moment execution exists.
    let paper_run = matches!(cfg.mode, Mode::Paper);

    tokio::spawn(async move {
        // Owned by the sweep task, because the risk gate is per-run state and there is
        // exactly one place trades are decided. `None` in paper mode, and then no code
        // below can reach a signature no matter what the rest of the loop does.
        let mut trader = trader;
        let mut next_id: u64 = 1;
        let mut evaluated_total: u64 = 0;
        let mut sweep_n: u32 = 0;
        // The sweep is a full pass over the cycle graph, not a reaction to one pool.
        // At this graph size it costs well under a millisecond, so running it on a
        // timer is cheaper than reasoning about which updates could matter — and it
        // has no blind spot for pools in the middle of a triangle.
        let mut sweep_timer = tokio::time::interval(SWEEP_INTERVAL);
        // Token valuations move slowly and are only used for sizing; rebuilding the
        // index every sweep would be work spent on a number that has not changed.
        let mut usd_timer = tokio::time::interval(USD_REFRESH);
        let mut reconcile_timer = tokio::time::interval(RECONCILE_INTERVAL);
        let mut drift = (0usize, 0usize);
        // Both are edge-triggered: logged when they change, not every sweep. A warning
        // that fires five times a second is a warning nobody reads.
        let mut was_stalled = false;
        let mut last_stale_excluded = 0usize;

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
                _ = reconcile_timer.tick() => {
                    // Blocks the other branches for a second or so. That is fine: the
                    // update channel is bounded at 4096 and buffers far more than a
                    // second of traffic, and trading on an unverified book is worse
                    // than trading on a book that is one second late.
                    match market.reconcile().await {
                        Ok(r) => {
                            drift = (r.drifted, r.checked);
                            if r.drifted > 0 || r.missing > 0 || r.undecodable > 0 {
                                tracing::warn!(
                                    "reconcile at slot {}: {}/{} pools drifted from the feed, \
                                     {} missing, {} undecodable",
                                    r.slot, r.drifted, r.checked, r.missing, r.undecodable
                                );
                            } else {
                                tracing::info!(
                                    "reconcile at slot {}: all {} pools matched the feed",
                                    r.slot, r.checked
                                );
                            }
                        }
                        Err(e) => tracing::warn!("reconcile failed: {e:#}"),
                    }
                }
                _ = sweep_timer.tick() => {
                    let sweep = market.sweep(tradable_usd, min_depth_usd, max_hops);
                    evaluated_total = evaluated_total.saturating_add(sweep.evaluated);

                    // The one thing the per-pool staleness guard structurally cannot
                    // see: a feed that has stopped entirely, where every pool ages
                    // together and nothing looks stale relative to anything else.
                    let last_ms = stats.last_update_ms.load(Ordering::Relaxed);
                    let feed_age_ms =
                        if last_ms == 0 { u64::MAX } else { now_ms().saturating_sub(last_ms) };
                    let feed_stalled = feed_age_ms >= FEED_STALL_SECS * 1_000;
                    if feed_stalled != was_stalled {
                        if feed_stalled {
                            tracing::warn!(
                                "feed silent for over {FEED_STALL_SECS}s — pausing the ledger; sweeps continue but nothing is recorded"
                            );
                        } else {
                            tracing::info!("feed recovered — recording resumed");
                        }
                        was_stalled = feed_stalled;
                    }
                    if sweep.stale_excluded != last_stale_excluded {
                        if sweep.stale_excluded > 0 {
                            tracing::warn!(
                                "{} pool(s) excluded from the sweep for lagging over {} slots",
                                sweep.stale_excluded,
                                live::MAX_STALE_LAG_SLOTS,
                            );
                        }
                        last_stale_excluded = sweep.stale_excluded;
                    }

                    if let Ok(mut g) = shared.lock() {
                        *g = SweepSummary {
                            // Report the *current* best, not an all-time high-water
                            // mark: a record from ten minutes ago describes nothing.
                            best: sweep.best().cloned(),
                            tradeable: sweep.tradeable().cloned(),
                            tradeable_min_usd: sweep.tradeable_min_usd,
                            stale_excluded: sweep.stale_excluded,
                            feed_stalled,
                            evaluated_total,
                            sweep_us: sweep.duration_us,
                            pools_ready: market.ready_count(),
                            venues: market.venue_count(),
                            sol_price_usd: market.sol_price_usd().unwrap_or(0.0),
                            slot: sweep.slot,
                            reconcile_drift: drift.0,
                            reconcile_checked: drift.1,
                        };
                    }

                    sweep_n = sweep_n.wrapping_add(1);
                    // Nothing is recorded while the feed is stalled: the numbers would
                    // describe a market that has stopped being observed, and they would
                    // be indistinguishable afterwards from ones that had.
                    if let (Some(l), Some(best), false) =
                        (ledger.as_ref(), sweep.best(), feed_stalled)
                    {
                        if sweep_n % LEDGER_EVERY_N_SWEEPS == 0 {
                            let tradeable = sweep.tradeable();
                            let sample = cb_ledger::SweepSample {
                                slot: sweep.slot,
                                evaluated: sweep.evaluated,
                                clearing: sweep.clearing,
                                best_edge_bps: best.edge_bps,
                                best_dislocation_bps: best.dislocation_bps,
                                best_fee_bps: best.fee_bps,
                                best_route: best.route.clone(),
                                best_venues: best.venues.clone(),
                                best_hops: best.hops,
                                best_depth_usd: best.depth_usd,
                                sol_price_usd: market.sol_price_usd().unwrap_or(0.0),
                                pools_ready: market.ready_count(),
                                sweep_us: sweep.duration_us,
                                tradeable_edge_bps: tradeable.map(|t| t.edge_bps),
                                tradeable_dislocation_bps: tradeable.map(|t| t.dislocation_bps),
                                tradeable_fee_bps: tradeable.map(|t| t.fee_bps),
                                tradeable_depth_usd: tradeable.map(|t| t.depth_usd),
                                tradeable_route: tradeable
                                    .map_or_else(String::new, |t| t.route.clone()),
                                stale_excluded: sweep.stale_excluded,
                                depth_measured: true,
                            };
                            if let Err(e) = l.record_sweep(&sample) {
                                tracing::warn!("could not record sweep: {e:#}");
                            }
                        }
                    }

                    if !sweep.rows.is_empty() {
                        bus.publish(Event::Routes {
                            rows: sweep.rows.iter().map(RouteRow::from).collect(),
                            tradeable_min_usd: sweep.tradeable_min_usd,
                            evaluated: sweep.evaluated,
                            sweep_us: sweep.duration_us,
                            slot: sweep.slot,
                            ts_ms: now_ms(),
                        });
                    }

                    let sol_price = market.sol_price_usd().unwrap_or(0.0);
                    // Reset every sweep: one attempt per pass, not one per run.
                    let mut attempted_this_sweep = false;
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

                        // Competition is charged once, in the tip, and not again here.
                        //
                        // This used to also decline every contested cycle outright,
                        // *after* pricing in a tip sized to win it — paying to win the
                        // race and then refusing to enter. The ledger says the flag was
                        // not detecting races anyway: cycles declined as contested were
                        // still quotable a slot later 27.9% of the time against 15.3%
                        // for everything else, so they survived *better*, which is what
                        // it looks like when nobody took them. A $0.01 threshold sorts
                        // by size and calls the big ones a race.
                        //
                        // What remains is the honest question: a bundle that loses does
                        // not land, so a lost race costs the base fee or nothing at all.
                        // Anything still positive after the tip it would have to pay is
                        // worth attempting. See `race_ladder` for what the assumption is
                        // worth at each win rate.
                        let skipped = if net <= 0.0 {
                            Some("net negative after tip".to_string())
                        } else {
                            None
                        };

                        // Decide what actually happened BEFORE anything is recorded.
                        //
                        // This used to write the fill first, with `taken: skipped.is_none()`,
                        // and only then try to execute. In paper mode that is exactly right:
                        // "taken" means "would have been taken" and there is nothing else it
                        // could mean. In live mode it was a lie — a row saying `taken` with a
                        // realised P&L, written before a single byte had left the machine.
                        //
                        // It showed up as the History panel reporting 9 trades and $0.0251
                        // realised against a wallet whose balance had not moved and whose
                        // explorer showed no transactions. The instrument was reporting money
                        // it had not made, while live. That is the §4 failure mode, in the
                        // one place where believing it costs real money.
                        let mut landed = false;
                        let mut realised = 0.0f64;
                        let mut signature: Option<String> = None;
                        let mut latency_ms: u64 = 0;
                        let mut outcome_reason = skipped.clone();

                        if skipped.is_none() {
                            match trader.as_mut() {
                                // Paper: nothing is submitted and nothing pretends to be.
                                // `landed` stays what it has always been for the paper
                                // measurement — the assumption the whole archive rests on.
                                None => {
                                    landed = true;
                                    realised = net;
                                    outcome_reason =
                                        Some("paper — uncontested, assumed landed".into());
                                }
                                Some(t) => {
                                    // One attempt per sweep, on the first encodable survivor
                                    // of a list already sorted by gross profit. Submitting a
                                    // dozen transactions against overlapping pools in one slot
                                    // would have each invalidate the next.
                                    let candidate = if attempted_this_sweep {
                                        None
                                    } else {
                                        opp.plan.as_ref().filter(|pl| pl.encodable())
                                    };
                                    match candidate {
                                        None => {
                                            outcome_reason = Some(
                                                "not attempted — one trade per sweep, or no \
                                                 encoder for this route"
                                                    .into(),
                                            );
                                        }
                                        Some(plan) => {
                                            attempted_this_sweep = true;
                                            let started = std::time::Instant::now();
                                            let r = t.attempt(plan, opp.size_usd, net).await;
                                            latency_ms = started.elapsed().as_millis() as u64;
                                            match r {
                                                Ok(cb_executor::Attempt::Submitted {
                                                    signature: sig,
                                                    ..
                                                }) => {
                                                    // Submitted is not confirmed. The signature
                                                    // is a receipt for having asked, so the
                                                    // realised figure stays zero until a
                                                    // confirmation path fills it in — a P&L
                                                    // nobody has confirmed is not a P&L.
                                                    t.record(
                                                        cb_executor::risk::Outcome::Landed {
                                                            net_usd: 0.0,
                                                        },
                                                    );
                                                    tracing::info!("submitted {sig}");
                                                    landed = true;
                                                    outcome_reason = Some(
                                                        "submitted; not yet confirmed".into(),
                                                    );
                                                    signature = Some(sig);
                                                }
                                                Ok(cb_executor::Attempt::SimulationRejected {
                                                    reason,
                                                    ..
                                                }) => {
                                                    tracing::warn!(
                                                        "simulation rejected: {reason}"
                                                    );
                                                    outcome_reason =
                                                        Some(format!("simulation: {reason}"));
                                                }
                                                Ok(cb_executor::Attempt::Refused(why)) => {
                                                    // Logged at info, not debug. A live run
                                                    // that refuses everything must say so in
                                                    // the log the operator actually reads;
                                                    // at debug this was invisible and looked
                                                    // like nothing happening at all.
                                                    tracing::info!("refused: {why}");
                                                    outcome_reason =
                                                        Some(format!("refused: {why}"));
                                                }
                                                Err(e) => {
                                                    // An RPC failure is not a defect in what
                                                    // was built, so it must not trip the
                                                    // breaker that exists to catch defects.
                                                    t.record(cb_executor::risk::Outcome::Missed);
                                                    tracing::warn!(
                                                        "execution could not reach the chain: {e:#}"
                                                    );
                                                    outcome_reason =
                                                        Some(format!("rpc error: {e}"));
                                                }
                                            }
                                            if let Some(why) = t.halted() {
                                                tracing::error!("trading halted: {why}");
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if let (Some(l), false) = (ledger.as_ref(), feed_stalled) {
                            let rec = cb_ledger::FillRecord {
                                slot: opp.slot,
                                route: opp.route.clone(),
                                venues: opp.venues.clone(),
                                hops: opp.hops,
                                edge_bps: opp.edge_bps,
                                dislocation_bps: opp.edge_bps + opp.fee_bps,
                                fee_bps: opp.fee_bps,
                                size_usd: opp.size_usd,
                                optimal_size_usd: opp.optimal_size_usd,
                                gross_usd: opp.gross_profit_usd,
                                profit_at_optimal_usd: opp.profit_at_optimal_usd,
                                tip_usd: est_tip_usd,
                                // The realised figure, not the hoped-for one. In paper these
                                // are the same number; in live they are not, and the whole
                                // point of the run is the difference.
                                net_usd: realised,
                                taken: landed,
                                skipped_reason: outcome_reason.clone(),
                                cycle_key: opp.cycle_key.clone(),
                                profit_at_capital_usd: Some(opp.profit_at_capital_usd),
                                slot_spread: Some(opp.slot_spread),
                            };
                            if let Err(e) = l.record_fill(&rec) {
                                tracing::warn!("could not record fill: {e}");
                            }
                        }

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

                        if skipped.is_some() {
                            continue;
                        }

                        bus.publish(Event::Execution {
                            id,
                            opportunity_id: id,
                            paper: paper_run,
                            landed,
                            realised_usd: realised,
                            tip_paid_usd: est_tip_usd,
                            latency_ms,
                            signature,
                            reason: outcome_reason,
                            ts_ms: now_ms(),
                        });
                    }
                }
            }
        }
    });

    Ok(())
}

/// Buckets for the distance-to-profitable histogram, in bps.
///
/// Tight around zero because that is where the answer lives: at these fee tiers the
/// market spends its time within a couple of basis points of breaking even, and a
/// histogram with 10 bps buckets would show one tall bar and tell you nothing.
const EDGE_BUCKETS: [f64; 11] =
    [-1e9, -20.0, -10.0, -5.0, -2.0, -1.0, 0.0, 1.0, 2.0, 5.0, 1e9];

/// Print what the run has measured, and stop.
///
/// Separate from the live loop on purpose: the measurement is the product, and it
/// should be readable without starting a feed, a socket, or a browser.
fn report(path: &str) -> anyhow::Result<()> {
    let ledger = cb_ledger::Ledger::open(path)?;
    let s = ledger.summary()?;

    println!("\ncryptobot — measurement report");
    println!("  ledger        {path}");
    match (&s.first_at, &s.last_at) {
        (Some(a), Some(b)) => println!("  window        {a} .. {b} UTC"),
        _ => println!("  window        (no samples yet)"),
    }
    if s.samples == 0 {
        println!("\nNothing recorded yet. Run the bot for a while first.\n");
        return Ok(());
    }

    if !s.has_depth_measurement() {
        println!(
            "\n  This ledger predates the tradeable/marginal split, so every edge in it\n  \
             is a marginal rate with no depth behind it. Those are not opportunities;\n  \
             see HANDOVER 5.1. Only the fill-derived sections below are comparable.\n  \
             Highest marginal rate on record: {:.2} bps.",
            s.marginal_best_edge_bps
        );
    }

    println!("\n  DISTANCE TO PROFITABLE      (best *tradeable* route per sweep)");
    println!("    samples                   {}", s.samples);
    if s.has_depth_measurement() {
        println!(
            "    with a tradeable cycle    {}   ({:.1}% of {} depth-measured)",
            s.tradeable_samples,
            100.0 * s.tradeable_samples as f64 / s.depth_samples as f64,
            s.depth_samples
        );
        println!("    mean edge                 {:>8.2} bps", s.mean_edge_bps);
        println!("    best edge                 {:>8.2} bps", s.best_edge_bps);
        println!("    mean price dislocation    {:>8.2} bps", s.mean_dislocation_bps);
        println!("    widest dislocation        {:>8.2} bps", s.best_dislocation_bps);
        println!("    mean fee wall             {:>8.2} bps", s.mean_fee_bps);
        println!(
            "    moments something cleared {:>8.2}%   ({} of {})",
            s.clearing_rate() * 100.0,
            s.clearing_samples,
            s.depth_samples
        );

        // The gap between the two searches, stated rather than smoothed away. It is
        // not noise: it measures how much of the visible book has no size behind it.
        println!("\n  WHAT THE BOOK ADVERTISES BUT WILL NOT FILL");
        println!(
            "    best marginal rate ever   {:>8.2} bps   (at infinitesimal size)",
            s.marginal_best_edge_bps
        );
        println!(
            "    leader untradeable in     {:>8.2}%   ({} of {} samples)",
            s.untradeable_leader_rate() * 100.0,
            s.untradeable_leader_samples,
            s.depth_samples
        );
        if s.best_edge_bps > 0.0 && s.marginal_best_edge_bps > s.best_edge_bps * 2.0 {
            println!(
                "    the marginal maximum is {:.0}x the best rate anyone could have taken.",
                s.marginal_best_edge_bps / s.best_edge_bps
            );
        }
        if s.stale_excluded_max > 0 {
            println!(
                "    most pools excluded once  {:>8}       (lagging over {} slots)",
                s.stale_excluded_max,
                live::MAX_STALE_LAG_SLOTS
            );
        }
    }

    println!("\n  EDGE DISTRIBUTION");
    let hist = ledger.edge_histogram(&EDGE_BUCKETS)?;
    let peak = hist.iter().map(|(_, _, n)| *n).max().unwrap_or(1).max(1);
    for (lo, hi, n) in &hist {
        if *n == 0 {
            continue;
        }
        let label = match (*lo, *hi) {
            (l, _) if l <= -1e8 => format!("     below {:>6.0}", -20.0),
            (_, h) if h >= 1e8 => format!("     above {:>6.1}", 5.0),
            (l, h) => format!("  {l:>7.1} .. {h:>6.1}"),
        };
        let bar = "#".repeat(((*n as f64 / peak as f64) * 46.0).round() as usize);
        println!("  {label}  {n:>7}  {bar}");
    }

    let hours = ledger.hours_observed()?.max(1e-9);
    println!("\n  CYCLES THAT CLEARED THEIR OWN FEES");
    println!("    observed for              {hours:.2} h");
    let ep = ledger.episodes(EPISODE_GAP_SLOTS)?;
    println!("    detections                {}   ({:.0}/h)", s.fills, s.fills as f64 / hours);
    println!(
        "    distinct opportunities    {}   ({:.1}/h)",
        ep.count,
        ep.count as f64 / hours
    );
    println!("    of those, we would take   {}", ep.taken);
    println!(
        "    detections per opportunity{:>8.0}   <- one standing gap, re-seen every sweep",
        ep.inflation()
    );
    println!(
        "    longest single episode    {} detections ({} slots)",
        ep.longest_detections, ep.longest_slots
    );
    println!();
    println!(
        "    net, counting each once   ${:.6}   (${:.4}/h)",
        ep.total_net_usd,
        ep.total_net_usd / hours
    );
    println!("    median opportunity        ${:.6}", ep.median_net_usd);
    println!("    best opportunity          ${:.6}", ep.best_net_usd);
    println!(
        "    [summing every detection would say ${:.2}. It counts the same gap {:.0}",
        s.realised_net_usd,
        ep.inflation()
    );
    println!("     times over, and taking an arbitrage is what removes it. Not real.]");

    let (top_route, share) = ledger.concentration()?;
    if share > 0.0 {
        println!(
            "    from one route            {:.0}%  ({})",
            share * 100.0,
            top_route
        );
        if share > 0.4 {
            println!(
                "                              ^ every average above is mostly this pair"
            );
        }
    }

    let bands = ledger.survival(EPISODE_GAP_SLOTS)?;
    if !bands.is_empty() {
        println!("
  HOW LONG AN OPPORTUNITY LASTS, AGAINST WHAT IT IS WORTH");
        println!(
            "    {:<16} {:>8} {:>10} {:>10} {:>15}",
            "whole pie", "seen", "avg life", "longest", "capital needed"
        );
        for b in &bands {
            println!(
                "    {:<16} {:>8} {:>9.1}s {:>9.1}s {:>15}",
                b.label,
                b.episodes,
                b.mean_secs(),
                b.longest_slots as f64 * 0.4,
                format!("${:.0}", b.mean_capital_usd())
            );
        }
        println!("    Size and lifetime run in opposite directions. A longest of 0.0s means");
        println!("    every gap in that band was gone before the next slot began — there is");
        println!("    no size at which one is both worth taking and still there on arrival.");
    }

    let p = ledger.fill_percentiles()?;
    println!("\n  WHAT ONE OPPORTUNITY IS WORTH");
    println!("    {:<22} {:>12} {:>12} {:>12}", "", "median", "p90", "p99");
    println!(
        "    {:<22} {:>12} {:>12} {:>12}",
        "the whole opportunity",
        format!("${:.6}", p.at_optimal_p50),
        format!("${:.6}", p.at_optimal_p90),
        format!("${:.6}", p.at_optimal_p99)
    );
    println!(
        "    {:<22} {:>12} {:>12} {:>12}",
        "kept, after costs",
        format!("${:.6}", p.taken_net_p50),
        format!("${:.6}", p.taken_net_p90),
        format!("${:.6}", p.taken_net_p99)
    );
    println!("    median size traded        ${:.2}", p.size_p50);
    if p.at_optimal_p50 > 0.0 {
        println!(
            "    our capital reached       {:.0}% of the median opportunity",
            100.0 * p.taken_net_p50 / p.at_optimal_p50
        );
    }
    println!(
        "    best seen, whole pie      ${:.6}   <- the ceiling, at any capital",
        s.best_profit_at_optimal_usd
    );

    // The fixed cost of a transaction does not shrink with the trade, so below a
    // certain account size every opportunity is negative regardless of how good the
    // price is. Saying where that line falls is more useful than any average.
    let sol_price = ledger.median_sol_price()?;
    let fixed_cost = (JITO_TIP_FLOOR_SOL + BASE_FEE_SOL) * sol_price;
    if p.taken_net_p50 > 0.0 && p.size_p50 > 0.0 {
        let edge_frac = (p.taken_net_p50 + fixed_cost) / p.size_p50;
        if edge_frac > 0.0 {
            println!(
                "\n  BREAK-EVEN CAPITAL        ${:.2}   at the median edge of {:.2} bps",
                fixed_cost / edge_frac,
                edge_frac * 10_000.0
            );
            println!(
                "    Costs are per transaction, not per dollar: ~${fixed_cost:.4} of tip and"
            );
            println!("    base fee whatever the size. Below that line nothing clears.");
        }
    }

    // The counterfactual this run exists to answer. Every rung prices the *same*
    // episodes, so the gaps between them are what capital buys and nothing else — and a
    // rung that fails to beat the one below it is depth, not funding, running out.
    let ladder = ledger.capital_ladder(EPISODE_GAP_SLOTS)?;
    if ladder.measured_episodes > 0 {
        println!("\n  WHAT A BIGGER BOOK WOULD HAVE TAKEN");
        println!("    {:<24} {:>14} {:>17}", "book size", "gross, run", "vs the rung below");
        println!(
            "    {:<24} {:>14} {:>17}",
            "this run, after costs",
            format!("${:.4}", ladder.realised_usd),
            "—"
        );
        let mut prev = 0.0f64;
        for (i, (rung, got)) in ladder.rungs.iter().enumerate() {
            let delta = if i == 0 {
                String::from("—")
            } else if got - prev < 1e-9 {
                String::from("nothing more")
            } else {
                format!("+${:.4}", got - prev)
            };
            println!("    {:<24} {:>14} {:>17}", format!("${rung:.0}"), format!("${got:.4}"), delta);
            prev = *got;
        }
        println!(
            "    {:<24} {:>14} {:>17}",
            "unlimited",
            format!("${:.4}", ladder.at_optimal_usd),
            if ladder.at_optimal_usd - prev < 1e-9 { "nothing more" } else { "" }
        );
        println!(
            "\n    Gross of tip and assuming every race is won — an upper bound on both\n    \
             counts. Measured over {} episode{}{}.",
            ladder.measured_episodes,
            if ladder.measured_episodes == 1 { "" } else { "s" },
            if ladder.unmeasured_episodes > 0 {
                format!(
                    ";\n    {} more predate the ladder and are left out rather than counted as zero",
                    ladder.unmeasured_episodes
                )
            } else {
                String::new()
            }
        );
        println!("    Where two rungs agree the cycles ran out of depth, not funding.");
        println!("    Borrowed capital cannot widen a tick, so a flat step is the");
        println!("    measurement that says a flash loan would have added nothing.");
    }

    // Whether the reported gaps were ever simultaneously available. A dislocation is a
    // claim that two venues disagreed at one moment; if the claim grows with how far
    // apart in time the legs were read, what is being reported is the market moving
    // between two observations, not two venues disagreeing.
    let spread = ledger.spread_audit()?;
    let measured: u64 = spread.iter().map(|b| b.fills).sum();
    if measured > 0 {
        println!("\n  WERE THE TWO PRICES EVER ON SCREEN AT THE SAME TIME?");
        println!(
            "    {:<12} {:>9} {:>14} {:>10} {:>13}",
            "legs apart", "fills", "mean gap bps", "mean fee", "value @ $100"
        );
        for b in &spread {
            if b.fills == 0 {
                continue;
            }
            println!(
                "    {:<12} {:>9} {:>14.2} {:>10.2} {:>13}",
                b.label,
                b.fills,
                b.mean_dislocation_bps,
                b.mean_fee_bps,
                format!("${:.4}", b.value_at_100_usd)
            );
        }
        println!(
            "\n    Flat is healthy: a real disagreement between venues has no reason to\n    \
             depend on whether we read them one slot apart or five hundred. Rising is\n    \
             the instrument reporting the market's movement between two observations as\n    \
             an edge — one that was never simultaneously on offer and cannot be taken.\n    \
             `--verify` cannot see this: it checks one pool against a router at one\n    \
             instant, and this is a gap that only exists across two."
        );
    }

    // The cut that isolates timing from disagreement. Comparing spread bands does not
    // work - each band mixes fee tiers and the effect hides inside them. Holding the fee
    // tier fixed and requiring both legs from one slot is what shows it.
    let sim = ledger.simultaneity_audit()?;
    if sim.iter().any(|t| t.fills_same_slot > 0) {
        println!("
  WAS THE EDGE EVER SIMULTANEOUSLY ON OFFER?");
        println!(
            "    {:<13} {:>9} {:>11} {:>11} {:>11} {:>10}",
            "route fee", "fills", "edge, all", "same-slot", "n same-slot", "timing"
        );
        for t in &sim {
            if t.fills_all == 0 {
                continue;
            }
            let timing = t
                .timing_share()
                .map_or_else(|| "—".to_string(), |v| format!("{:.0}%", v * 100.0));
            println!(
                "    {:<13} {:>9} {:>11.2} {:>11.2} {:>11} {:>10}",
                t.label, t.fills_all, t.edge_all_bps, t.edge_same_slot_bps, t.fills_same_slot, timing
            );
        }
        println!();
        for line in [
            "    82% of loops price their two legs from different slots, because the",
            "    staleness guard admits a pool minutes behind the head. Where the",
            "    same-slot column is lower, that tier was reporting the market moving",
            "    between two observations as two venues disagreeing — an edge that was",
            "    never simultaneously on offer and cannot be taken.",
            "    Read the sample size before believing either column.",
        ] {
            println!("{line}");
        }
    }

    // What the contest rule costs, priced across the range of win rates rather than
    // assumed at one. The rule refuses cycles it has *already* charged a tip large
    // enough to win, so competition is priced twice — once as a haircut, again as a
    // refusal — and the refusal decides most of the value this instrument ever sees.
    let race = ledger.race_ladder(EPISODE_GAP_SLOTS)?;
    if race.declined_episodes > 0 {
        println!("\n  WHAT REFUSING CONTESTED RACES COSTS");
        println!("    {:<28} {:>14}", "if we win this share…", "net, run");
        for (p, got) in &race.rungs {
            let label = if *p == 0.0 {
                "  0%  (what we book now)".to_string()
            } else {
                format!("{:>3.0}%", p * 100.0)
            };
            println!("    {:<28} {:>14}", label, format!("${got:.4}"));
        }
        println!(
            "\n    {} episode{} worth ${:.4} net are refused for being contested — after\n    \
             already paying a tip sized to win them. A lost race costs the base fee or\n    \
             nothing at all, since the bundle does not land, so the downside of trying is\n    \
             close to zero and this ladder is close to linear.",
            race.declined_episodes,
            if race.declined_episodes == 1 { "" } else { "s" },
            race.declined_net_usd,
        );
        if race.declined_unprofitable_episodes > 0 {
            println!(
                "    A further {} were already negative after tip; refusing those is right\n    \
                 at any win rate, and they carry no weight here.",
                race.declined_unprofitable_episodes
            );
        }
        println!("    Which rung is real cannot be settled on paper. Only trying settles it.");
    }

    // The largest unverified assumption in the instrument, checked against its own data.
    // Opportunities over a profit threshold are declined as races we would lose; that
    // decision is worth more than every other decision here combined, and until now
    // nothing has tested it.
    let ca = ledger.contest_audit(EPISODE_GAP_SLOTS)?;
    if ca.contested_episodes > 0 || ca.uncontested_episodes > 0 {
        println!("\n  IS THE CONTEST MODEL MEASURING ANYTHING?");
        println!(
            "    {:<22} {:>10} {:>14} {:>14}",
            "", "episodes", "outlived slot", "avg life"
        );
        println!(
            "    {:<22} {:>10} {:>14} {:>14}",
            "declined as contested",
            ca.contested_episodes,
            format!("{:.0}%", 100.0 * ca.contested_survival_rate()),
            format!("{:.1} slots", ca.contested_mean_slots())
        );
        println!(
            "    {:<22} {:>10} {:>14} {:>14}",
            "not contested",
            ca.uncontested_episodes,
            format!("{:.0}%", 100.0 * ca.uncontested_survival_rate()),
            format!("{:.1} slots", ca.uncontested_mean_slots())
        );
        println!(
            "\n    Declined value: ${:.4}, of which ${:.4} was still quotable a slot",
            ca.declined_usd, ca.declined_but_survived_usd
        );
        println!("    later — which means nobody had taken it, so that race was not lost.");

        if ca.has_enough_evidence() {
            let (c, u) = (ca.contested_survival_rate(), ca.uncontested_survival_rate());
            if c >= u * 0.9 {
                println!(
                    "\n    VERDICT: declined opportunities survive about as well as accepted\n    \
                     ones ({:.0}% vs {:.0}%). The threshold is sorting by size and calling\n    \
                     it competition. It is a profit cutoff, so this is what it does by\n    \
                     construction — and it is discarding real money to do it.",
                    100.0 * c,
                    100.0 * u
                );
            } else {
                println!(
                    "\n    VERDICT: declined opportunities do vanish faster ({:.0}% vs {:.0}%\n    \
                     survival). Consistent with somebody else taking them, though a price\n    \
                     that simply moved looks identical from here.",
                    100.0 * c,
                    100.0 * u
                );
            }
        } else {
            println!(
                "\n    Not enough of both groups yet to compare ({} contested, {} not;\n    \
                 20 of each is the bar). Let the run continue.",
                ca.contested_episodes, ca.uncontested_episodes
            );
        }
    }

    let routes = ledger.top_routes(8)?;
    if !routes.is_empty() {
        println!("\n  ROUTES THAT CLEARED MOST OFTEN");
        println!("    {:<26} {:<28} {:>6} {:>9} {:>11}", "route", "venues", "times", "mean bps", "best net");
        for r in routes {
            println!(
                "    {:<26} {:<28} {:>6} {:>9.2} ${:>10.6}",
                truncate(&r.route, 26),
                truncate(&r.venues, 28),
                r.fills,
                r.mean_edge_bps,
                r.best_net_usd
            );
        }
    }
    println!();
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// Tolerance before a disagreement with the router is called a fault, in bps.
///
/// Generous on purpose. Jupiter's quote is net of its own accounting and may be taken
/// a slot or two from ours, so small differences are noise. The failure this is hunting
/// is not subtle: the creator-fee bug it was built after was worth 52 bps.
const VERIFY_TOLERANCE_BPS: f64 = 20.0;

/// Pause between router requests. The public endpoint is rate limited and this is an
/// audit, not a race.
const VERIFY_PACE: Duration = Duration::from_millis(1200);

/// Cross-check every decoded pool against an independent router.
///
/// # Why one-sided
///
/// Jupiter routes across these venues and more. So for any pair, the best output *it*
/// can find should be at least as good as the best output *we* think one of our pools
/// offers. If ours is materially better, we are not finding an edge nobody else can —
/// we are decoding something wrong. That asymmetry is the whole test: being *worse*
/// than the router is fine and expected (it knows venues we do not), being *better* is
/// a bug.
///
/// This exists because a decoder that is wrong in the profitable direction produces
/// numbers that look like success. A missing fee bucket in Raydium CP-Swap put one
/// pool 52 bps off its true price, and the cycle search reported the difference as a
/// standing arbitrage for four hours. Every internal consistency check passed, because
/// the error was in the input, not the arithmetic. Only an outside opinion could catch
/// it, so now there is one, on demand.
async fn verify(cfg: &Config) -> anyhow::Result<()> {
    let registry = registry::Registry::embedded()?;
    println!("cryptobot — decoder audit against an independent router\n");
    println!("  reading {} pools from chain...", registry.pools.len());

    let mut market = live::LiveMarket::bootstrap(&cfg.rpc_http_url, registry).await?;
    market.rebuild_usd_index();
    let probes = market.audit_probes(5.0);
    println!("  {} pool/direction pairs to check, ~{:.0}s\n", probes.len(),
             probes.len() as f64 * VERIFY_PACE.as_secs_f64());

    let client = reqwest::Client::new();
    println!(
        "  {:<18} {:<9} {:>13} {:>13} {:>8}  routed via     verdict",
        "pair", "venue", "ours", "router", "diff"
    );

    let (mut checked, mut faults, mut skipped, mut off_premise) = (0usize, 0usize, 0usize, 0usize);
    for p in probes {
        tokio::time::sleep(VERIFY_PACE).await;
        let theirs = match jupiter_quote(&client, &p.from_b58, &p.to_b58, p.amount_in).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                skipped += 1;
                continue;
            }
            Err(e) => {
                println!("  {:<18} {:<9} router error: {e:#}", p.pair, p.venue);
                skipped += 1;
                continue;
            }
        };
        checked += 1;

        // Positive means our quote claims more output than the router could find.
        let diff_bps = if theirs.out > 0 {
            (p.amount_out as f64 / theirs.out as f64 - 1.0) * 10_000.0
        } else {
            0.0
        };
        let beaten = diff_bps > VERIFY_TOLERANCE_BPS;

        // Being beaten only implicates a decoder if the router was quoting the same
        // kind of liquidity. When it served an RFQ venue we do not decode, the audit's
        // premise simply does not apply to that row — which is reported, never
        // silently excused and never counted as a clean pass.
        let verdict = if beaten && theirs.routed_through(&p.pool_b58) {
            faults += 1;
            "FAULT — router used THIS pool and still paid less"
        } else if beaten && theirs.touches_a_venue_we_decode() {
            faults += 1;
            "FAULT — we quote better than any router can route"
        } else if beaten {
            off_premise += 1;
            "premise broken — router served liquidity we do not decode"
        } else if diff_bps < -100.0 {
            "ok (router found a venue we do not watch)"
        } else {
            "ok"
        };
        println!(
            "  {:<18} {:<9} {:>13} {:>13} {:>+7.1}b  {:<14} {verdict}",
            truncate(&format!("{} {}", p.pair, p.label), 18),
            p.venue,
            p.amount_out,
            theirs.out,
            diff_bps,
            truncate(&theirs.venues(), 14),
        );
    }

    println!("\n  {checked} checked, {skipped} skipped, {faults} faults, {off_premise} off-premise");
    if faults == 0 {
        println!("  No pool quotes better than the router can route. Decoders look honest.");
    } else {
        println!(
            "  {faults} pool(s) claim an output nobody else can produce. That is a decode \n\
             \x20 error, not an edge — do not trade on those routes until it is explained."
        );
    }
    if off_premise > 0 {
        println!(
            "  {off_premise} row(s) were beaten only against liquidity we do not decode — RFQ\n\
             \x20 market-maker fills rather than pools. The audit cannot judge those: it\n\
             \x20 assumes the router is quoting the same venues we are. Inspect by hand\n\
             \x20 rather than reading them as either a pass or a fault."
        );
    }
    println!();
    Ok(())
}

/// What the router answered, and which venues it actually went through.
///
/// The route matters as much as the number. The audit's premise is "a router that
/// covers these venues cannot be beaten by one of them", and that premise only holds
/// while the router is quoting AMM liquidity. Jupiter now serves RFQ fills under
/// labels like `Aquifer` and `Flux`, which are market-maker quotes rather than pools —
/// being beaten by one of those says nothing about our decoders.
#[derive(Debug, Clone)]
struct RouterQuote {
    out: u128,
    /// One entry per leg: the venue label and the account it swapped against.
    legs: Vec<(String, String)>,
}

impl RouterQuote {
    fn venues(&self) -> String {
        let mut names: Vec<&str> = self.legs.iter().map(|(l, _)| l.as_str()).collect();
        names.dedup();
        names.join("+")
    }

    /// Whether any leg ran through a venue family we decode ourselves.
    ///
    /// Matched on the label rather than a list of program ids, because the label is
    /// what the router reports and what a human reading the audit sees. An unknown
    /// label is treated as *not* ours, which is the conservative direction: it
    /// downgrades a fault to "inspect this", never the reverse.
    fn touches_a_venue_we_decode(&self) -> bool {
        const OURS: [&str; 4] = ["orca", "raydium", "meteora", "whirlpool"];
        self.legs
            .iter()
            .any(|(label, _)| {
                let l = label.to_ascii_lowercase();
                OURS.iter().any(|o| l.contains(o))
            })
    }

    fn routed_through(&self, pool_b58: &str) -> bool {
        self.legs.iter().any(|(_, amm)| amm == pool_b58)
    }
}

/// Best output the router can find for a direct swap, or `None` if it has no route.
async fn jupiter_quote(
    client: &reqwest::Client,
    from: &str,
    to: &str,
    amount_in: u128,
) -> anyhow::Result<Option<RouterQuote>> {
    let url = format!(
        "https://lite-api.jup.ag/swap/v1/quote?inputMint={from}&outputMint={to}\
         &amount={amount_in}&slippageBps=50&onlyDirectRoutes=true"
    );
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let v: serde_json::Value = resp.json().await?;
    let Some(out) = v
        .get("outAmount")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<u128>().ok())
    else {
        return Ok(None);
    };

    let legs = v
        .get("routePlan")
        .and_then(serde_json::Value::as_array)
        .map(|plan| {
            plan.iter()
                .filter_map(|step| step.get("swapInfo"))
                .map(|info| {
                    let label = info
                        .get("label")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let amm = info
                        .get("ammKey")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    (label, amm)
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Some(RouterQuote { out, legs }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(out: u128, legs: &[(&str, &str)]) -> RouterQuote {
        RouterQuote {
            out,
            legs: legs.iter().map(|(l, a)| ((*l).to_string(), (*a).to_string())).collect(),
        }
    }

    #[test]
    fn the_staleness_guard_does_not_fire_while_reconcile_is_working() {
        // These two constants are coupled and live in different files. `reconcile()`
        // re-reads every watched account on its own timer and refreshes each pool's
        // slot, so any guard tighter than that cadence spends its time excluding pools
        // that were just verified — measured at ~40 of 84 pools and a third of the
        // cycle graph, for nothing. The guard is a backstop for reconcile having
        // stopped, not a routine filter, so it must sit above the reconcile interval.
        let guard_ms = live::MAX_STALE_LAG_SLOTS * SLOT_MS;
        let reconcile_ms = RECONCILE_INTERVAL.as_millis() as u64;
        assert!(
            guard_ms >= reconcile_ms * 2,
            "the staleness guard ({guard_ms} ms) must outlast two reconciles              ({reconcile_ms} ms each), or it excludes pools reconcile has already              proven correct and quietly shrinks the search space"
        );
    }

    #[test]
    fn a_fault_against_liquidity_we_do_not_watch_is_labelled_not_counted() {
        // Jupiter serves RFQ fills under names like these. They are market-maker
        // quotes, not pools, so being beaten by one says nothing about our decoders —
        // and must not be scored as if it did.
        let rfq = quote(1_000, &[("Aquifer", "someRfqAccount")]);
        assert!(!rfq.touches_a_venue_we_decode());
        assert!(!rfq.routed_through("ourPool111"));

        let ours = quote(1_000, &[("Orca (Whirlpools)", "ourPool111")]);
        assert!(ours.touches_a_venue_we_decode(), "an Orca leg is a venue we decode");
    }

    #[test]
    fn a_router_leg_through_our_own_pool_is_recognised() {
        // The strongest evidence a decode fault can produce: the router priced the
        // exact account we did and still paid out less.
        let q = quote(900, &[("Raydium CLMM", "poolAAA"), ("Meteora DLMM", "poolBBB")]);
        assert!(q.routed_through("poolBBB"));
        assert!(!q.routed_through("poolCCC"));
    }

    #[test]
    fn an_unknown_venue_label_is_treated_as_not_ours() {
        // Conservative direction: an unrecognised label downgrades a fault to
        // "inspect this by hand", never the other way round.
        let q = quote(1_000, &[("SomeNewAggregator", "acct")]);
        assert!(!q.touches_a_venue_we_decode());
    }

    #[test]
    fn the_route_is_named_for_the_reader() {
        let q = quote(1, &[("Orca (Whirlpools)", "a"), ("Orca (Whirlpools)", "b")]);
        assert_eq!(q.venues(), "Orca (Whirlpools)", "a repeated venue reads once");
        let q = quote(1, &[("Orca (Whirlpools)", "a"), ("Raydium CLMM", "b")]);
        assert_eq!(q.venues(), "Orca (Whirlpools)+Raydium CLMM");
    }
}
