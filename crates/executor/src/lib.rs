//! Execution: turning a detected opportunity into a signed transaction, or refusing to.
//!
//! # The safety argument
//!
//! This crate builds swap instructions for venues whose on-chain layouts cannot be
//! verified from a development machine. A wrong account order or a wrong discriminator
//! produces a transaction that fails — and, in the worst case, one that succeeds while
//! doing something other than what was intended.
//!
//! The answer is that **nothing is ever sent that has not first been simulated against
//! the node's live state, and shown to increase the balance it was supposed to
//! increase**. Simulation runs the real program against the real accounts. An encoder
//! that is wrong fails there, costing a round trip and nothing else. This is why
//! [`Plan::execute`] has no path that submits without simulating first, and why the
//! profit it checks is the *observed* balance delta rather than the quote that motivated
//! the trade.
//!
//! That reverses the usual relationship between this codebase and its own arithmetic.
//! Everywhere else, the quote is the answer. Here the quote is only a reason to ask the
//! chain, and the chain's answer is what decides.

pub mod encode;
pub mod pda;
pub mod risk;
pub mod route;
pub mod rpc;
pub mod ticks;
pub mod tx;
pub mod venue;
pub mod verify;

use anyhow::{bail, Result};
use cb_wallet::Wallet;
use risk::{Decision, Outcome, Proposal, RiskGate};
use rpc::Rpc;
use solana_sdk::pubkey::Pubkey;

/// How the outcome of an attempt is reported back to the caller.
#[derive(Debug, Clone)]
pub enum Attempt {
    /// Never left the machine. Carries the reason, which is always printable.
    Refused(String),
    /// Simulated, and the simulation said this would not profit.
    SimulationRejected { reason: String, observed_net_usd: Option<f64> },
    /// Submitted. A signature is not yet a profit.
    Submitted { signature: String, expected_net_usd: f64 },
    /// Simulated deliberately and never submitted, to find out what would have
    /// happened. See [`Plan::probe`].
    Probed(Probe),
}

/// What a probe found out, having spent nothing to find it out.
///
/// A probe exists to answer a question the executable filters make unanswerable: a
/// candidate that is never built is never re-priced, and a candidate that is never
/// re-priced can never show whether the filter that rejected it was right. Every
/// variant here is a measurement, and none of them cost a lamport.
#[derive(Debug, Clone)]
pub enum Probe {
    /// Simulated against live state and cleared the floor this trade would have
    /// demanded. The filter that held it back cost this much of a real opportunity.
    WouldHaveProfited { after: u64, needed: u64, units_consumed: Option<u64> },
    /// The price moved between re-pricing and simulation, so the last hop could not
    /// deliver what the instruction demanded. The ordinary outcome.
    MissedFloor { reason: String, units_consumed: Option<u64> },
    /// Simulated cleanly but came out below the balance that would have meant profit.
    ShortOfFloor { after: u64, needed: u64, units_consumed: Option<u64> },
    /// Failed for a reason that is not a missed floor — a defect, or a route this
    /// code builds wrongly.
    Rejected { reason: String },
}

impl Probe {
    /// Whether this probe found a trade that would have made money.
    #[must_use]
    pub const fn would_have_profited(&self) -> bool {
        matches!(self, Self::WouldHaveProfited { .. })
    }

    /// What the simulation actually burned, where the chain reported it.
    #[must_use]
    pub const fn units_consumed(&self) -> Option<u64> {
        match self {
            Self::WouldHaveProfited { units_consumed, .. }
            | Self::MissedFloor { units_consumed, .. }
            | Self::ShortOfFloor { units_consumed, .. } => *units_consumed,
            Self::Rejected { .. } => None,
        }
    }
}

impl std::fmt::Display for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WouldHaveProfited { after, needed, units_consumed } => write!(
                f,
                "probe: would have profited — simulated {after} against the {needed} it                  had to reach{}",
                units_consumed.map_or(String::new(), |u| format!(", {u} compute units"))
            ),
            Self::MissedFloor { reason, units_consumed } => write!(
                f,
                "probe: the floor was missed — {reason}{}",
                units_consumed.map_or(String::new(), |u| format!(", {u} compute units"))
            ),
            Self::ShortOfFloor { after, needed, units_consumed } => write!(
                f,
                "probe: simulated {after}, under the {needed} that would have meant                  profit{}",
                units_consumed.map_or(String::new(), |u| format!(", {u} compute units"))
            ),
            Self::Rejected { reason } => write!(f, "probe: rejected — {reason}"),
        }
    }
}

/// A trade that has been priced and is ready to be considered.
pub struct Plan {
    pub size_usd: f64,
    pub expected_net_usd: f64,
    /// Where the proof of profit is read from, and what it must reach.
    pub profit: route::Profit,
    /// The balance `profit` must show **after** execution.
    ///
    /// A post-balance, not a gain. The distinction is load-bearing: a token account
    /// that already held a balance satisfies a gain-shaped threshold without the trade
    /// having earned anything, and the account we trade from is exactly the account
    /// that already holds a balance. [`route::build`] computes this from a measured
    /// pre-balance so the caller cannot supply the wrong one by omission.
    pub min_post_balance: u64,
    /// The serialised, signed transaction, base64 for the wire.
    pub tx_base64: String,
}

impl Plan {
    /// Run the full gauntlet: risk gate, simulation, and only then submission.
    ///
    /// # Errors
    /// If the RPC calls themselves fail. A refusal is not an error — it is a normal
    /// outcome and comes back as [`Attempt::Refused`].
    pub async fn execute(
        &self,
        gate: &mut RiskGate,
        rpc: &Rpc,
        dry_run: bool,
    ) -> Result<Attempt> {
        let proposal = Proposal {
            size_usd: self.size_usd,
            expected_net_usd: self.expected_net_usd,
        };
        match gate.check(&proposal) {
            Decision::Allow => {}
            Decision::Refuse(r) | Decision::Halt(r) => return Ok(Attempt::Refused(r)),
        }

        // The chain's opinion, before anything irreversible.
        let sim = rpc.simulate(&self.tx_base64, &[self.profit.address()]).await?;
        if !sim.succeeded() {
            // Both read before `err` is moved out of `sim`.
            let context = sim.error_context();
            let missed_floor = sim.missed_its_floor();
            let err = sim.err.unwrap_or_else(|| "unknown".into());
            // Carry the program's own account of what went wrong, not just the error
            // code. `{"InstructionError":[6,{"Custom":6018}]}` says a floor was missed;
            // it does not say by how much, and those are entirely different findings —
            // a miss of two basis points is a race worth re-entering, a miss of two
            // orders of magnitude is a defect in what this code built. A live run spent
            // days at a 100% rejection rate unable to tell those apart, because the
            // logs the chain already returned were being dropped on this line.
            let reason = match context {
                Some(ctx) => format!("{err} — {ctx}"),
                None => err,
            };
            // A malformed transaction is a defect and trips the breaker. A floor the
            // pool could not meet is not — it is the guard working, and it costs
            // nothing because nothing was submitted.
            //
            // This line used to record every rejection as a defect, on the reasoning
            // that simulation only fails when what was built is wrong. That was true
            // while the floors came from detection-time quotes: a rejection then meant
            // the transaction had been demanding a profit derived from a price that no
            // longer existed. Since `hops_for` began re-pricing against state fetched
            // moments earlier, a missed floor means something else entirely — the price
            // moved in the round trip between the re-price and the simulation.
            //
            // Losing that race is the normal outcome, roughly three times in four. So
            // six in a row arrived quickly, halted trading for the full cooldown, and
            // did it again on resume. Over six hours of live running the bot reached
            // simulation nineteen times, missed the floor on fifteen of them, and spent
            // the gaps refusing everything with "trading is halted".
            gate.record(if missed_floor { Outcome::Missed } else { Outcome::Failed });
            return Ok(Attempt::SimulationRejected { reason, observed_net_usd: None });
        }

        // Read the balance from whichever place this route's profit lands in. Reading a
        // token amount for a route that ends by closing its token account would find
        // nothing, and "nothing" must never be mistaken for "no profit is fine".
        let observed = match self.profit {
            route::Profit::TokenAccount(_) => sim.post_token_amounts.first().copied().flatten(),
            route::Profit::Lamports(_) => sim.post_lamports.first().copied(),
        };
        let Some(after) = observed else {
            gate.record(Outcome::Failed);
            return Ok(Attempt::SimulationRejected {
                reason: "simulation returned no balance for the profit account".into(),
                observed_net_usd: None,
            });
        };

        if after < self.min_post_balance {
            return Ok(Attempt::SimulationRejected {
                reason: format!(
                    "simulated balance {after} is below the {} this trade must reach \
                     to have profited",
                    self.min_post_balance
                ),
                observed_net_usd: None,
            });
        }

        if dry_run {
            return Ok(Attempt::Refused(
                "dry run — the trade simulated profitably and was not sent".into(),
            ));
        }

        let signature = rpc.send(&self.tx_base64, true).await?;
        Ok(Attempt::Submitted { signature, expected_net_usd: self.expected_net_usd })
    }

    /// Ask the chain what would have happened, and stop there.
    ///
    /// # Why this is a separate function and not a flag on [`Plan::execute`]
    ///
    /// Because a flag can be false. This function contains no call to `Rpc::send` at
    /// all, so no argument, no configuration mistake and no future edit to a condition
    /// can turn a measurement into a trade. That property is the whole point: probes
    /// run on candidates the executable filters rejected, which are by definition the
    /// candidates nobody has decided are safe to submit.
    ///
    /// It also deliberately does not touch the [`RiskGate`]. A probe spends nothing, so
    /// it must not consume the daily trade budget, and it must not count toward the
    /// consecutive-failure breaker — a filter being wrong a hundred times in a row is a
    /// finding, not a reason to stop trading.
    ///
    /// # Errors
    /// If the simulation RPC itself fails. A rejection is not an error: it is the
    /// answer, and comes back as [`Probe::Rejected`].
    pub async fn probe(&self, rpc: &Rpc) -> Result<Probe> {
        let sim = rpc.simulate(&self.tx_base64, &[self.profit.address()]).await?;
        let units = sim.units_consumed;

        if !sim.succeeded() {
            let missed = sim.missed_its_floor();
            let context = sim.error_context();
            let err = sim.err.clone().unwrap_or_else(|| "unknown".into());
            let reason = match context {
                Some(ctx) => format!("{err} — {ctx}"),
                None => err,
            };
            return Ok(if missed {
                Probe::MissedFloor { reason, units_consumed: units }
            } else {
                Probe::Rejected { reason }
            });
        }

        let observed = match self.profit {
            route::Profit::TokenAccount(_) => sim.post_token_amounts.first().copied().flatten(),
            route::Profit::Lamports(_) => sim.post_lamports.first().copied(),
        };
        let Some(after) = observed else {
            return Ok(Probe::Rejected {
                reason: "simulation returned no balance for the profit account".into(),
            });
        };

        Ok(if after < self.min_post_balance {
            Probe::ShortOfFloor {
                after,
                needed: self.min_post_balance,
                units_consumed: units,
            }
        } else {
            Probe::WouldHaveProfited {
                after,
                needed: self.min_post_balance,
                units_consumed: units,
            }
        })
    }
}

/// Everything execution needs that is not per-trade.
pub struct Executor {
    pub wallet: Wallet,
    pub rpc: Rpc,
    pub gate: RiskGate,
    /// When true, nothing is ever submitted however good it looks.
    pub dry_run: bool,
}

impl Executor {
    /// # Errors
    /// If the limits are unusable.
    pub fn new(wallet: Wallet, rpc: Rpc, limits: risk::Limits, dry_run: bool) -> Result<Self> {
        if let Err(e) = limits.validate() {
            bail!("{e}");
        }
        Ok(Self { wallet, rpc, gate: RiskGate::new(limits), dry_run })
    }

    #[must_use]
    pub fn pubkey(&self) -> Pubkey {
        self.wallet.pubkey()
    }
}

#[cfg(test)]
mod probe_tests {
    use super::Probe;

    /// The guarantee that makes a probe safe to point at candidates nobody has approved
    /// for trading: there is no submission inside it to reach.
    ///
    /// A flag can be passed wrongly and a condition can be edited into the wrong shape.
    /// The absence of the call cannot. If someone later adds one, this fails rather than
    /// letting a measurement quietly become a trade against a route the executable
    /// filters had rejected.
    #[test]
    fn a_probe_has_no_way_to_submit() {
        let src = include_str!("lib.rs");
        let probe_fn = &src[src.find("pub async fn probe(").expect("probe exists")..];
        let body = &probe_fn[..probe_fn.find("\n    }").expect("body ends")];
        assert!(
            !body.contains("rpc.send("),
            "a probe must contain no submission — it is pointed at candidates that were \
             refused, and the refusal is the only thing keeping them off the chain"
        );
        assert!(body.contains("rpc.simulate("), "a probe that asks nothing measures nothing");
    }

    /// Only one verdict means the filter cost something. The others are the filter
    /// being right, and counting them as findings would turn this into a machine for
    /// congratulating itself.
    #[test]
    fn only_a_cleared_floor_counts_as_a_missed_opportunity() {
        let profited = Probe::WouldHaveProfited { after: 10, needed: 9, units_consumed: Some(1) };
        assert!(profited.would_have_profited());
        for other in [
            Probe::MissedFloor { reason: "moved".into(), units_consumed: Some(1) },
            Probe::ShortOfFloor { after: 8, needed: 9, units_consumed: Some(1) },
            Probe::Rejected { reason: "malformed".into() },
        ] {
            assert!(
                !other.would_have_profited(),
                "{other} is the filter being right, not an opportunity it cost"
            );
        }
    }

    /// Every verdict has to be readable in a log line, because the log is where these
    /// are read back from. An empty one is a measurement that was taken and lost.
    #[test]
    fn every_verdict_says_something() {
        for p in [
            Probe::WouldHaveProfited { after: 10, needed: 9, units_consumed: Some(77_938) },
            Probe::MissedFloor { reason: "TooLittleOutputReceived".into(), units_consumed: None },
            Probe::ShortOfFloor { after: 8, needed: 9, units_consumed: None },
            Probe::Rejected { reason: "malformed".into() },
        ] {
            let said = p.to_string();
            assert!(said.starts_with("probe: "), "{said} must announce what it is");
            assert!(said.len() > 12, "{said} says too little to be worth recording");
        }
    }

    /// The compute figure is what a right-sized limit will be set from, so it has to
    /// survive being carried out of every verdict that has one.
    #[test]
    fn compute_units_are_carried_out_of_every_verdict_that_has_them() {
        assert_eq!(
            Probe::WouldHaveProfited { after: 1, needed: 1, units_consumed: Some(77_938) }
                .units_consumed(),
            Some(77_938)
        );
        assert_eq!(
            Probe::MissedFloor { reason: "x".into(), units_consumed: Some(42) }.units_consumed(),
            Some(42)
        );
        assert_eq!(Probe::Rejected { reason: "x".into() }.units_consumed(), None);
    }
}
