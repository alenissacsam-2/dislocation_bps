//! The gate every trade passes through before it can be signed.
//!
//! This is the part of the system that assumes the rest of it is wrong. Every other
//! module tries to be correct; this one tries to bound how expensive being incorrect
//! gets. It is pure logic with no I/O so that all of it is testable, and every refusal
//! carries a reason the operator can read.
//!
//! # The ordering matters
//!
//! Checks run cheapest-and-most-fatal first. A tripped breaker is not a per-trade
//! judgement — it means something is systematically wrong — so it is answered before
//! anything bothers evaluating the trade on its merits.
//!
//! # What this cannot do
//!
//! It cannot stop a loss on a trade that has already been sent. Solana has no
//! cancellation, and once a bundle is submitted the outcome is the chain's to decide.
//! Everything here is about the decision to submit, which is the only moment there is
//! any control at all.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Operator-set bounds. Every one of these is a hard stop, not a target.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    /// Largest notional a single trade may carry.
    pub max_position_usd: f64,
    /// Cumulative realised loss for the day at which trading stops entirely.
    pub max_daily_loss_usd: f64,
    /// Consecutive failed or reverted trades before the breaker trips.
    pub max_consecutive_failures: u32,
    /// Expected net profit below which a trade is not worth its own risk.
    ///
    /// Not zero. A trade expected to net a hundredth of a cent is indistinguishable
    /// from noise in the estimate that produced it, and the estimate is what is being
    /// trusted when the transaction is signed.
    pub min_net_profit_usd: f64,
    /// How far the simulated result may fall short of the quote before it is refused.
    pub max_slippage_bps: f64,
    /// Upper bound on trades per day, whatever they are worth.
    pub max_daily_trades: u32,
    /// How long a breaker trip stands before the gate clears it on its own.
    ///
    /// Zero disables this: the halt then stands until an operator calls
    /// [`RiskGate::resume`], exactly as before this field existed. A live run found
    /// this halt had no reachable resume path at all — nothing in the app ever called
    /// `resume()`, so a trip was in practice permanent until the whole process was
    /// restarted. A cooldown is not a claim that whatever tripped it is fixed; it is
    /// only a bound on how long the instrument sits idle before trying again.
    pub halt_cooldown_secs: u64,
}

impl Default for Limits {
    /// Deliberately timid. These are the numbers someone gets if they never open the
    /// settings, and the cost of them being too tight is a missed trade, while the cost
    /// of them being too loose is a drained wallet.
    fn default() -> Self {
        Self {
            max_position_usd: 25.0,
            max_daily_loss_usd: 5.0,
            max_consecutive_failures: 3,
            min_net_profit_usd: 0.01,
            max_slippage_bps: 30.0,
            max_daily_trades: 500,
            halt_cooldown_secs: 600,
        }
    }
}

impl Limits {
    /// Returns the reason the limits are unusable, phrased for the UI to print verbatim.
    ///
    /// # Errors
    /// If any bound is negative, non-finite, or a combination that can never trade.
    pub fn validate(&self) -> Result<(), String> {
        let finite = |v: f64, name: &str| -> Result<(), String> {
            if !v.is_finite() || v < 0.0 {
                return Err(format!("{name} must be a positive number."));
            }
            Ok(())
        };
        finite(self.max_position_usd, "Maximum position")?;
        finite(self.max_daily_loss_usd, "Maximum daily loss")?;
        finite(self.min_net_profit_usd, "Minimum net profit")?;
        finite(self.max_slippage_bps, "Maximum slippage")?;
        if self.max_position_usd <= 0.0 {
            return Err("Maximum position must be greater than zero.".into());
        }
        if self.max_daily_loss_usd <= 0.0 {
            return Err(
                "Maximum daily loss must be greater than zero, or nothing can ever trade.".into(),
            );
        }
        if self.max_consecutive_failures == 0 {
            return Err("Consecutive failures before stopping must be at least 1.".into());
        }
        if self.max_daily_trades == 0 {
            return Err("Maximum daily trades must be at least 1.".into());
        }
        Ok(())
    }
}

/// What the gate decided, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refused for this trade only; later trades may still pass.
    Refuse(String),
    /// Refused and trading is over until the operator intervenes.
    Halt(String),
}

impl Decision {
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::Refuse(r) | Self::Halt(r) => Some(r),
        }
    }
}

/// A trade as proposed, before anything irreversible has happened.
#[derive(Debug, Clone, Copy)]
pub struct Proposal {
    pub size_usd: f64,
    /// Net of every cost the caller can estimate: fees, tip, base fee.
    pub expected_net_usd: f64,
}

/// How a submitted trade turned out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// Landed on chain. `net_usd` may be negative — landing is not the same as winning.
    Landed { net_usd: f64 },
    /// Did not land: lost the race, expired, or was dropped. Costs nothing but time.
    Missed,
    /// Landed and reverted, or was rejected for a reason that suggests a defect
    /// rather than competition. These are what trip the breaker.
    Failed,
}

/// Running state for one trading day.
#[derive(Debug, Clone)]
pub struct RiskGate {
    limits: Limits,
    realised_usd: f64,
    consecutive_failures: u32,
    trades_today: u32,
    halted: Option<String>,
    /// When the current halt started, for [`Self::tick_auto_resume`] to measure
    /// against. `None` whenever `halted` is `None`.
    halted_at: Option<Instant>,
}

impl RiskGate {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            realised_usd: 0.0,
            consecutive_failures: 0,
            trades_today: 0,
            halted: None,
            halted_at: None,
        }
    }

    /// Stop trading immediately and stay stopped until [`Self::resume`] is called, or
    /// until [`Self::tick_auto_resume`] finds the configured cooldown has elapsed.
    ///
    /// The operator's kill switch, and also what the gate does to itself when a limit
    /// is breached.
    pub fn halt(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        if self.halted.is_none() {
            tracing::error!("trading halted: {reason}");
            self.halted = Some(reason);
            self.halted_at = Some(Instant::now());
        }
    }

    /// Why trading is stopped, if it is.
    #[must_use]
    pub fn halted(&self) -> Option<String> {
        self.halted.clone()
    }

    /// Clear a halt. Requires an explicit act, and says so in the log.
    pub fn resume(&mut self) {
        if let Some(prev) = self.halted.take() {
            tracing::warn!("trading resumed by operator; was halted for: {prev}");
        }
        self.halted_at = None;
        self.consecutive_failures = 0;
    }

    /// Clear a halt on its own once it has stood for the configured cooldown.
    ///
    /// Cheap to call on every sweep: it is a no-op unless a halt is both active and
    /// older than `limits.halt_cooldown_secs`. A cooldown of zero disables this
    /// entirely, leaving `resume()` as the only way out — the pre-cooldown behaviour.
    ///
    /// This does not mean whatever tripped the breaker is confirmed fixed; it only
    /// bounds how long the instrument sits idle before it is allowed to find out by
    /// trying again. The next trade is judged on its own merits like any other.
    pub fn tick_auto_resume(&mut self) {
        if self.limits.halt_cooldown_secs == 0 {
            return;
        }
        let Some(at) = self.halted_at else { return };
        if at.elapsed() >= Duration::from_secs(self.limits.halt_cooldown_secs) {
            if let Some(prev) = self.halted.take() {
                tracing::warn!(
                    "trading resumed automatically after a {}s cooldown; was halted for: {prev}",
                    self.limits.halt_cooldown_secs
                );
            }
            self.halted_at = None;
            self.consecutive_failures = 0;
        }
    }

    /// A new day: the daily counters reset, a halt does not.
    ///
    /// A breaker that survived midnight was tripped by something that midnight did not
    /// fix.
    pub fn roll_day(&mut self) {
        self.realised_usd = 0.0;
        self.trades_today = 0;
    }

    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.halted.is_some()
    }

    #[must_use]
    pub fn halt_reason(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    #[must_use]
    pub fn realised_usd(&self) -> f64 {
        self.realised_usd
    }

    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    #[must_use]
    pub fn trades_today(&self) -> u32 {
        self.trades_today
    }

    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Decide whether a proposal may proceed to signing.
    #[must_use]
    pub fn check(&self, p: &Proposal) -> Decision {
        if let Some(r) = &self.halted {
            return Decision::Halt(format!("trading is halted: {r}"));
        }

        // A NaN reaching arithmetic that is compared against a limit silently passes
        // every `>` test. Refuse the trade rather than the comparison.
        if !p.size_usd.is_finite() || !p.expected_net_usd.is_finite() {
            return Decision::Refuse("trade has a non-finite size or profit estimate".into());
        }

        if self.realised_usd <= -self.limits.max_daily_loss_usd {
            return Decision::Halt(format!(
                "daily loss limit reached: {:.4} of {:.2} allowed",
                -self.realised_usd, self.limits.max_daily_loss_usd
            ));
        }

        if self.consecutive_failures >= self.limits.max_consecutive_failures {
            return Decision::Halt(format!(
                "{} consecutive failures — something is wrong that retrying will not fix",
                self.consecutive_failures
            ));
        }

        if self.trades_today >= self.limits.max_daily_trades {
            return Decision::Refuse(format!(
                "daily trade cap reached ({})",
                self.limits.max_daily_trades
            ));
        }

        if p.size_usd <= 0.0 {
            return Decision::Refuse("trade size is not positive".into());
        }

        if p.size_usd > self.limits.max_position_usd {
            return Decision::Refuse(format!(
                "size ${:.2} exceeds the ${:.2} per-trade limit",
                p.size_usd, self.limits.max_position_usd
            ));
        }

        if p.expected_net_usd < self.limits.min_net_profit_usd {
            return Decision::Refuse(format!(
                "expected net ${:.6} is below the ${:.6} floor",
                p.expected_net_usd, self.limits.min_net_profit_usd
            ));
        }

        Decision::Allow
    }

    /// Fold a completed trade into the running state.
    ///
    /// A `Missed` trade is not a failure. Losing a race is the normal outcome of racing
    /// and says nothing about whether the machinery works — counting it toward the
    /// breaker would stop trading on a busy day rather than a broken one.
    pub fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Landed { net_usd } => {
                if net_usd.is_finite() {
                    self.realised_usd += net_usd;
                }
                self.trades_today += 1;
                self.consecutive_failures = 0;
            }
            Outcome::Missed => {
                self.consecutive_failures = 0;
            }
            Outcome::Failed => {
                self.trades_today += 1;
                self.consecutive_failures += 1;
            }
        }

        if self.realised_usd <= -self.limits.max_daily_loss_usd {
            self.halt(format!(
                "daily loss limit of ${:.2} reached",
                self.limits.max_daily_loss_usd
            ));
        }
        if self.consecutive_failures >= self.limits.max_consecutive_failures {
            self.halt(format!("{} trades failed in a row", self.consecutive_failures));
        }
    }

    /// Fold a *confirmed* profit or loss into a trade that has already been recorded.
    ///
    /// # Why this exists separately from [`RiskGate::record`]
    ///
    /// A submission has to be counted the moment it is sent, because `max_daily_trades`
    /// is the cap on how much this bot may spend and a transaction in flight is already
    /// spending. But its P&L is not known then — the chain has not answered — so it is
    /// booked at zero, and until now it stayed at zero forever. That left
    /// `max_daily_loss_usd` measuring a number nothing ever wrote to: the one limit
    /// denominated in money could not see money.
    ///
    /// That was survivable only because a submission cost a flat ~6,000 lamports and the
    /// separate `max_daily_trades` cap bounded the day at around $0.30. It stops being
    /// survivable the moment the priority bid varies with the opportunity, because then
    /// the cost of a reverted trade varies with it too.
    ///
    /// So: this adds to the realised figure and re-checks the loss limit, and
    /// deliberately touches neither `trades_today` nor `consecutive_failures` — the
    /// trade was already counted, and counting it twice would halve the daily cap.
    pub fn settle(&mut self, net_usd: f64) {
        if !net_usd.is_finite() {
            return;
        }
        self.realised_usd += net_usd;
        if self.realised_usd <= -self.limits.max_daily_loss_usd {
            self.halt(format!(
                "daily loss limit of ${:.2} reached",
                self.limits.max_daily_loss_usd
            ));
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> RiskGate {
        RiskGate::new(Limits {
            max_position_usd: 100.0,
            max_daily_loss_usd: 10.0,
            max_consecutive_failures: 3,
            min_net_profit_usd: 0.01,
            max_slippage_bps: 30.0,
            max_daily_trades: 5,
            // Disabled here so the existing "stays halted until resume()" tests keep
            // meaning exactly what they say. Auto-resume gets its own gate below.
            halt_cooldown_secs: 0,
        })
    }

    /// The trade is counted when it is sent and priced when the chain answers. Pricing
    /// it must not count it a second time, or the daily cap silently halves.
    #[test]
    fn settling_a_trade_prices_it_without_counting_it_again() {
        let mut g = gate();
        g.record(Outcome::Landed { net_usd: 0.0 });
        assert_eq!(g.trades_today(), 1);
        assert!((g.realised_usd() - 0.0).abs() < f64::EPSILON);

        g.settle(-0.25);
        assert_eq!(g.trades_today(), 1, "settling is not a new trade");
        assert!((g.realised_usd() + 0.25).abs() < 1e-9);
        assert!(g.halted().is_none(), "a quarter is nowhere near the $10 limit");
    }

    /// The whole point: a loss that only becomes known after confirmation still has to
    /// reach the daily limit. Before `settle` existed this figure stayed at zero and the
    /// one limit denominated in money could never trigger.
    #[test]
    fn losses_confirmed_after_the_fact_still_halt_on_the_daily_limit() {
        let mut g = gate();
        for _ in 0..4 {
            g.record(Outcome::Landed { net_usd: 0.0 });
            g.settle(-3.0);
        }
        assert!(
            g.halted().is_some(),
            "four confirmed $3 losses exceed the $10 daily limit and must halt"
        );
        assert!(g.halted().unwrap().contains("daily loss limit"));
    }

    /// A confirmation that never arrives must not be guessed at in either direction.
    #[test]
    fn settling_a_non_finite_figure_changes_nothing() {
        let mut g = gate();
        g.record(Outcome::Landed { net_usd: 0.0 });
        g.settle(f64::NAN);
        g.settle(f64::NEG_INFINITY);
        assert!(g.realised_usd().is_finite(), "a bad figure must not poison the running total");
        assert!(g.halted().is_none());
    }

    fn ok_trade() -> Proposal {
        Proposal { size_usd: 50.0, expected_net_usd: 1.0 }
    }

    #[test]
    fn a_sound_trade_is_allowed() {
        assert_eq!(gate().check(&ok_trade()), Decision::Allow);
    }

    #[test]
    fn a_trade_larger_than_the_position_limit_is_refused() {
        let d = gate().check(&Proposal { size_usd: 101.0, ..ok_trade() });
        assert!(matches!(d, Decision::Refuse(_)));
        assert!(d.reason().unwrap().contains("exceeds"));
    }

    #[test]
    fn a_trade_below_the_profit_floor_is_refused() {
        let d = gate().check(&Proposal { expected_net_usd: 0.001, ..ok_trade() });
        assert!(matches!(d, Decision::Refuse(_)));
    }

    /// The failure this exists to prevent: an estimate that went to NaN compares false
    /// against every limit, so an unguarded gate waves it through.
    #[test]
    fn a_non_finite_estimate_is_refused_rather_than_compared() {
        let g = gate();
        assert!(!g.check(&Proposal { expected_net_usd: f64::NAN, ..ok_trade() }).is_allowed());
        assert!(!g.check(&Proposal { size_usd: f64::NAN, ..ok_trade() }).is_allowed());
        assert!(!g.check(&Proposal { size_usd: f64::INFINITY, ..ok_trade() }).is_allowed());
    }

    #[test]
    fn losses_accumulate_until_the_daily_limit_halts_trading() {
        let mut g = gate();
        for _ in 0..4 {
            g.record(Outcome::Landed { net_usd: -3.0 });
        }
        assert!(g.is_halted(), "realised {} should have tripped -10", g.realised_usd());
        assert!(matches!(g.check(&ok_trade()), Decision::Halt(_)));
    }

    #[test]
    fn consecutive_failures_trip_the_breaker() {
        let mut g = gate();
        g.record(Outcome::Failed);
        g.record(Outcome::Failed);
        assert!(!g.is_halted(), "two failures is not yet three");
        g.record(Outcome::Failed);
        assert!(g.is_halted());
    }

    /// Losing a race is the normal case, not a defect. Counting it would stop trading
    /// on a competitive day rather than a broken one.
    #[test]
    fn missed_races_do_not_trip_the_breaker() {
        let mut g = gate();
        for _ in 0..50 {
            g.record(Outcome::Missed);
        }
        assert!(!g.is_halted());
        assert_eq!(g.consecutive_failures(), 0);
    }

    #[test]
    fn one_success_clears_the_failure_streak() {
        let mut g = gate();
        g.record(Outcome::Failed);
        g.record(Outcome::Failed);
        g.record(Outcome::Landed { net_usd: 0.5 });
        assert_eq!(g.consecutive_failures(), 0);
        g.record(Outcome::Failed);
        assert!(!g.is_halted(), "the streak restarted, so one more must not trip it");
    }

    #[test]
    fn the_daily_trade_cap_refuses_without_halting() {
        let mut g = gate();
        for _ in 0..5 {
            g.record(Outcome::Landed { net_usd: 0.10 });
        }
        let d = g.check(&ok_trade());
        assert!(matches!(d, Decision::Refuse(_)), "got {d:?}");
        assert!(!g.is_halted(), "a full day is not a fault");
    }

    #[test]
    fn a_halt_persists_until_explicitly_resumed() {
        let mut g = gate();
        g.halt("operator pressed stop");
        assert!(!g.check(&ok_trade()).is_allowed());
        g.resume();
        assert!(g.check(&ok_trade()).is_allowed());
    }

    /// A breaker that survived midnight was tripped by something midnight did not fix.
    #[test]
    fn rolling_the_day_clears_counters_but_not_a_halt() {
        let mut g = gate();
        g.record(Outcome::Landed { net_usd: -11.0 });
        assert!(g.is_halted());

        g.roll_day();

        assert_eq!(g.realised_usd(), 0.0);
        assert_eq!(g.trades_today(), 0);
        assert!(g.is_halted(), "a new day must not silently re-arm a tripped breaker");
    }

    #[test]
    fn the_kill_switch_beats_an_otherwise_perfect_trade() {
        let mut g = gate();
        g.halt("kill switch");
        match g.check(&Proposal { size_usd: 1.0, expected_net_usd: 1_000.0 }) {
            Decision::Halt(r) => assert!(r.contains("kill switch")),
            other => panic!("a halted gate must refuse everything, got {other:?}"),
        }
    }

    /// The exact bug found on a live run: nothing in the app ever called `resume()`,
    /// so a trip was permanent until the process was restarted. `tick_auto_resume`
    /// exists so a cooldown, not a restart, is what ends a halt.
    #[test]
    fn a_halt_clears_itself_once_the_cooldown_elapses() {
        let mut g = RiskGate::new(Limits { halt_cooldown_secs: 1, ..Limits::default() });
        g.halt("simulation rejected three times in a row");
        assert!(g.is_halted());

        g.tick_auto_resume();
        assert!(g.is_halted(), "the cooldown has not elapsed yet");

        std::thread::sleep(Duration::from_millis(1100));
        g.tick_auto_resume();
        assert!(!g.is_halted(), "the cooldown elapsed; nothing should still be holding it");
        assert_eq!(g.consecutive_failures(), 0);
    }

    #[test]
    fn a_zero_cooldown_disables_auto_resume_entirely() {
        let mut g = RiskGate::new(Limits { halt_cooldown_secs: 0, ..Limits::default() });
        g.halt("kill switch");
        std::thread::sleep(Duration::from_millis(50));
        g.tick_auto_resume();
        assert!(g.is_halted(), "a zero cooldown must behave exactly like resume()-only");
    }

    #[test]
    fn ticking_auto_resume_while_not_halted_does_nothing() {
        let mut g = gate();
        g.tick_auto_resume();
        assert!(!g.is_halted());
        assert_eq!(g.consecutive_failures(), 0);
    }

    #[test]
    fn limits_that_could_never_trade_are_rejected_up_front() {
        assert!(Limits::default().validate().is_ok());
        assert!(Limits { max_position_usd: 0.0, ..Limits::default() }.validate().is_err());
        assert!(Limits { max_daily_loss_usd: 0.0, ..Limits::default() }.validate().is_err());
        assert!(Limits { max_consecutive_failures: 0, ..Limits::default() }.validate().is_err());
        assert!(Limits { max_daily_trades: 0, ..Limits::default() }.validate().is_err());
        assert!(Limits { max_position_usd: f64::NAN, ..Limits::default() }.validate().is_err());
    }

    /// The defaults are what someone gets if they never open the settings, so they had
    /// better be survivable rather than ambitious.
    #[test]
    fn the_default_limits_are_conservative() {
        let d = Limits::default();
        assert!(d.max_position_usd <= 25.0);
        assert!(d.max_daily_loss_usd <= 5.0);
        assert!(d.min_net_profit_usd > 0.0, "a zero floor trades on noise");
    }
}
