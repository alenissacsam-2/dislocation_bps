//! cryptobot — Solana arbitrage research system.
//!
//! Paper mode is the default. Live *trading* requires two independent switches:
//! `mode = "live"` in the config **and** `CRYPTOBOT_ALLOW_LIVE=1` in the environment.
//! Live *data* is a separate, read-only setting and is on by default.
//!
//! See `docs/superpowers/specs/` for the design and `docs/research/` for the numbers.

mod demand;
mod execute;
mod live;
mod live_log;
mod registry;
mod sim;

use cb_core::config::{Config, FeedSource, Mode, SubmitVia};
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

/// Whether the transactions this bot builds actually carry a Jito tip.
///
/// They do not. [`cb_executor::route::build`] emits a compute-budget instruction, the
/// wrap, the swaps and the close, and nothing else — there is no transfer to a tip
/// account anywhere in the executor, and `priority_micro_lamports` defaults to zero.
///
/// Charging the tip anyway is not conservatism, it is a wrong number in the one place
/// it does the most damage. The median believable opportunity here grosses $0.00044
/// and the tip floor is $0.00077, so a cost that is never incurred was refusing trades
/// at nearly twice the rate the real cost would have: 879 cycles over fifteen hours
/// were declined as "net negative after tip" while being positive against the fee the
/// wallet actually pays.
///
/// The counter-argument is that the tip models *competition* — a cycle we do not bid
/// for is a cycle somebody else wins. That is true and it is still not a cost, because
/// a race we lose is discovered in simulation and never submitted. It costs a round
/// trip, not money. Competition is priced where it belongs instead: see
/// [`EXECUTABLE_FEE_CEILING_BPS`], which is measured rather than assumed.
///
/// Flip this the moment a tip instruction exists, and the arithmetic below follows.
const PAYS_A_TIP: bool = false;

/// How far apart, in slots, a cycle's two legs may have been priced and still be worth
/// trading.
///
/// This bounds the *artifact*, not the risk. An edge computed from two prices observed
/// seconds apart is partly the market moving between the observations, and that part was
/// never simultaneously on offer to anyone. `--report` has argued this since the
/// artifact was found; until now nothing stopped execution acting on it.
///
/// # Why this is twelve and not one
///
/// It was one, which is as strict as the measure can be, and at one it became the
/// largest single filter on candidates worth having: over thirty-nine minutes of live
/// running, **109 of the ~151 cycles that cleared the economics gate died here** while
/// only two ever reached the chain.
///
/// What it was rejecting was mostly not an artifact. The biggest buckets were spreads
/// of eight and nine slots between two Raydium CLMM pools, and `reconcile` — which
/// re-reads every watched account over HTTP and compares — was reporting one to three
/// pools drifted out of seventy-nine, often none at all. The prices were right. This
/// module's own note on reconciliation says why: *no update means no change*, so an old
/// slot on a pool nobody has swapped is a correct price rather than a stale one, and
/// nothing in a slot number tells those two apart.
///
/// The honest test is not the slot at all. It is whether the edge survives being
/// re-priced against both pools fetched in the same round trip, which `hops_for` does
/// on every attempt, and which costs a round trip and no money when it fails. This is
/// now a cheap pre-filter in front of that: it keeps the sweep's one attempt away from
/// the far bands, where the artifact really does dominate — the run that found it
/// measured 1.27 bps of mean edge at 0-1 slots against 3.42 at 21+ — and lets the
/// re-price adjudicate everything nearer.
///
/// Twelve slots is about five seconds. Revisit it with the data it is about to produce:
/// there is none above one slot yet, because the ceiling of one is what stopped it
/// being collected.
const MAX_EXECUTABLE_SLOT_SPREAD: u64 = 12;

/// Whether a cycle's legs were priced too far apart in time to be worth trading.
#[must_use]
fn too_skewed_to_trade(slot_spread: u64) -> bool {
    slot_spread > MAX_EXECUTABLE_SLOT_SPREAD
}

/// What this trade will actually pay in tips, in USD.
///
/// Zero while [`PAYS_A_TIP`] is false, which is the honest answer for a transaction
/// that carries no tip instruction. The contested arithmetic is kept beside it rather
/// than deleted, because the day a tip is attached is the day it becomes correct again.
#[must_use]
fn tip_cost_usd(gross_profit_usd: f64, sol_price_usd: f64) -> f64 {
    if !PAYS_A_TIP {
        return 0.0;
    }
    if gross_profit_usd > CONTESTED_USD {
        gross_profit_usd * CONTESTED_TIP_SHARE
    } else {
        JITO_TIP_FLOOR_SOL * sol_price_usd
    }
}

/// The most a route may cost in fees and still be worth spending an attempt on.
///
/// This is the competition model, and it is measured rather than argued. Every cycle
/// that reached route-building over a fifteen-hour live run was re-priced against
/// fresh state moments later, and the two numbers sorted almost perfectly by the
/// route's own fee:
///
/// | round-trip fee | attempts | median re-priced edge | still positive |
/// |----------------|----------|-----------------------|----------------|
/// | under 4 bps    | 26       | −1.69 bps             | 6 (23%)        |
/// | 4 – 7 bps      | 47       | −4.50 bps             | 0              |
/// | 7 – 10 bps     | 25       | −10.77 bps            | 0              |
/// | 20 – 40 bps    | 37       | −11.44 bps            | 2 (5%)         |
/// | over 40 bps    | 22       | −17.30 bps            | 0              |
///
/// The mechanism is not subtle. A route that costs 3 bps needs a 3 bp disagreement,
/// and disagreements that small are constantly available because nobody else can
/// profit from them either. A route that costs 30 bps needs a 30 bp disagreement,
/// which on a liquid pair is a transient somebody faster has already taken — and on an
/// illiquid one is not a disagreement at all but a dead pool whose price has drifted
/// and stayed there, priced correctly by both venues and arbitraged by neither.
///
/// Both look identical at detection. Only the fee separates them in advance, and with
/// one attempt per sweep to spend, spending it on the 23% class instead of the 0%
/// class is the whole difference between measuring this market and trading it.
/// # What this number is still not allowed to assume
///
/// The table above is 157 attempts, and the 4-7 bps row is 47 of them with no survivor.
/// Zero out of 47 is consistent with a survival rate anywhere up to about 6%, which at
/// the grosses that band carries would be worth having — and the 20-40 bps row of the
/// same table did produce two. So this is a reasonable prior, not a settled fact, and it
/// rejects thousands of candidates an hour on the strength of it.
///
/// A candidate it rejects is never built, so it is never re-priced, so the claim that it
/// would have lost can never be checked. `Intent::Measure` closes that loop: rejected
/// candidates are built and simulated anyway, at no cost and with no path to submission,
/// and the verdicts land in the ledger beside the fee that rejected them. Re-derive this
/// constant from those rather than from argument.
const EXECUTABLE_FEE_CEILING_BPS: f64 = 4.0;

/// Whether a cycle's round trip costs more than any real dislocation ever pays for.
#[must_use]
fn too_expensive_to_trade(fee_bps: f64) -> bool {
    fee_bps > EXECUTABLE_FEE_CEILING_BPS
}

/// How far behind the feed's own newest slot a cycle's stalest leg may sit.
///
/// [`MAX_EXECUTABLE_SLOT_SPREAD`] bounds the gap *between* two legs. It says nothing
/// about how far the pair together has fallen behind the chain, and those are different
/// failures: two pools both quoted from four minutes ago have a spread of zero and a
/// price from four minutes ago. `MAX_STALE_LAG_SLOTS` is the only thing bounding the
/// second, and it admits 1800 slots — twelve minutes.
///
/// # The measurement that produced this number
///
/// Over one eleven-hour live run, every recorded detection was compared against the
/// newest slot the feed itself held at that instant:
///
/// | stalest leg behind the head | detections | share |
/// |-----------------------------|------------|-------|
/// | 0 – 1 slots                 | 21,457     | 24.0% |
/// | 2 – 5                       | 19,146     | 21.4% |
/// | 6 – 12                      | 16,094     | 18.0% |
/// | 13 – 60                     | 16,254     | 18.2% |
/// | 61 – 300                    | 11,581     | 13.0% |
/// | 301 – 1800                  |  4,835     |  5.4% |
///
/// Median six slots, ninetieth percentile 121, worst 573. Three quarters of everything
/// this instrument has ever called an opportunity was priced from state the instrument
/// already knew was behind.
///
/// # Why that is fatal rather than merely untidy
///
/// The same run re-priced 2,201 of those candidates against accounts fetched in one
/// round trip, and the answer was always the same shape: the round trip came back
/// **3.64 bps short** of its own input against a route fee of **3.00 bps**. Subtract
/// one from the other and the dislocation that motivated the trade is 0.37 bps, with a
/// quartile range of −0.96 to +1.84. On fresh, self-consistent state these venues agree.
/// The disagreement was the lag.
///
/// Survival at re-pricing was 2.6%, and it is **flat** in every dimension that would
/// have to vary if the edges were real and merely decaying: flat in detected edge
/// (0.6% at 20+ bps against 4.7% at 2-4 bps), flat in fee band, flat in slot spread.
/// A decay process does not look like that. Selection on one's own measurement error
/// does, because conditioning on a large apparent edge selects the largest errors.
///
/// Two slots is one round trip's worth of head start and no more.
const MAX_EXECUTABLE_LEG_LAG_SLOTS: u64 = 2;

// The per-pool guard admits 1800 slots, which is the whole reason this gate exists. If
// it is ever tightened below this one, this gate has become dead code and the argument
// above it is naming the wrong number as load-bearing.
const _: () = assert!(live::MAX_STALE_LAG_SLOTS > MAX_EXECUTABLE_LEG_LAG_SLOTS);

/// Whether a cycle was priced from state the feed had already superseded.
#[must_use]
fn too_stale_to_trade(newest_slot: u64, cycle_slot: u64) -> bool {
    // A cycle from a slot *ahead* of the sweep's own is not stale; it is a snapshot
    // read while an update landed. Saturating rather than signed for that reason.
    newest_slot.saturating_sub(cycle_slot) > MAX_EXECUTABLE_LEG_LAG_SLOTS
}

/// How many cycles one sweep may *attempt*, as distinct from how many it may submit.
///
/// # Why these were ever the same number, and why they must not be
///
/// The rule used to be one attempt per sweep, for a reason that is still correct:
/// submitting several transactions against overlapping pools inside one slot has each
/// one invalidate the next. But that hazard belongs to *submitting*. An attempt that
/// gets refused re-prices against fresh accounts, decides the edge is gone and stops —
/// it signs nothing, sends nothing, and cannot invalidate anything. Conflating the two
/// spent the sweep's whole budget on the first candidate even when that candidate was
/// refused a few milliseconds later for a reason that said nothing about the rest.
///
/// The cost was measured over the live-armed run: **768 candidates** that were inside
/// every gate — fee at or under 4 bps, legs within 12 slots, net edge in the 1-10 bps
/// band where every one of the twelve submissions to date has come from — were turned
/// away with "another cycle took this sweep's one attempt". At the 0.6% conversion
/// measured in that band that is roughly 4.6 submissions never made, against 12 made,
/// and each submission carries a positive expectation of about $0.00027.
///
/// Three rather than more because attempts are sequential and each costs three or four
/// RPC round trips, near 400ms: the third candidate is being re-priced about 0.8s after
/// detection, and the price moves around 0.27 bps in two seconds, so the staleness this
/// adds stays well inside the floor's tolerance. A larger number would not.
const MAX_ATTEMPTS_PER_SWEEP: usize = 3;

/// How many candidates one sweep may *measure* without trading them.
///
/// A probe simulates and stops. It cannot submit, cannot spend and cannot trip the
/// breaker, so the only thing it competes for is time — one RPC round trip that a real
/// candidate might have wanted. One per sweep keeps that cost bounded while still
/// gathering, over an hour of sweeping, several hundred observations of the thing this
/// bot most needs to know: whether the routes its filters reject would have worked.
const MAX_PROBES_PER_SWEEP: usize = 1;


/// How long a cycle refused because *the price moved* is left alone. Two sweeps: long
/// enough that another cycle gets the next attempt, short enough to come back while
/// the gap may still be open. See `refusal_cooldown_for`.
///
/// This was 1500 ms, which is seven sweeps, and it was the single largest filter on
/// the trades worth having: of 2,494 simultaneous, encodable, cheap-fee detections
/// over a fifteen-hour run, 1,056 were suppressed by it. That was the right window
/// when a refusal meant a reverted transaction and a breaker strike. It stopped being
/// the right window when `hops_for` began re-pricing against fresh state *before*
/// building anything: a refusal now costs one RPC round trip and nothing else, so
/// waiting out a gap that is still open is the more expensive mistake.
const PRICE_MOVED_COOLDOWN: Duration = Duration::from_millis(400);
/// How long a cycle refused for a reason *the market cannot change* is left alone —
/// a size that will not fit the wallet refuses identically until the wallet moves.
const STRUCTURAL_COOLDOWN: Duration = Duration::from_secs(20);
/// How long an *entry mint* the wallet cannot fund is left alone.
///
/// Keyed on the mint rather than the cycle, because that is what the fact is about.
/// The wallet holds SOL and a few thousand base units of USDC; a cycle entered at USDC
/// is unfundable no matter which loop it belongs to, and it stays unfundable until a
/// balance moves rather than until a price does.
const UNFUNDABLE_ENTRY_WINDOW: Duration = Duration::from_secs(20);
/// How often the same refusal may reach the log, tracked separately from how often it
/// may be retried. Retrying a cycle four times a second is correct; saying so four
/// times a second is not.
const LOG_COOLDOWN: Duration = Duration::from_secs(20);

/// Where the measurement goes. The dashboard is a window; this is the record.
const LEDGER_PATH: &str = "cryptobot.db";

/// Where the rolling record of which mints attempts needed is kept, so a restart does
/// not forget a day of it. See `demand`.
const DEMAND_PATH: &str = "token-demand.json";
/// Where the address of this wallet's lookup table is remembered between runs.
const LOOKUP_PATH: &str = "lookup-table.json";
/// The most addresses the lookup table may grow to. At today's rent each costs about
/// 160,000 lamports of refundable deposit, so this bounds it near 0.02 SOL.
const LOOKUP_TABLE_CAP: usize = 120;
/// At most this many table extensions in any hour; each is a setup transaction that
/// holds the sweep while it confirms.
const MAX_LOOKUP_GROWS_PER_HOUR: usize = 6;

fn read_lookup_path() -> Option<solana_sdk::pubkey::Pubkey> {
    let text = std::fs::read_to_string(LOOKUP_PATH).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["address"].as_str()?.parse().ok()
}

fn write_lookup_path(key: &solana_sdk::pubkey::Pubkey) {
    let body = serde_json::json!({ "address": key.to_string() }).to_string();
    if let Err(e) = std::fs::write(LOOKUP_PATH, body) {
        tracing::warn!("could not record lookup table {key} in {LOOKUP_PATH}: {e}");
    }
}
/// How often that record is saved, and idle accounts are looked for.
const DEMAND_SAVE_INTERVAL: Duration = Duration::from_secs(300);
/// At most this many accounts opened mid-run in any hour. Each is a refundable deposit
/// and a base fee; the cap is what stops a burst of one-off detections from turning
/// into a burst of opens.
const MAX_ACCOUNT_OPENS_PER_HOUR: usize = 6;
/// At most this many idle accounts closed per save tick, so one tick cannot block the
/// sweep for long.
const MAX_ACCOUNT_CLOSES_PER_TICK: usize = 2;
/// How long an idle account that could not be closed — it holds a balance, or the close
/// did not confirm — is left before it is tried again.
const KEPT_OPEN_RECHECK: Duration = Duration::from_secs(6 * 3_600);

/// Record that an attempt needed each intermediate mint of `plan` that is not a base
/// or pinned one. See `demand`.
fn note_demand(
    demand: &mut demand::Demand,
    plan: &execute::CyclePlan,
    net_usd: f64,
    protected: &std::collections::HashSet<cb_core::types::Pubkey32>,
) {
    let now = now_ms() / 1_000;
    let inner = plan.mints.len().saturating_sub(1);
    for m in plan.mints.iter().take(inner).skip(1) {
        if !protected.contains(m) {
            demand.record(now, m, net_usd);
        }
    }
}

/// Open — or swap in — the account an attempt on `plan` was just refused for, once
/// the mint has been asked for often enough to earn one. See `demand::on_missing`.
///
/// Blocks the sweep while the open confirms, usually a second or two. Rare by
/// construction (`MAX_ACCOUNT_OPENS_PER_HOUR`), and the reconcile pass repairs any
/// feed drift a long wait lets in.
async fn rotate_for(
    t: &mut execute::Trader,
    plan: &execute::CyclePlan,
    demand: &demand::Demand,
    protected: &std::collections::HashSet<cb_core::types::Pubkey32>,
    slots: usize,
    opens: &mut std::collections::VecDeque<std::time::Instant>,
) {
    let Some(mint) = t.missing_account(plan) else { return };
    if protected.contains(&mint) {
        return;
    }
    opens.retain(|at| at.elapsed() < Duration::from_secs(3_600));
    if opens.len() >= MAX_ACCOUNT_OPENS_PER_HOUR {
        return;
    }
    let managed = t.managed_accounts(protected);
    let Some(mv) = demand::on_missing(demand, &managed, slots, &mint) else { return };
    opens.push_back(std::time::Instant::now());
    let (open, close) = match mv {
        demand::Move::Open(o) => (o, None),
        demand::Move::Replace { open, close } => (open, Some(close)),
    };
    let name = |m: &cb_core::types::Pubkey32| bs58::encode(m).into_string();
    if let Some(c) = close {
        match t.close_account(&c).await {
            Ok(true) => tracing::warn!(
                "closed the account for {} to make room for {}, which attempts ask for more",
                name(&c),
                name(&open)
            ),
            // Kept because it holds a balance, or a dry run: no slot was freed.
            Ok(false) => return,
            Err(e) => {
                tracing::warn!("could not close {} to make room: {e:#}", name(&c));
                return;
            }
        }
    }
    let score = demand.scores().get(&open).copied().unwrap_or_default();
    match t.open_account(&open).await {
        Ok(true) => tracing::warn!(
            "opened a token account for {}: asked for {} times in the last day, ${:.4} \
             expected between them. The deposit comes back when the account is closed.",
            name(&open),
            score.asks,
            score.usd
        ),
        Ok(false) => tracing::info!("the account for {} was not opened (dry run)", name(&open)),
        Err(e) => tracing::warn!("could not open an account for {}: {e:#}", name(&open)),
    }
}
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

/// How long a cycle refused for `reason` should be left alone before retrying.
///
/// `None` means do not hold it off at all.
///
/// The distinction this draws is the whole point. A refusal that depends on *this
/// instant's price* - a simulation that came back `TooLittleOutputReceived`, a net that
/// landed under the floor - says nothing about the next slot, and slots are ~400ms. A
/// refusal that depends on the wallet's *shape* - a size that cannot fit the balance
/// once rent is reserved - refuses identically until something outside the market
/// changes, and re-deriving it once a sweep is a wasted round trip to every endpoint.
///
/// This started life as one flat 20s window for both, which measured badly: over one
/// 3.9h live run, **1,586 of 2,064 detections** of cycles that were genuinely
/// encodable, simultaneous and net-positive were skipped by a cooldown earned by a
/// price that had since moved. Twenty seconds is ~50 slots; the opportunities being
/// waited out live 0.1-5s. The log spam that motivated the window is throttled
/// separately now - how often a thing is *said* and how often it is *retried* were
/// never the same question.
#[must_use]
fn refusal_cooldown_for(reason: &str) -> Option<Duration> {
    // Global, not a fact about this cycle: it has its own announce throttle, and
    // holding every cycle seen during a halt would leave them all still held for the
    // window *after* the halt lifts, which is exactly when they should be retried.
    if reason.starts_with("trading is halted") {
        return None;
    }
    // Not a fact about the loop, so it earns the loop no hold at all — see
    // `refused_for_entry_mint`, which holds the *mint* instead.
    if refused_for_entry_mint(reason) {
        return None;
    }
    // Structural: true until the wallet or the limits change, not until the price does.
    let structural = reason.contains("would leave less than")
        || reason.contains("exceeds")
        || reason.contains("has no encoder");
    Some(if structural { STRUCTURAL_COOLDOWN } else { PRICE_MOVED_COOLDOWN })
}

/// Whether a refusal was about *which end of the loop was entered* rather than about
/// the loop.
///
/// This distinction is worth a function because `cycle_key` is deliberately invariant
/// to the entry point: one round trip appears in a sweep twice, once per direction,
/// under a single key. Holding that key because the USDC-entered rotation could not be
/// funded also holds the SOL-entered rotation, which could — so the one refusal that
/// names its own remedy ("the same loop entered from SOL can be funded") was silencing
/// the remedy for twenty seconds.
///
/// Measured over one 1h53m live run: 10 of 56 attempts were refused here. Each also
/// spent that sweep's single attempt and put its fundable twin on a structural hold.
#[must_use]
fn refused_for_entry_mint(reason: &str) -> bool {
    reason.contains("of a mint the wallet holds")
}

/// A cycle held off until its own deadline should be skipped rather than re-attempted.
///
/// Pure, so the window itself is testable without running a sweep. Each entry carries
/// the cooldown it earned - see [`refusal_cooldown_for`].
#[must_use]
fn on_refusal_cooldown(
    recent: &std::collections::HashMap<String, (std::time::Instant, Duration)>,
    cycle_key: &str,
) -> bool {
    recent.get(cycle_key).is_some_and(|(at, window)| at.elapsed() < *window)
}

#[cfg(test)]
mod refusal_cooldown_tests {
    use super::*;
    use std::time::Instant;

    const SHORT: Duration = Duration::from_millis(1500);
    const LONG: Duration = Duration::from_secs(20);

    #[test]
    fn a_cycle_refused_moments_ago_is_on_cooldown() {
        let mut recent = std::collections::HashMap::new();
        recent.insert("SOL-USDC-SOL".to_string(), (Instant::now(), LONG));
        assert!(on_refusal_cooldown(&recent, "SOL-USDC-SOL"));
    }

    #[test]
    fn a_cycle_never_refused_is_never_on_cooldown() {
        let recent = std::collections::HashMap::new();
        assert!(!on_refusal_cooldown(&recent, "anything"));
    }

    /// The whole point: once the window has genuinely elapsed, retrying resumes. A
    /// stuck cycle must eventually be re-checked, not silenced forever — the wallet's
    /// balance or the market's price may have moved since.
    #[test]
    fn a_cycle_refused_before_the_window_is_not_on_cooldown() {
        let mut recent = std::collections::HashMap::new();
        let long_ago = Instant::now() - Duration::from_secs(30);
        recent.insert("SOL-USDC-SOL".to_string(), (long_ago, LONG));
        assert!(!on_refusal_cooldown(&recent, "SOL-USDC-SOL"));
    }

    /// A different cycle sharing nothing with the refused one must be unaffected — the
    /// cooldown is per-loop, not a global "stop trying anything" switch.
    #[test]
    fn a_different_cycle_is_not_covered_by_anothers_cooldown() {
        let mut recent = std::collections::HashMap::new();
        recent.insert("SOL-USDC-SOL".to_string(), (Instant::now(), LONG));
        assert!(!on_refusal_cooldown(&recent, "SOL-USDT-SOL"));
    }

    /// Each entry is judged against the window it earned, not one shared constant. A
    /// price-moved refusal two seconds old is finished; a structural one is not.
    #[test]
    fn each_entry_expires_on_its_own_window() {
        let mut recent = std::collections::HashMap::new();
        let two_secs_ago = Instant::now() - Duration::from_secs(2);
        recent.insert("moved".to_string(), (two_secs_ago, SHORT));
        recent.insert("structural".to_string(), (two_secs_ago, LONG));
        assert!(!on_refusal_cooldown(&recent, "moved"), "1.5s window, 2s ago: over");
        assert!(on_refusal_cooldown(&recent, "structural"), "20s window, 2s ago: still held");
    }

    /// The measured failure this classifier exists for. Over one 3.9h live run, 1,586 of
    /// 2,064 detections of genuinely takeable cycles were skipped because a *price* had
    /// moved twenty seconds — fifty slots — earlier. Slippage rejections and profit-floor
    /// misses describe one instant and must come back quickly.
    #[test]
    fn a_price_that_moved_is_retried_far_sooner_than_a_wallet_that_cannot_fit_the_trade() {
        assert_eq!(
            refusal_cooldown_for("expected net $0.000073 is below the $0.000100 floor"),
            Some(PRICE_MOVED_COOLDOWN)
        );
        assert_eq!(
            refusal_cooldown_for(
                "wrapping 128808539 lamports would leave less than the 4178560 lamports \
                 this transaction needs for account rent and fees"
            ),
            Some(STRUCTURAL_COOLDOWN)
        );
        assert_eq!(
            refusal_cooldown_for("size $14.00 exceeds the $10.00 per-trade limit"),
            Some(STRUCTURAL_COOLDOWN)
        );
        // A balance in the wrong token does not become the right one because the price
        // moved — but the hold belongs on the *mint*, not on the loop, because the loop
        // is also on offer entered from SOL and shares this one's key.
        assert_eq!(
            refusal_cooldown_for(
                "this cycle starts by spending 9200000 of a mint the wallet holds 7904 of \
                 — the same loop entered from SOL can be funded, this one cannot"
            ),
            None,
            "holding the cycle key here also holds the rotation that can be funded"
        );
        // But an edge that cannot cover the fee is a fact about the price, and the
        // price is the thing most likely to have changed by the next sweep.
        assert_eq!(
            refusal_cooldown_for(
                "the widest floor this edge allows leaves 3100 base units between what the \
                 last hop delivers and what it guarantees, and the transaction fee needs \
                 10000 of that"
            ),
            Some(PRICE_MOVED_COOLDOWN)
        );
        assert!(PRICE_MOVED_COOLDOWN < STRUCTURAL_COOLDOWN);
    }

    /// The entry-mint refusal is recognised on the wording the executor actually
    /// emits, and nothing else is mistaken for it — in particular the *other* balance
    /// refusal, which is about SOL and is genuinely structural for the whole loop.
    #[test]
    fn only_the_entry_mint_refusal_is_treated_as_an_entry_mint_refusal() {
        assert!(refused_for_entry_mint(
            "this cycle starts by spending 9540740 of a mint the wallet holds 0 of — the \
             same loop entered from SOL can be funded, this one cannot"
        ));
        assert!(!refused_for_entry_mint(
            "wrapping 128808539 lamports would leave less than the 4178560 lamports this \
             transaction needs for account rent and fees"
        ));
        assert!(!refused_for_entry_mint(
            "this route spends 17770165 and guarantees only 17764611 back — signing it \
             would authorise a loss"
        ));
    }

    /// A halt is one fact about the gate, not about any cycle. Holding cycles for it
    /// would leave every one of them still held for the window *after* the halt lifts,
    /// which is exactly the moment they should all be retried.
    #[test]
    fn a_halt_holds_no_individual_cycle_off() {
        assert_eq!(refusal_cooldown_for("trading is halted: 6 trades failed in a row"), None);
    }
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
        halt_cooldown_secs: cfg.halt_cooldown_secs,
    };
    limits.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::info!(
        "risk limits from config.toml: max position ${:.2}, min net ${:.4}, daily loss ${:.2}, \
         {} consecutive failures, {} trades/day, halt cooldown {}s{}",
        limits.max_position_usd,
        limits.min_net_profit_usd,
        limits.max_daily_loss_usd,
        limits.max_consecutive_failures,
        limits.max_daily_trades,
        limits.halt_cooldown_secs,
        if limits.halt_cooldown_secs == 0 { " (auto-resume disabled — a halt needs a restart)" } else { "" }
    );
    let opts = execute::TradeOptions {
        slippage_tenth_bps: cfg.slippage_tenth_bps,
        priority_micro_lamports: cfg.priority_micro_lamports,
        // Priority is charged on the limit *requested*, not the units consumed, so this
        // number is a price as much as a safety margin. 400,000 was the conservative
        // guess made before anything had landed.
        //
        // Measured since, on chain rather than on ours: across 16,036 successful
        // transactions in 15 consecutive blocks, the closed-loop arbitrages that
        // actually profited consumed a median of 179,138 compute units, with a
        // ninetieth percentile of 255,880 and a maximum of 282,466. Our own reverted
        // attempt burned 77,938 getting one hop in, including the account creations.
        //
        // 300,000 sits above every winner in that sample and buys a third more priority
        // for the same lamports. Raising it again is cheap to justify and cheap to do;
        // a transaction that runs out of units pays its fee and reverts, so the failure
        // is bounded but it is not free.
        compute_units: 300_000,
        dry_run: cfg.dry_run,
        create_token_accounts: true,
        wsol: cb_executor::route::WsolPolicy::WrapAndClose,
        submit: match cfg.submit_via {
            SubmitVia::Rpc => execute::Submit::Rpc,
            SubmitVia::Jito => execute::Submit::Jito {
                tip_max_lamports: cfg.jito_tip_max_lamports,
                simulate_first: cfg.jito_simulate_first,
            },
        },
    };
    let exec = cb_executor::Executor::new(wallet, rpc, limits, cfg.dry_run)?;

    if cfg.dry_run {
        tracing::warn!(
            "LIVE ARMED, DRY RUN: {address} will build and simulate real transactions and \
             submit none. Set dry_run = false in config.toml to spend."
        );
    } else {
        tracing::error!(
            "LIVE ARMED, SUBMITTING: {address} will sign and send real transactions. {}",
            if cfg.submit_via == SubmitVia::Jito && !cfg.jito_simulate_first {
                "Trades go to Jito unsimulated: the on-chain floors guarantee the fee and \
                 tip, and a trade that misses them is dropped for free."
            } else {
                "Every trade is still simulated first and abandoned unless the simulated \
                 balance clears the profit floor."
            }
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

    let mut trader = execute::Trader::new(exec, opts);
    trader.set_jito_url(&cfg.jito_url);
    match opts.submit {
        execute::Submit::Jito { tip_max_lamports, simulate_first } => tracing::info!(
            "trades go to Jito as bundles of one: a missed floor is dropped and costs \
             nothing, and every floor guarantees the base fee and the tip on chain — tip a \
             quarter of the gross, {} to {tip_max_lamports} lamports; {}",
            cb_executor::jito::MIN_TIP_LAMPORTS,
            if simulate_first {
                "each is still simulated first"
            } else {
                "sent without our own simulation, one round trip sooner"
            }
        ),
        execute::Submit::Rpc => tracing::warn!(
            "trades go through sendTransaction on the RPC: a floor missed by the time one \
             lands reverts it on chain and pays the whole fee (submit_via = \"jito\" \
             makes that free)"
        ),
    }
    // Which mints the classic token program does not own. Configuration, read once:
    // no swap changes a mint's owner, and getting it wrong derives the wrong
    // associated account rather than raising anything.
    {
        let reg = registry::Registry::load()?;
        let t22: Vec<_> =
            reg.mints.iter().filter(|(_, m)| m.token_2022).map(|(k, _)| *k).collect();
        if !t22.is_empty() {
            tracing::info!(
                "{} of {} mints belong to Token-2022 and will be traded through the venues' \
                 v2 swap instructions",
                t22.len(),
                reg.mints.len()
            );
        }
        trader.set_token_2022_mints(t22);
    }
    tracing::info!(
        "live executor armed for {} — {} bps slippage floor, {} priority",
        trader.address(),
        f64::from(cfg.slippage_tenth_bps) / 10.0,
        cfg.priority_micro_lamports
    );

    // Pay the rent for the accounts a cycle needs to hold, once, before any trade has
    // to. See `Trader::ensure_token_accounts` for why a missing one is not an
    // inconvenience but a wall: a trade that opens an account is asked to show a gain of
    // twenty-one cents on a cycle worth a tenth of one.
    //
    // Only the base mints, which is where every executable cycle starts and ends and is
    // the shortest list that unblocks all of them.
    // Retried, because it runs exactly once per run and everything downstream depends
    // on it. A single RPC hiccup here would leave the wallet without an account it
    // needs and every cycle through that mint quietly failing its profit check for the
    // rest of the session — which is precisely the failure this call exists to end.
    // Idempotent, so a retry after a send that actually succeeded finds nothing to do.
    // Rent is a chain parameter and it has moved: 2,039,280 lamports for a token account
    // when this was written, 1,488,440 by 2026-09-25. Read it rather than remember it.
    match trader.learn_account_rent().await {
        Ok(r) => tracing::info!("a token account deposits {r} lamports of rent at today's rate"),
        Err(e) => tracing::warn!("could not read the current rent ({e:#}); using {}", trader.account_rent()),
    }
    // A PumpSwap hop needs the program's fee recipients, and a purchase needs the
    // wallet's volume accumulator to exist already. Only when the universe has one.
    if registry::Registry::load()?.pools.iter().any(|p| p.dex == cb_core::types::Dex::PumpSwap) {
        if let Err(e) = trader.load_pump_fees().await {
            tracing::warn!("could not read PumpSwap's fee recipients ({e:#}); PumpSwap hops will be refused");
        }
        match trader.ensure_pump_volume_accumulator().await {
            Ok(true) => tracing::warn!("created the wallet's PumpSwap volume accumulator (rent, returned if closed)"),
            Ok(false) => {}
            Err(e) => tracing::warn!("could not create the PumpSwap volume accumulator ({e:#}); PumpSwap purchases will fail their floors"),
        }
    }
    // Once per start, before any trade: can this path land a bundle at all? See
    // `Trader::jito_probe`.
    match trader.jito_probe().await {
        Ok(Some(true)) => tracing::warn!(
            "Jito self-test LANDED: the send path works, so a trade bundle that is not \
             included lost its race or its floor, not its way"
        ),
        Ok(Some(false)) => tracing::error!(
            "Jito self-test was NOT included within 30 s: a bundle with no floor and no race \
             did not land, so the send path itself is failing (region, tip, or bundle format)"
        ),
        Ok(None) => {}
        Err(e) => tracing::warn!("Jito self-test could not be sent: {e:#}"),
    }
    // The lookup table from earlier runs, if one was built. See `cb_executor::alt`.
    if let Some(key) = read_lookup_path() {
        match trader.load_lookup(key).await {
            Ok(n) => tracing::info!("using lookup table {key}: {n} addresses"),
            Err(e) => tracing::warn!("could not load lookup table {key} ({e:#}); a new one will be built if needed"),
        }
    }
    let mut base_mints = registry::Registry::load()?.base_mints;
    // The base mints are only where cycles *start*. A loop through an intermediate mint
    // the wallet has no account for cannot pass its profit check at all, because the
    // rent it would have to pay inside the trade is two hundred times the gain — so the
    // mints named in `extra_token_mints` are appended here rather than being a separate
    // call, and `MAX_ACCOUNTS_TO_OPEN` caps the whole list together.
    for m in &cfg.extra_token_mints {
        match registry::pk(m) {
            Ok(k) if !base_mints.contains(&k) => base_mints.push(k),
            Ok(_) => {}
            // Named but unreadable is worth saying out loud: the operator asked for this
            // mint to be reachable and it silently will not be.
            Err(e) => tracing::error!("extra_token_mints: {m} is not a public key ({e}); skipped"),
        }
    }
    for attempt in 1..=3u32 {
        match trader.ensure_token_accounts(&base_mints).await {
            Ok(_) => break,
            Err(e) if attempt == 3 => {
                // Not fatal. A book that can still trade its existing accounts is worth
                // more than a process that refuses to start, and the log says exactly
                // what is missing so it can be fixed deliberately.
                tracing::error!(
                    "could not open the token accounts this book needs, after {attempt} \
                     attempts: {e:#}. Cycles through any mint the wallet has no account for \
                     will keep failing their profit check by the price of the rent."
                );
            }
            Err(e) => {
                tracing::warn!("opening token accounts failed ({e:#}); retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    // Now the rest of the book. Not to open — opening every mint in the registry would
    // be a deliberate spend of 1.68% of this wallet each — but to know, so a cycle
    // through a mint we cannot hold is refused in one line instead of two round trips
    // and an unexplained balance shortfall.
    let all_mints: Vec<_> = registry::Registry::load()?.mints.keys().copied().collect();
    match trader.learn_token_accounts(&all_mints).await {
        Ok(missing) if missing.is_empty() => {
            tracing::info!("the wallet holds an account for every mint in the book");
        }
        Ok(missing) => {
            let cost = missing.len() as u64 * trader.account_rent();
            tracing::warn!(
                "{} of {} mints have no token account, so cycles through them are refused \
                 rather than attempted. Opening all of them would deposit {cost} lamports \
                 of rent; name the ones worth it in extra_token_mints.",
                missing.len(),
                all_mints.len()
            );
        }
        Err(e) => {
            tracing::warn!(
                "could not read which token accounts exist ({e:#}); cycles through a mint \
                 the wallet cannot hold will be discovered the slow way instead"
            );
        }
    }
    Ok(trader)
}


/// A trade that has been sent and whose fate the chain has not told us yet.
///
/// # Why this exists
///
/// The send path used to wait for the answer in line — three status reads two seconds
/// apart, then a status call to the block engine — which stopped the whole loop for
/// five seconds or more after every send: no feed updates applied, no sweeps, no
/// attempts, at exactly the moment the market was moving. Now the send is recorded as
/// awaiting an answer, the loop carries on, and a one-second timer asks once per
/// pending send until it lands, reverts, or has plainly not been included.
struct PendingSend {
    sig: String,
    sent_at: std::time::Instant,
    /// The ledger row written when it was sent, updated with the outcome.
    row: Option<i64>,
    via_jito: bool,
}

/// Ask once about every pending send old enough to have an answer and log the outcome
/// in the words the desk classifies. Returns `(ledger row, taken, reason, net USD)` for
/// each one settled, for the caller to write: the ledger is not `Sync`, so it is not
/// held across the awaits here.
async fn settle_pending(
    t: &mut execute::Trader,
    pending: &mut Vec<PendingSend>,
    sol_price: f64,
) -> Vec<(i64, bool, String, f64)> {
    /// A block and a bit: asking sooner mostly hears "not yet".
    const FIRST_LOOK: Duration = Duration::from_secs(2);
    /// A bundle is valid for a few slots; after this it is not coming.
    const GIVE_UP: Duration = Duration::from_secs(8);
    let mut keep = Vec::new();
    let mut settled = Vec::new();
    for p in std::mem::take(pending) {
        if p.sent_at.elapsed() < FIRST_LOOK {
            keep.push(p);
            continue;
        }
        let sig = &p.sig;
        let (taken, reason, net) = match t.confirm(sig, 1).await {
            Some(true) => {
                tracing::error!("LANDED {sig} — the transaction confirmed on chain");
                (true, "submitted and landed".to_string(), 0.0)
            }
            Some(false) => {
                // A revert pays the whole fee and returns nothing; the daily budget has
                // to see it.
                let cost_usd = t.submission_cost_lamports() as f64 / 1e9 * sol_price;
                t.settle(-cost_usd);
                tracing::warn!(
                    "{sig} landed and reverted — the floor was not met by the time it was \
                     included; this cost ${cost_usd:.6} and returned nothing"
                );
                (true, "submitted, landed, reverted".to_string(), -cost_usd)
            }
            None if p.sent_at.elapsed() < GIVE_UP => {
                keep.push(p);
                continue;
            }
            None if p.via_jito => {
                tracing::warn!(
                    "{sig} was not included — a Jito bundle that misses is dropped and costs \
                     nothing"
                );
                (false, "sent to Jito; not included, cost nothing".to_string(), 0.0)
            }
            None => {
                tracing::warn!("{sig} has not confirmed yet — not the same as failed; check it in an explorer");
                (false, "submitted; not yet confirmed".to_string(), 0.0)
            }
        };
        if let Some(row) = p.row {
            settled.push((row, taken, reason, net));
        }
    }
    *pending = keep;
    settled
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
    // `cb-bot --pools`: bootstrap the registry and the census watchlist from chain,
    // report which pools would be dropped and why, and exit. No feed, no wallet.
    if args.iter().any(|a| a == "--pools") {
        tracing_subscriber::fmt().with_ansi(false).with_target(false).init();
        let cfg = Config::load("config.toml")?;
        let registry = registry::Registry::load()?;
        let asked = registry.pools.len();
        let market = live::LiveMarket::bootstrap(&cfg.rpc_http_url, registry).await?;
        println!("{} of {asked} pools priceable", market.store_len());
        return Ok(());
    }
    if args.iter().any(|a| a == "--report") {
        let path = args.iter().position(|a| a == "--report").and_then(|i| args.get(i + 1));
        return report(path.map_or(LEDGER_PATH, String::as_str));
    }

    // The channel a connected dashboard's Log tab reads from in real time. Created
    // before the subscriber so the very first line logged already has somewhere live
    // to go; the receiving end is wired up once `bus` exists, a few lines below.
    let (log_tap_tx, log_tap_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cb_server=info,cb_feed=info".into()),
        )
        // No console window has existed to render colour in since cryptobot-desk
        // stopped giving this process one of its own; ANSI codes past that point are
        // pure noise sitting in the log file and in the Log tab's live preview.
        .with_ansi(false)
        .with_writer(live_log::TeeMakeWriter::new(log_tap_tx))
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
            // Empty, like every other default here: this fallback is the paper-mode
            // path, and paying rent is not something a run with no config should decide.
            extra_token_mints: Vec::new(),
            token_account_slots: 0,
            capital_usd: 100.0,
            fee_buffer_usd: 0.20,
            min_trade_usd: 10.0,
            max_hops: 3,
            slippage_tenth_bps: 300,
            priority_micro_lamports: 0,
            submit_via: SubmitVia::Jito,
            jito_url: String::new(),
            jito_tip_max_lamports: 20_000,
            jito_simulate_first: true,
            dry_run: true,
            max_position_usd: 25.0,
            max_daily_loss_usd: 5.0,
            min_net_profit_usd: 0.01,
            max_slippage_bps: 30.0,
            max_consecutive_failures: 3,
            max_daily_trades: 500,
            halt_cooldown_secs: 600,
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

    // The other end of the tap: turn every line the subscriber just wrote into a live
    // event on the same bus everything else uses, so the desk app sees it in the same
    // round trip as an Opportunity or a Status heartbeat rather than on its own timer.
    {
        let log_bus = bus.clone();
        let mut log_tap_rx = log_tap_rx;
        tokio::spawn(async move {
            while let Some(line) = log_tap_rx.recv().await {
                log_bus.publish(Event::LogLine { line, ts_ms: now_ms() });
            }
        });
    }
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
            // Said the way the run is actually configured: "simulated first" was true
            // until jito_simulate_first could be turned off, and a line that overstates
            // the safety net is the one line here that must not.
            let checked = match cfg.submit_via {
                SubmitVia::Rpc => {
                    "every one is simulated first and abandoned unless the simulated balance \
                     clears the profit floor"
                }
                SubmitVia::Jito if cfg.jito_simulate_first => {
                    "every one is simulated first, then sent to Jito with its profit floor \
                     guaranteed on chain"
                }
                SubmitVia::Jito => {
                    "each goes to Jito unsimulated, with its profit floor guaranteed on chain; \
                     a bundle that misses it is dropped"
                }
            };
            tracing::error!(
                "mode: LIVE, dry_run = false — transactions will be SUBMITTED: {checked}. \
                 Money can move from here"
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
    let registry = registry::Registry::load()?;

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

    // Accounts the rotation in `demand` must never close: where every cycle starts and
    // ends, and whatever the operator pinned by name.
    let mut protected: std::collections::HashSet<cb_core::types::Pubkey32> =
        registry::Registry::load()?.base_mints.into_iter().collect();
    for m in &cfg.extra_token_mints {
        if let Ok(k) = registry::pk(m) {
            protected.insert(k);
        }
    }
    let token_slots = cfg.token_account_slots;

    tokio::spawn(async move {
        // Owned by the sweep task, because the risk gate is per-run state and there is
        // exactly one place trades are decided. `None` in paper mode, and then no code
        // below can reach a signature no matter what the rest of the loop does.
        let mut trader = trader;
        let mut next_id: u64 = 1;
        // A cycle whose execution attempt was refused very recently, keyed by the
        // identity of the loop rather than by which mint it was entered at — the same
        // cycle re-detected a moment later is the same refusal, not a new question.
        //
        // Without this, a cycle whose sizing cannot fit the wallet's real balance (see
        // `execute::wrap_shortfall`) gets re-fetched, re-built and re-refused on every
        // sweep for as long as it keeps being the best thing on the board — a fresh
        // round trip to three RPC providers and an identical log line roughly once a
        // second, forever, learning nothing the first refusal did not already
        // establish. A live run did exactly this for over an hour before the cooldown
        // below existed.
        let mut recent_refusals: std::collections::HashMap<
            String,
            (std::time::Instant, Duration),
        > = std::collections::HashMap::new();
        let mut recent_logged: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
        // Mints the wallet has just been shown it cannot start a trade in. Held here
        // rather than in `recent_refusals` because the fact is about the mint: the
        // cycle key it arrived under covers the rotation that *can* be funded too.
        // See `refused_for_entry_mint` and `UNFUNDABLE_ENTRY_WINDOW`.
        let mut unfundable_entries: std::collections::HashMap<
            cb_core::types::Pubkey32,
            std::time::Instant,
        > = std::collections::HashMap::new();
        // The gate being halted, separately throttled from the per-cycle map above: it is
        // one fact about the whole run, not one fact per cycle, so it gets one shared
        // timestamp rather than a fresh cooldown entry for every distinct cycle that
        // happens to ask while it is true.
        let mut last_halt_announced: Option<std::time::Instant> = None;
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
        // Keeps the RPC and Jito connections a trade uses open, so a send does not
        // start with a handshake. See `Trader::keep_warm`.
        let mut warm_timer = tokio::time::interval(execute::KEEP_WARM_EVERY);
        // Sends whose fate is not known yet, and the timer that asks. See `PendingSend`.
        let mut pending_sends: Vec<PendingSend> = Vec::new();
        let mut confirm_timer = tokio::time::interval(Duration::from_secs(1));
        let mut drift = (0usize, 0usize);
        // Both are edge-triggered: logged when they change, not every sweep. A warning
        // that fires five times a second is a warning nobody reads.
        let mut was_stalled = false;
        let mut last_stale_excluded = 0usize;
        // Which intermediate mints attempts have needed over the last day. See `demand`.
        let demand_path = std::path::Path::new(DEMAND_PATH);
        let mut demand = demand::Demand::load(demand_path, now_ms() / 1_000);
        let mut demand_timer = tokio::time::interval(DEMAND_SAVE_INTERVAL);
        let mut account_opens: std::collections::VecDeque<std::time::Instant> =
            std::collections::VecDeque::new();
        let mut kept_open: std::collections::HashMap<cb_core::types::Pubkey32, std::time::Instant> =
            std::collections::HashMap::new();
        let mut lookup_grows: std::collections::VecDeque<std::time::Instant> =
            std::collections::VecDeque::new();

        loop {
            tokio::select! {
                _ = demand_timer.tick() => {
                    if let Err(e) = demand.save(demand_path, now_ms() / 1_000) {
                        tracing::warn!("could not save {DEMAND_PATH}: {e}");
                    }
                    // The priority order the rotation works from, on the record: which
                    // mints attempts asked for over the last day, most valuable first.
                    let ranking = demand.ranking();
                    if !ranking.is_empty() {
                        let held = trader.as_ref().map(|t| t.managed_accounts(&protected));
                        let line: Vec<String> = ranking
                            .iter()
                            .take(10)
                            .map(|(m, s)| {
                                let b = bs58::encode(m).into_string();
                                let mark = if held.as_ref().is_some_and(|h| h.contains(m)) {
                                    "held"
                                } else {
                                    "no account"
                                };
                                format!("{}… {} asks ${:.4} ({mark})", &b[..6], s.asks, s.usd)
                            })
                            .collect();
                        tracing::info!("token demand, last day: {}", line.join(" · "));
                    }
                    if let Some(t) = trader.as_mut() {
                        let managed = t.managed_accounts(&protected);
                        let idle = demand::idle(&demand, &managed, now_ms() / 1_000);
                        // An account left open for a balance is not retried every tick.
                        kept_open.retain(|_, at: &mut std::time::Instant| at.elapsed() < KEPT_OPEN_RECHECK);
                        let due: Vec<_> = idle.into_iter().filter(|m| !kept_open.contains_key(m)).collect();
                        for mint in due.into_iter().take(MAX_ACCOUNT_CLOSES_PER_TICK) {
                            match t.close_account(&mint).await {
                                Ok(true) => tracing::warn!(
                                    "closed an account no attempt asked for in a day, to trade its deposit instead"
                                ),
                                Ok(false) => { kept_open.insert(mint, std::time::Instant::now()); }
                                Err(e) => tracing::warn!("could not close an idle account: {e:#}"),
                            }
                        }
                    }
                }
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
                _ = warm_timer.tick() => {
                    if let Some(t) = trader.as_mut() {
                        t.keep_warm().await;
                    }
                }
                _ = confirm_timer.tick(), if !pending_sends.is_empty() => {
                    if let Some(t) = trader.as_mut() {
                        let sol_price = market.sol_price_usd().unwrap_or(0.0);
                        let outcomes = settle_pending(t, &mut pending_sends, sol_price).await;
                        if let Some(l) = ledger.as_ref() {
                            for (row, taken, reason, net) in outcomes {
                                if let Err(e) = l.update_fill_outcome(row, taken, &reason, net) {
                                    tracing::warn!("could not record a send's outcome: {e}");
                                }
                            }
                        }
                    }
                }
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
                    // Ahead of anything else this sweep does: a halt clears itself on
                    // its own cooldown, and a sweep that finds no candidate to attempt
                    // must not be the reason that takes longer to notice.
                    if let Some(t) = trader.as_mut() {
                        t.tick_auto_resume();
                    }
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
                                "feed silent for over {FEED_STALL_SECS}s — pausing the ledger; \
                                 sweeps continue but nothing is recorded"
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
                    // Both reset every sweep: this is a budget per pass, not per run.
                    //
                    // Two counters rather than one flag, because attempting and
                    // submitting are different acts with different hazards. See
                    // [`MAX_ATTEMPTS_PER_SWEEP`].
                    let mut attempts_this_sweep: usize = 0;
                    let mut probes_this_sweep: usize = 0;
                    let mut submitted_this_sweep = false;
                    // Left in the order the scanner produced: best gross profit first.
                    //
                    // Re-examined against every attempt the ledger can label, because the
                    // obvious improvement here is wrong and the next reader should not
                    // have to rediscover that. 2,362 attempts split cleanly: 254 built a
                    // route whose floor beat its input and reached simulation, 2,108 died
                    // at the re-price. Scoring each candidate ordering by how well it puts
                    // the survivors first — AUC, 0.5 being a coin flip:
                    //
                    // | ordering                                   | AUC   |
                    // |--------------------------------------------|-------|
                    // | gross profit — what the scanner already does| 0.729 |
                    // | gross, net edge over 10 bps pushed to back  | 0.700 |
                    // | same, thresholds at 5 / 8 / 15 / 25 bps     | 0.673-0.705 |
                    // | detected net edge alone                     | 0.461 |
                    // | cheapest fee first                          | 0.517 |
                    //
                    // A seven-feature logistic regression on the same labels, fitted on
                    // the first 70% and scored on the last 30%, reaches 0.788 held out and
                    // still loses to plain gross.
                    //
                    // The tempting story was that a very large detected edge is an
                    // artefact — a concentrated pool quoting a price no size can trade at.
                    // It is not: attempts above 10 bps of net edge reach simulation 9.2%
                    // of the time against 11.0% below it. The apparent cliff came from
                    // counting *submissions*, of which there have only ever been twelve;
                    // at that base rate the high-edge group would be expected to yield
                    // about 1.3, so observing none says nothing.
                    //
                    // Rank by gross. Do not rank by edge — that is worse than a coin flip.
                    //
                    // Read before the move, because `too_stale_to_trade` needs the head
                    // the sweep was taken against and the sweep is about to be consumed.
                    let sweep_slot = sweep.slot;
                    let opportunities = sweep.opportunities;
                    // Bounded, the same way the event bus is: a cycle refused once and
                    // never seen again must not sit in this map for the life of the
                    // process. Five times the cooldown is generous headroom for a cycle
                    // that is still actively being retried; anything older than that is
                    // history, not state.
                    recent_refusals.retain(|_, (at, window)| at.elapsed() < *window * 5);
                    recent_logged.retain(|_, at| at.elapsed() < LOG_COOLDOWN * 5);
                    unfundable_entries.retain(|_, at| at.elapsed() < UNFUNDABLE_ENTRY_WINDOW);
                    for opp in opportunities {
                        let id = next_id;
                        next_id += 1;

                        // Uncontested cycles pay the tip floor; contested ones get bid
                        // up to most of the profit. A cycle worth more than a cent on
                        // a major pair will have been seen by faster searchers too.
                        // Still recorded, because "big enough that somebody faster has
                        // seen this too" remains a true and useful fact about a cycle.
                        // It is no longer charged as a cost — see `tip_cost_usd`.
                        let contested = opp.gross_profit_usd > CONTESTED_USD;
                        let est_tip_usd = tip_cost_usd(opp.gross_profit_usd, sol_price);
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
                        let mut pending_new: Option<PendingSend> = None;

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
                                    // would have each invalidate the next. And not a cycle
                                    // that was already refused within the cooldown window —
                                    // see `recent_refusals`'s own comment for why.
                                    let on_cooldown =
                                        on_refusal_cooldown(&recent_refusals, &opp.cycle_key);
                                    // Legs priced too far apart in time are not an
                                    // opportunity, they are the clock.
                                    //
                                    // `slot_spread` has been measured on every cycle
                                    // since the artifact was first found, and `--report`
                                    // has a whole section arguing that an edge which
                                    // grows with the gap between two observations was
                                    // never simultaneously on offer. It was never wired
                                    // to execution, and the omission was worse than
                                    // neutral: candidates are taken best-gross-first, and
                                    // gross grows with skew, so the selection walked
                                    // straight up the artifact every sweep. One live run
                                    // measured it — mean detected edge by spread band,
                                    // 1.27 bps at 0-1 slots against 3.42 bps at 21+ — and
                                    // all 26 of that run's attempts came from the skewed
                                    // bands, none from the simultaneous one. Every one was
                                    // refused for a loss once re-priced against fresh
                                    // state, by 0.3 to 4.7 bps.
                                    let too_skewed = too_skewed_to_trade(opp.slot_spread);
                                    // A route whose own fee is larger than any
                                    // disagreement that survives the trip to the chain.
                                    // See `EXECUTABLE_FEE_CEILING_BPS` for the measured
                                    // table this comes from.
                                    let too_expensive = too_expensive_to_trade(opp.fee_bps);
                                    // Priced from state the feed had already moved past.
                                    // `too_skewed` cannot see this: both legs can be
                                    // equally old, which is a spread of zero and a price
                                    // from minutes ago. See `MAX_EXECUTABLE_LEG_LAG_SLOTS`
                                    // for the eleven-hour measurement that says three
                                    // quarters of this instrument's detections are this,
                                    // and that what they are worth on fresh state is
                                    // 0.37 bps against a 3.00 bps fee.
                                    let too_stale = too_stale_to_trade(sweep_slot, opp.slot);
                                    // Read before the filter so the refusal can say
                                    // *which* venue is missing an encoder, instead of
                                    // leaving it pooled with "somebody else went first".
                                    let blocked_by = opp.plan.as_ref().and_then(
                                        execute::CyclePlan::blocking_venue,
                                    );
                                    // A mint the wallet was just shown it cannot start a
                                    // trade in. Cheap here, and the alternative is two
                                    // RPC round trips to be told the same thing by the
                                    // balance the executor fetches.
                                    let unfundable_entry = opp
                                        .plan
                                        .as_ref()
                                        .and_then(|pl| pl.mints.first())
                                        .is_some_and(|m| {
                                            unfundable_entries.get(m).is_some_and(|at| {
                                                at.elapsed() < UNFUNDABLE_ENTRY_WINDOW
                                            })
                                        });
                                    // Everything that disqualifies a candidate for
                                    // reasons that have nothing to do with its fee.
                                    // A shape this trader can never send, whatever its price —
                                    // a USDC-start cycle under Jito, a venue it cannot re-price.
                                    // Refused inside the attempt too, but only after it had taken
                                    // one of the sweep's few attempt slots: 363 times in one run,
                                    // each crowding out a cycle that could have been sent.
                                    let unsendable =
                                        opp.plan.as_ref().is_some_and(|pl| !t.could_execute(pl));
                                    let barred = submitted_this_sweep
                                        || attempts_this_sweep >= MAX_ATTEMPTS_PER_SWEEP
                                        || on_cooldown
                                        || unfundable_entry
                                        || unsendable;
                                    // The two filters that reject a candidate by making a
                                    // claim about what would have happened to it, rather
                                    // than by observing something that already has. Both
                                    // were calibrated on samples of a few dozen, both
                                    // reject thousands of candidates an hour, and neither
                                    // has been rechecked since. An unchecked claim is an
                                    // assumption wearing a measurement's clothes.
                                    let held_back_by_a_claim =
                                        too_expensive || too_skewed || too_stale;
                                    // A candidate the fee ceiling is the *only* thing
                                    // standing between and an attempt. This is the class
                                    // the ceiling has been rejecting sight-unseen — 30,262
                                    // of them in one seven-hour run, 7,659 of those showing
                                    // a detected gross over two cents — and the ceiling's
                                    // own evidence is thin: it rests on 47 attempts in the
                                    // 4-7 bps band, which is consistent with a survival
                                    // rate anywhere up to about 6%. At the grosses this
                                    // band carries, 6% would be worth having. Measure it.
                                    let worth_measuring = !barred
                                        && held_back_by_a_claim
                                        && probes_this_sweep < MAX_PROBES_PER_SWEEP;
                                    // Under Jito the measurement *is* the trade. A probe
                                    // re-prices on fresh state and simulates, then stops;
                                    // an attempt does exactly the same and then sends a
                                    // bundle whose floor guarantees the fee and the tip on
                                    // chain, and which costs nothing if it misses. The
                                    // claims above were written to save the fee a failed
                                    // RPC send paid — under Jito there is no such fee left
                                    // to save, and holding them to a probe only means a
                                    // route that simulates profitably is written down
                                    // instead of taken. That happened on 2026-09-23 at
                                    // 13:09: a probe cleared its floor by 8,958 lamports
                                    // and was logged, not sent.
                                    //
                                    // Same budget as the probe it replaces, so the RPC load
                                    // does not change.
                                    let promoted = worth_measuring
                                        && t.sends_via_jito()
                                        && opp.plan.as_ref().is_some_and(execute::CyclePlan::encodable);
                                    if promoted {
                                        probes_this_sweep += 1;
                                    }
                                    let candidate = if barred || (held_back_by_a_claim && !promoted) {
                                        None
                                    } else {
                                        opp.plan.as_ref().filter(|pl| pl.encodable())
                                    };
                                    let to_measure = if worth_measuring && !promoted {
                                        opp.plan.as_ref().filter(|pl| pl.encodable())
                                    } else {
                                        None
                                    };
                                    match candidate {
                                        None => {
                                            if let Some(probe_plan) = to_measure {
                                                probes_this_sweep += 1;
                                                if t.could_execute(probe_plan) {
                                                    note_demand(&mut demand, probe_plan, net, &protected);
                                                }
                                                let started = std::time::Instant::now();
                                                let r = t
                                                    .attempt(
                                                        probe_plan,
                                                        opp.size_usd,
                                                        net,
                                                        execute::Intent::Measure,
                                                    )
                                                    .await;
                                                latency_ms =
                                                    started.elapsed().as_millis() as u64;
                                                outcome_reason = Some(match r {
                                                    Ok(cb_executor::Attempt::Probed(found)) => {
                                                        // A filter being wrong is the most
                                                        // valuable thing this run can find,
                                                        // and it is invisible in a file that
                                                        // is otherwise all refusals. Logged
                                                        // at ERROR for the same reason a
                                                        // submission is.
                                                        if found.would_have_profited() {
                                                            tracing::error!(
                                                                fee_bps = opp.fee_bps,
                                                                slot_spread = opp.slot_spread,
                                                                gross_usd = opp.gross_profit_usd,
                                                                "a filter held back a \
                                                                 route that simulated \
                                                                 profitably — {found}"
                                                            );
                                                        }
                                                        found.to_string()
                                                    }
                                                    // `Measure` cannot come back any other
                                                    // way, but the refusals on the way in —
                                                    // no encoder, unfundable entry — are
                                                    // still worth keeping verbatim.
                                                    Ok(cb_executor::Attempt::Refused(why)) => {
                                                        format!("probe not run: {why}")
                                                    }
                                                    Ok(other) => {
                                                        format!("probe returned {other:?}")
                                                    }
                                                    Err(e) => format!("probe failed: {e}"),
                                                });
                                                // One measurement of a cycle is enough for a
                                                // while; the budget is better spent on the
                                                // next distinct loop than on this one again
                                                // next sweep.
                                                recent_refusals.insert(
                                                    opp.cycle_key.clone(),
                                                    (std::time::Instant::now(), STRUCTURAL_COOLDOWN),
                                                );
                                            } else {
                                            outcome_reason = Some(if on_cooldown {
                                                "not attempted — refused recently and \
                                                 nothing has changed"
                                                    .to_string()
                                            } else if too_stale {
                                                format!(
                                                    "not attempted — the stalest leg is {} \
                                                     slots behind the feed's own head, over \
                                                     the {MAX_EXECUTABLE_LEG_LAG_SLOTS} this \
                                                     will trade on; on fresh state this class \
                                                     is worth 0.37 bps against a 3.00 bps fee",
                                                    sweep_slot.saturating_sub(opp.slot)
                                                )
                                            } else if too_skewed {
                                                format!(
                                                    "not attempted — legs priced {} slots \
                                                     apart, over the {MAX_EXECUTABLE_SLOT_SPREAD} \
                                                     this will trade on; the gap is the clock, \
                                                     not a disagreement between venues",
                                                    opp.slot_spread
                                                )
                                            } else if too_expensive {
                                                format!(
                                                    "not attempted — the round trip costs \
                                                     {:.2} bps, over the \
                                                     {EXECUTABLE_FEE_CEILING_BPS} bps that have \
                                                     produced every survivor so far; this sweep \
                                                     had no probe budget left to check whether \
                                                     that still holds",
                                                    opp.fee_bps
                                                )
                                            } else if unfundable_entry {
                                                "not attempted — the wallet cannot start a \
                                                 trade in this cycle's entry mint; the same \
                                                 loop entered from SOL is not held by this"
                                                    .to_string()
                                            } else if unsendable && blocked_by.is_none() {
                                                "not attempted — this shape cannot be sent: under Jito only a \
                                                 cycle that starts and ends in SOL has its fee guaranteed on \
                                                 chain, and a binned pool leaves room for two hops"
                                                    .to_string()
                                            } else if let Some(dex) = blocked_by {
                                                // Its own message. Pooled with the
                                                // sweep budget these were unreadable,
                                                // and the two ask for opposite things:
                                                // one is a queue, the other is an
                                                // encoder somebody has to write.
                                                format!(
                                                    "not attempted — {} has no encoder",
                                                    dex.name()
                                                )
                                            } else {
                                                format!(
                                                    "not attempted — this sweep's {} attempts \
                                                     went to other cycles",
                                                    MAX_ATTEMPTS_PER_SWEEP
                                                )
                                            });
                                            }
                                        }
                                        Some(plan) => {
                                            attempts_this_sweep += 1;
                                            if t.could_execute(plan) {
                                                note_demand(&mut demand, plan, net, &protected);
                                            }
                                            let started = std::time::Instant::now();
                                            let r = t
                                                .attempt(
                                                    plan,
                                                    opp.size_usd,
                                                    net,
                                                    execute::Intent::Trade,
                                                )
                                                .await;
                                            // A loss on the fresh read means the feed's copy of at
                                            // least one of these pools is behind the chain. Re-read
                                            // them now, so the next sweep does not detect the same
                                            // phantom through a different partner venue.
                                            if let Ok(cb_executor::Attempt::Refused(why)) = &r {
                                                if why.contains("guarantees only")
                                                    || why.contains("the floor would let through")
                                                {
                                                    let ids: Vec<_> =
                                                        plan.pools.iter().map(|(p, _)| *p).collect();
                                                    if let Err(e) = market.refresh(&ids).await {
                                                        tracing::debug!("could not re-read a stale cycle: {e:#}");
                                                    }
                                                }
                                            }
                                            // Cleared its floor and did not fit a packet: put what it
                                            // names into the lookup table, so the next attempt fits.
                                            if let Some(wanted) = t.take_oversize() {
                                                lookup_grows.retain(|at| at.elapsed() < Duration::from_secs(3_600));
                                                if lookup_grows.len() < MAX_LOOKUP_GROWS_PER_HOUR && !wanted.is_empty() {
                                                    lookup_grows.push_back(std::time::Instant::now());
                                                    match t.grow_lookup(wanted, LOOKUP_TABLE_CAP).await {
                                                        Ok(Some(key)) => write_lookup_path(&key),
                                                        Ok(None) => {}
                                                        Err(e) => tracing::warn!("could not grow the lookup table: {e:#}"),
                                                    }
                                                }
                                            }
                                            if matches!(r, Ok(cb_executor::Attempt::Refused(_))) {
                                                rotate_for(
                                                    t,
                                                    plan,
                                                    &demand,
                                                    &protected,
                                                    token_slots,
                                                    &mut account_opens,
                                                )
                                                .await;
                                            }
                                            latency_ms = started.elapsed().as_millis() as u64;
                                            match r {
                                                Ok(cb_executor::Attempt::Submitted {
                                                    signature: sig,
                                                    bundle_id,
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
                                                    // The sweep is finished here whatever
                                                    // its attempt budget had left. This is
                                                    // the hazard the old one-attempt rule
                                                    // existed for: a second transaction
                                                    // against an overlapping pool in the
                                                    // same slot invalidates the first.
                                                    submitted_this_sweep = true;
                                                    tracing::error!("submitted {sig}");
                                                    signature = Some(sig.clone());
                                                    // What became of it is asked by the
                                                    // confirm timer, off this loop: waiting
                                                    // here left the bot blind to the market
                                                    // for five seconds after every send. See
                                                    // `PendingSend`.
                                                    let _ = bundle_id;
                                                    outcome_reason = Some(if t.sends_via_jito() {
                                                        "sent to Jito; awaiting inclusion".to_string()
                                                    } else {
                                                        "submitted; awaiting confirmation".to_string()
                                                    });
                                                    pending_new = Some(PendingSend {
                                                        sig: sig.clone(),
                                                        sent_at: std::time::Instant::now(),
                                                        row: None,
                                                        via_jito: t.sends_via_jito(),
                                                    });
                                                }
                                                Ok(cb_executor::Attempt::SimulationRejected {
                                                    reason,
                                                    ..
                                                }) => {
                                                    // A rejection here is the chain saying the
                                                    // price moved between the quote and the
                                                    // simulation — the most transient fact
                                                    // there is, and no reason to stop watching
                                                    // this cycle for fifty slots. Held off
                                                    // briefly so another cycle gets this
                                                    // sweep's one attempt, and logged on the
                                                    // long window so a cycle failing all
                                                    // minute does not fill the log.
                                                    let quiet = recent_logged
                                                        .get(&opp.cycle_key)
                                                        .is_some_and(|at: &std::time::Instant| {
                                                            at.elapsed() < LOG_COOLDOWN
                                                        });
                                                    if !quiet {
                                                        tracing::warn!(
                                                            "simulation rejected: {reason}"
                                                        );
                                                        recent_logged.insert(
                                                            opp.cycle_key.clone(),
                                                            std::time::Instant::now(),
                                                        );
                                                    }
                                                    outcome_reason =
                                                        Some(format!("simulation: {reason}"));
                                                    recent_refusals.insert(
                                                        opp.cycle_key.clone(),
                                                        (
                                                            std::time::Instant::now(),
                                                            PRICE_MOVED_COOLDOWN,
                                                        ),
                                                    );
                                                }
                                                Ok(cb_executor::Attempt::Refused(why)) => {
                                                    // Logged at info, not debug. A live run
                                                    // that refuses everything must say so in
                                                    // the log the operator actually reads;
                                                    // at debug this was invisible and looked
                                                    // like nothing happening at all. Logged
                                                    // once per cooldown window rather than
                                                    // once per sweep — see `recent_refusals`.
                                                    //
                                                    // "Halted" is the one exception: it is a
                                                    // fact about the gate, not about this
                                                    // cycle, so a *different* cycle found next
                                                    // sweep would otherwise re-announce it under
                                                    // its own, separate cooldown entry. Shared
                                                    // with the ERROR-level announce below so
                                                    // both move together.
                                                    let is_halt_refusal =
                                                        why.starts_with("trading is halted");
                                                    let quiet = recent_logged
                                                        .get(&opp.cycle_key)
                                                        .is_some_and(|at: &std::time::Instant| {
                                                            at.elapsed() < LOG_COOLDOWN
                                                        });
                                                    let should_log = if is_halt_refusal {
                                                        last_halt_announced.is_none_or(
                                                            |at: std::time::Instant| {
                                                                at.elapsed() >= LOG_COOLDOWN
                                                            },
                                                        )
                                                    } else {
                                                        !quiet
                                                    };
                                                    if should_log {
                                                        tracing::info!("refused: {why}");
                                                        if !is_halt_refusal {
                                                            recent_logged.insert(
                                                                opp.cycle_key.clone(),
                                                                std::time::Instant::now(),
                                                            );
                                                        }
                                                    }
                                                    outcome_reason =
                                                        Some(format!("refused: {why}"));
                                                    // Nothing was signed and nothing was sent:
                                                    // the executor read a balance and stopped.
                                                    // Charging the sweep's single attempt for
                                                    // that hands the slot to nobody, when the
                                                    // rotation of this very loop that *can* be
                                                    // funded is further down the same list.
                                                    // The mint is held instead, so the round
                                                    // trip is not repeated either.
                                                    if refused_for_entry_mint(&why) {
                                                        if let Some(m) = plan.mints.first() {
                                                            unfundable_entries.insert(
                                                                *m,
                                                                std::time::Instant::now(),
                                                            );
                                                        }
                                                        attempts_this_sweep =
                                                            attempts_this_sweep.saturating_sub(1);
                                                    }
                                                    // A halt earns no per-cycle hold: it is one
                                                    // fact about the gate, and holding every
                                                    // cycle seen during it would leave them all
                                                    // still held for the window *after* the halt
                                                    // lifts — precisely when they should be
                                                    // retried.
                                                    if let Some(window) = refusal_cooldown_for(&why)
                                                    {
                                                        recent_refusals.insert(
                                                            opp.cycle_key.clone(),
                                                            (std::time::Instant::now(), window),
                                                        );
                                                    }
                                                }
                                                // `Intent::Trade` has no path that
                                                // produces one, and the day it does the
                                                // right answer is not to pretend a
                                                // measurement was a trade.
                                                Ok(cb_executor::Attempt::Probed(found)) => {
                                                    tracing::error!(
                                                        "a trade attempt came back as a \
                                                         measurement, which should be \
                                                         impossible: {found}"
                                                    );
                                                    outcome_reason =
                                                        Some(format!("unexpected {found}"));
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
                                            // The gate being halted is a global state, not
                                            // a fact about this one cycle, so it does not
                                            // fit the per-cycle-key cooldown above — every
                                            // distinct cycle the sweep finds would otherwise
                                            // re-announce the same halt once each. Throttled
                                            // on its own timer instead: announced once, then
                                            // at most once more per cooldown window for as
                                            // long as it stays true, however many different
                                            // cycles keep asking in between.
                                            if let Some(why) = t.halted() {
                                                let should_announce = last_halt_announced
                                                    .is_none_or(|at: std::time::Instant| {
                                                        at.elapsed() >= LOG_COOLDOWN
                                                    });
                                                if should_announce {
                                                    tracing::error!("trading halted: {why}");
                                                    last_halt_announced =
                                                        Some(std::time::Instant::now());
                                                }
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
                                leg_lag_slots: Some(sweep_slot.saturating_sub(opp.slot)),
                                // Zero means nothing was attempted, and that is a
                                // different fact from a fast attempt — so it is
                                // recorded as "not measured" rather than as speed.
                                latency_ms: (latency_ms > 0).then_some(latency_ms),
                            };
                            match l.record_fill(&rec) {
                                Ok(id) => {
                                    if let Some(p) = pending_new.as_mut() {
                                        p.row = Some(id);
                                    }
                                }
                                Err(e) => tracing::warn!("could not record fill: {e}"),
                            }
                        }
                        if let Some(p) = pending_new.take() {
                            pending_sends.push(p);
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
                    ";\n    {} more predate the ladder and are left out rather than counted as \
                     zero",
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

    // Whether the prices were the *current* ones, which is a different question from
    // whether they were simultaneous and is the one the table below cannot ask.
    let lag = ledger.lag_audit()?;
    let lag_measured: u64 = lag.iter().map(|b| b.fills).sum();
    if lag_measured > 0 {
        println!("\n  WERE THEY THE PRICES THE CHAIN HAD, OR THE ONES WE STILL HELD?");
        println!(
            "    {:<14} {:>9} {:>8} {:>10} {:>10} {:>10}",
            "behind head", "detections", "share", "mean edge", "mean gross", "survived"
        );
        for b in &lag {
            if b.fills == 0 {
                continue;
            }
            println!(
                "    {:<14} {:>9} {:>7.1}% {:>10.2} {:>10} {:>10}",
                b.label,
                b.fills,
                b.fills as f64 / lag_measured as f64 * 100.0,
                b.mean_edge_bps,
                format!("${:.5}", b.mean_gross_usd),
                b.built
            );
        }
        println!(
            "\n    `survived` is the only column that is not a claim: it counts the loops\n    \
             that still paid once both legs were re-read in one round trip. A band with\n    \
             a healthy mean edge and no survivors is not an opportunity that got away.\n    \
             It is this instrument reading its own lag back to itself — the older price\n    \
             it is differencing against has already gone, and nobody was ever offered it."
        );
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
    let registry = registry::Registry::load()?;
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

    println!("\n  {checked} checked, {skipped} skipped, {faults} faults, {off_premise} \
              off-premise");
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
            "the staleness guard ({guard_ms} ms) must outlast two reconciles ({reconcile_ms} \
             ms each), or it excludes pools reconcile has already proven correct and quietly \
             shrinks the search space"
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

#[cfg(test)]
mod leg_lag_gate_tests {
    use super::{too_skewed_to_trade, too_stale_to_trade};

    /// State from the head, or one round trip behind it, is what this admits.
    #[test]
    fn state_from_the_head_is_tradeable() {
        assert!(!too_stale_to_trade(500, 500), "the head itself is never stale");
        assert!(!too_stale_to_trade(500, 498), "two slots is the head start we allow");
    }

    /// A snapshot read while an update landed can carry a slot ahead of the sweep's
    /// own. That is the freshest state there is, and subtraction must not wrap it into
    /// the largest staleness representable.
    #[test]
    fn a_leg_ahead_of_the_sweep_is_not_maximally_stale() {
        assert!(!too_stale_to_trade(500, 503));
    }

    /// The percentile points the eleven-hour run measured. The median detection sat six
    /// slots behind the head and the ninetieth sat 121; both classes re-priced to a loss.
    #[test]
    fn the_bands_that_measured_as_lag_are_excluded() {
        for behind in [3u64, 6, 12, 30, 121, 476, 573] {
            assert!(
                too_stale_to_trade(1_000_000, 1_000_000 - behind),
                "{behind} slots behind the head re-priced to a loss and must not trade"
            );
        }
    }

    /// The two gates answer different questions, and this is the case that proves it:
    /// both legs equally old is a spread of zero — perfectly simultaneous — and a price
    /// from minutes ago. Nothing in `too_skewed_to_trade` can see it.
    #[test]
    fn simultaneous_is_not_the_same_as_current() {
        let spread = 0;
        assert!(!too_skewed_to_trade(spread), "both legs from one slot straddle nothing");
        assert!(
            too_stale_to_trade(1_000_000, 1_000_000 - 600),
            "and that one slot can still be four minutes old"
        );
    }

}

#[cfg(test)]
mod slot_spread_gate_tests {
    use super::too_skewed_to_trade;

    /// Both legs from one slot is the case this exists to admit, and one slot of
    /// straddle must not be thrown away with the artifact.
    #[test]
    fn simultaneous_legs_are_tradeable() {
        assert!(!too_skewed_to_trade(0), "both legs from one slot is the case to trade");
        assert!(!too_skewed_to_trade(1), "a boundary straddle is not an artifact");
    }

    /// The bands a live run actually measured, and what they were worth. Mean detected
    /// edge rose with the gap between the two observations — 1.27 bps at 0-1 slots,
    /// 2.09 at 6-20, 3.42 at 21+ — which is the market moving, not two venues
    /// disagreeing. Every attempt that run made came from the skewed bands, and every
    /// one was refused for a loss once re-priced against fresh state.
    ///
    /// This fails if the ceiling is ever loosened back over those bands.
    #[test]
    fn the_bands_that_measured_as_artifact_are_excluded() {
        for spread in [13u64, 17, 21, 27, 100, 465] {
            assert!(
                too_skewed_to_trade(spread),
                "{spread} slots of skew is inside the ceiling — that band measured as \
                 the clock, not as an edge"
            );
        }
    }

    /// The near bands are let through to be adjudicated by re-pricing, not by the clock.
    ///
    /// A ceiling of one slot rejected 109 of the ~151 candidates that cleared the
    /// economics gate in a thirty-nine-minute run, while two reached the chain. Most of
    /// what it rejected was two Raydium CLMM pools eight or nine slots apart, at a time
    /// when `reconcile` was finding one to three pools of seventy-nine drifted and often
    /// none — so the prices were right and the gap was a pool nobody had swapped.
    #[test]
    fn a_quiet_pool_is_no_longer_mistaken_for_a_stale_one() {
        for spread in [0u64, 1, 4, 5, 8, 9, 12] {
            assert!(
                !too_skewed_to_trade(spread),
                "{spread} slots is inside the window the fresh re-price should judge, \
                 not the clock"
            );
        }
    }
}

#[cfg(test)]
mod fee_ceiling_tests {
    use super::{too_expensive_to_trade, tip_cost_usd, CONTESTED_USD};

    /// The fee bands a live run measured, and what each was worth once re-priced
    /// against fresh state. Only the cheapest band ever produced a survivor.
    ///
    /// | round-trip fee | attempts | median re-priced | still positive |
    /// |----------------|----------|------------------|----------------|
    /// | under 4 bps    | 26       | −1.69 bps        | 6              |
    /// | 4 – 7 bps      | 47       | −4.50 bps        | 0              |
    /// | 7 – 10 bps     | 25       | −10.77 bps       | 0              |
    /// | over 40 bps    | 22       | −17.30 bps       | 0              |
    #[test]
    fn only_the_band_that_ever_survived_is_worth_an_attempt() {
        for cheap in [0.0, 1.0, 2.0, 3.0, 4.0] {
            assert!(
                !too_expensive_to_trade(cheap),
                "{cheap} bps is the band that produced every survivor"
            );
        }
        for dear in [4.5, 6.0, 9.0, 30.0, 104.0] {
            assert!(
                too_expensive_to_trade(dear),
                "{dear} bps is a band where no attempt has ever re-priced profitably"
            );
        }
    }

    /// A cost that is never paid must not be charged.
    ///
    /// No tip instruction exists anywhere in the executor, and while that is true the
    /// only honest tip estimate is zero. The median believable opportunity here grosses
    /// $0.00044 against a tip floor of $0.00077, so charging it refused trades on
    /// nearly twice the cost the wallet actually bears.
    #[test]
    fn a_tip_that_is_never_attached_is_never_charged() {
        let sol = 102.7;
        assert!((tip_cost_usd(0.0004, sol)).abs() < f64::EPSILON, "uncontested pays nothing");
        assert!(
            (tip_cost_usd(CONTESTED_USD * 10.0, sol)).abs() < f64::EPSILON,
            "and neither does a contested cycle, for the same reason: there is no \
             instruction for the tip to ride in"
        );
    }
}
