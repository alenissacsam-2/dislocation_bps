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
            let reason = sim.err.unwrap_or_else(|| "unknown".into());
            // A simulation failure is a defect in what was built, not a lost race, so it
            // counts toward the breaker. Three of these in a row means stop.
            gate.record(Outcome::Failed);
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
