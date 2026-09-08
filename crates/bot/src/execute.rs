//! Turning a detected cycle into a signed transaction, or declining to.
//!
//! # What changed when this file appeared
//!
//! Until now `cb-bot` linked neither `cb-executor` nor `cb-wallet` nor `solana-sdk`,
//! and that was the strongest safety property this project had: the measuring binary
//! contained no path to a signature whatever its config said, and no argument about
//! guards or flags could change that, because the code simply was not there.
//!
//! It is there now. That property is gone and it is not coming back while this module
//! exists, so what replaces it has to be worth having. Four things, in the order they
//! stop a mistake:
//!
//! 1. **Two switches.** `mode = "live"` in the config *and* `CRYPTOBOT_ALLOW_LIVE=1` in
//!    the environment. The application deliberately does not set the second.
//! 2. **A passphrase the process does not have.** The key is encrypted at rest and the
//!    passphrase arrives on stdin at spawn. A live config alone signs nothing; without
//!    the passphrase there is no key in memory to sign with.
//! 3. **The risk gate**, which is checked before the chain is asked anything.
//! 4. **Simulation against live state, every time, with the profit read from the
//!    resulting balance** rather than from the quote that motivated the trade. This is
//!    the one that covers the encoders being wrong, and it is why an unverified account
//!    order is survivable: a wrong instruction fails in simulation and costs a round
//!    trip.
//!
//! # The sizing rule, which is the whole safety argument in one line
//!
//! Each hop is built to spend **exactly what the previous hop is guaranteed to
//! return** — `hop[i+1].amount_in = hop[i].min_amount_out` — and each floor is the
//! quoted output less slippage. Two things fall out of that:
//!
//! - No hop can be underfunded, because the hop before it promised at least that much
//!   or the transaction reverts.
//! - The last hop's floor is the only number that decides whether the cycle is worth
//!   signing, and [`cb_executor::route::build`] refuses to encode it unless that floor
//!   exceeds what the first hop spent.
//!
//! So a transaction that lands is profitable by construction, enforced by the AMM
//! programs rather than by this codebase's arithmetic. If slippage eats the edge, the
//! last floor drops below the first input and the route refuses to build — which is
//! the correct answer and not an error.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use cb_core::types::{Dex, Pubkey32};
use cb_executor::encode::{pk, programs, to_pubkey};
use cb_executor::pda::associated_token_address;
use cb_executor::route::{self, Hop, RouteOptions, WsolPolicy};
use cb_executor::venue::raydium::BitmapPolicy;
use cb_executor::venue::VenueExtra;
use cb_executor::{ticks, tx, Attempt, Executor, Plan};
use solana_sdk::pubkey::Pubkey;

/// Everything execution needs to rebuild a detected cycle as instructions.
///
/// Built at detection, from the same legs that produced the quote. Re-deriving it later
/// from the recorded USD figures would be a guess about which pools were involved, and
/// a swap against the wrong pool is not a rounding error.
#[derive(Debug, Clone)]
pub struct CyclePlan {
    /// Pools in traversal order, with the venue each belongs to.
    pub pools: Vec<(Pubkey32, Dex)>,
    /// Mints, one longer than `pools`. First and last are the same by construction.
    pub mints: Vec<Pubkey32>,
    /// What the first hop spends, in the base mint's own units.
    pub amount_in: u128,
    /// The quoted output of each hop, in that hop's output-mint units.
    ///
    /// **Detection-time.** Kept for the record and for comparison, but no longer what
    /// the floors are built from — see [`Trader::hops_for`].
    pub leg_out: Vec<u128>,
    /// Each pool's fee tier, in parts per million.
    ///
    /// Carried from detection because it is pool *configuration*, not pool *price*: it
    /// lives in a separate config account that a swap does not touch, so unlike a
    /// reserve or a sqrt-price it does not go stale between detection and execution.
    /// Re-pricing a Raydium CLMM leg needs it and re-fetching it would be a round trip
    /// spent on a number that cannot have changed.
    pub fee_ppm: Vec<u32>,
}

impl CyclePlan {
    /// Whether every venue in this cycle has an encoder.
    #[must_use]
    pub fn encodable(&self) -> bool {
        self.pools
            .iter()
            .all(|(_, d)| matches!(d, Dex::OrcaWhirlpool | Dex::RaydiumClmm))
    }

    /// The venue that stops this cycle being encodable, if any.
    #[must_use]
    pub fn blocking_venue(&self) -> Option<Dex> {
        self.pools
            .iter()
            .find(|(_, d)| !matches!(d, Dex::OrcaWhirlpool | Dex::RaydiumClmm))
            .map(|(_, d)| *d)
    }
}

/// Tunables that are not per-trade.
#[derive(Debug, Clone, Copy)]
pub struct TradeOptions {
    /// How far below the quote each hop's floor is set, in tenths of a basis point.
    pub slippage_tenth_bps: u32,
    pub priority_micro_lamports: u64,
    pub compute_units: u32,
    /// When true, everything runs including the simulation and nothing is submitted.
    pub dry_run: bool,
    /// Emit idempotent account creations for every mint the route touches.
    pub create_token_accounts: bool,
    /// How wrapped SOL is handled.
    pub wsol: WsolPolicy,
}

impl Default for TradeOptions {
    fn default() -> Self {
        Self {
            // Three tenths of a basis point, and it has to be about this small.
            //
            // A route builds only if its last floor exceeds its first input, so for `n`
            // hops at edge `e` the requirement is `s < e / n`. At 30 bps — the first
            // value here — a live dry run refused every cycle it found, guaranteeing
            // −25 to −28 bps. You cannot tolerate more slippage than the profit you are
            // chasing, and this market's profit is under 2 bps: at one whole basis
            // point per hop the two hops cost more than the edge, which refused all
            // eight cycles that a fifteen-hour run re-priced positive.
            slippage_tenth_bps: 3,
            priority_micro_lamports: 0,
            compute_units: 400_000,
            dry_run: true,
            create_token_accounts: true,
            // Wrap and close inside the transaction rather than expecting a standing
            // wSOL balance.
            //
            // A wallet holds *native* SOL; a swap moves SPL tokens, and the two are not
            // the same thing. `Reuse` assumes somebody has already wrapped some, which
            // is a manual step, an idle balance, and a thing to forget. WrapAndClose
            // makes it part of the cycle: lamports in at the start, account closed and
            // rent refunded at the end, all atomic — if any leg fails the wrap never
            // happened either.
            //
            // It costs three instructions and *no additional accounts*, because the
            // owner and the wSOL account are already in the list. The profit is then
            // read from the lamport balance rather than a token balance, which is
            // stricter in the right direction: the fee comes out of the same balance, so
            // a trade must beat its own fee to pass rather than merely beat zero.
            wsol: WsolPolicy::WrapAndClose,
        }
    }
}

/// The longest cycle that fits in one transaction.
///
/// Measured, not guessed: with real account sharing between legs a two-hop cycle
/// serialises to 800 bytes, three hops to 1048, and four to 1296 — over the 1232-byte
/// packet limit by 64 bytes, which is two accounts. See `cb_executor::tx`.
///
/// `tx::assemble` refuses an oversized transaction anyway. This constant exists so the
/// refusal happens before three RPC round trips are spent building one.
pub const MAX_EXECUTABLE_HOPS: usize = 3;

/// The same fee headroom `cb_desk::balances::FEE_ALLOWANCE` reserves, for the same
/// reason: a generous allowance for signature and priority fees on one cycle.
const FEE_ALLOWANCE_LAMPORTS: u64 = 100_000;

/// Whether wrapping `amount_in` lamports leaves enough for account rent and fees.
///
/// Returns the reserved amount when it does *not* fit, `None` when it does. Reserves
/// for every distinct mint the cycle touches regardless of whether that mint's account
/// already exists — confirming which do would cost another round trip, and refusing a
/// fundable trade is far cheaper than sending one that fails mid-transaction.
///
/// This is the check a live run was missing. A wrap-transfer sized against the full
/// quoted `capital_usd`, with nothing held back for the rent the same transaction also
/// pays to create token accounts, failed in simulation with the System Program's
/// `ResultWithNegativeLamports` three times before the risk gate's own breaker halted
/// trading. The exact case is pinned in `the_live_failure_is_caught_before_it_repeats`.
#[must_use]
pub fn wrap_shortfall(amount_in: u64, distinct_mints: usize, balance: u64) -> Option<u64> {
    let reserved = distinct_mints as u64 * route::TOKEN_ACCOUNT_RENT + FEE_ALLOWANCE_LAMPORTS;
    if amount_in.saturating_add(reserved) > balance {
        Some(reserved)
    } else {
        None
    }
}

/// Apply a slippage haircut, rounding down. The tolerance is in **tenths** of a basis
/// point.
///
/// Rounding down is the only safe direction: a floor rounded *up* is a floor the pool
/// may be unable to meet, which turns a winning trade into a revert.
///
/// The unit is a tenth of a basis point because a whole one is bigger than this
/// market's whole opportunity — see [`cb_core::config::Config::slippage_tenth_bps`]
/// for the eight measured cycles that proved it.
#[must_use]
pub fn haircut(amount: u128, tenth_bps: u32) -> u128 {
    let keep = 100_000u128.saturating_sub(u128::from(tenth_bps));
    amount.saturating_mul(keep) / 100_000
}

/// What a pool's tick geometry looked like the last time this trader decoded it.
///
/// Kept so the next attempt can predict which tick arrays to ask about and fold that
/// question into the round trip that fetches the pools — see
/// [`cb_executor::ticks::prefetch_candidates`]. `mint_a` is here because the traversal
/// direction decides which way the sweep walks, and that too is immutable pool
/// configuration rather than something worth a round trip.
///
/// Nothing here is trusted. `tick` is only ever used to choose *which addresses to ask
/// about*; the arrays the instruction finally names are picked from the tick read out
/// of the account fetched this attempt.
#[derive(Debug, Clone, Copy)]
struct TickHint {
    tick: i32,
    spacing: u16,
    mint_a: Pubkey32,
}

/// Where one hop's predicted tick-array candidates sit inside the batched fetch: the
/// offset of the first one, and the addresses asked for, in sweep order. `None` when
/// this pool has never been decoded here and there was nothing to predict from.
type Prefetched = Option<(usize, Vec<(i32, Pubkey)>)>;

/// The program that owns a venue's pools and tick arrays.
fn program_for(dex: Dex) -> Pubkey {
    match dex {
        Dex::OrcaWhirlpool => pk(cb_dex::orca_whirlpool::PROGRAM_ID),
        _ => pk(cb_dex::raydium_clmm::PROGRAM_ID),
    }
}

/// The pool's own token A, read from the account rather than from the registry.
fn mint_a_of(dex: Dex, data: &[u8]) -> Result<Pubkey32> {
    match dex {
        Dex::OrcaWhirlpool => Ok(cb_dex::orca_whirlpool::decode(data)?.mint_a),
        Dex::RaydiumClmm => Ok(cb_dex::raydium_clmm::decode(data)?.mint_0),
        other => bail!("{} is not encodable", other.name()),
    }
}

/// Live execution, holding the wallet and the risk gate across attempts.
pub struct Trader {
    exec: Executor,
    opts: TradeOptions,
    owner: Pubkey,
    /// Tick geometry per pool, so the tick-array question costs no round trip of its
    /// own after the first attempt against that pool. See [`TickHint`].
    ticks_seen: HashMap<Pubkey32, TickHint>,
}

impl Trader {
    #[must_use]
    pub fn new(exec: Executor, opts: TradeOptions) -> Self {
        let owner = exec.pubkey();
        Self { exec, opts, owner, ticks_seen: HashMap::new() }
    }

    #[must_use]
    pub fn address(&self) -> Pubkey {
        self.owner
    }

    /// What this hop would actually return, priced against state read moments ago.
    ///
    /// `None` for a venue with no encoder — those never reach here, because a route
    /// carrying one refuses to build long before this — and for a pool whose account
    /// will not decode, which is a pool this code has no business swapping through.
    ///
    /// The fee tier comes from the plan rather than the account: it lives in a separate
    /// config account a swap does not touch, so it cannot have gone stale. Everything
    /// else — liquidity, the sqrt price, the reserves — is read fresh, which is the
    /// entire point.
    fn fresh_quote(
        dex: Dex,
        address: Pubkey32,
        data: &[u8],
        input_mint: &Pubkey32,
        fee_ppm: u32,
        amount_in: u128,
    ) -> Option<u128> {
        let state = match dex {
            Dex::OrcaWhirlpool => cb_dex::orca_whirlpool::to_pool_state(address, data, 0).ok()?,
            Dex::RaydiumClmm => {
                cb_dex::raydium_clmm::to_pool_state(address, data, fee_ppm, 0).ok()?
            }
            _ => return None,
        };
        state.leg_for_input(input_mint)?.quote(amount_in)
    }

    /// Build the hops for a cycle, with each hop funded by the previous one's floor.
    ///
    /// # Errors
    /// If the plan is malformed, if a hop cannot be re-priced against the state just
    /// fetched, or if a hop's floor collapses to zero under slippage.
    pub fn hops_for(&self, plan: &CyclePlan, pool_data: &[Vec<u8>], arrays: &[[Pubkey; 3]]) -> Result<Vec<Hop>> {
        let n = plan.pools.len();
        if n < 2 || plan.mints.len() != n + 1 || plan.leg_out.len() != n {
            bail!("malformed cycle plan: {n} pools, {} mints, {} quotes", plan.mints.len(), plan.leg_out.len());
        }
        if pool_data.len() != n || arrays.len() != n {
            bail!("have {} accounts and {} array sets for {n} pools", pool_data.len(), arrays.len());
        }

        let mut hops = Vec::with_capacity(n);
        let mut spend = plan.amount_in;
        for i in 0..n {
            // Price this hop against the state fetched moments ago, not against the
            // quote that motivated the detection.
            //
            // This is the line the run turned on. `pool_data` was already being
            // re-read every attempt — the comment at the fetch even says building from
            // anything else "just widens the gap between what we ask for and what the
            // chain will do" — and then the floor was taken from `plan.leg_out`, a
            // number computed when the websocket last spoke. So the transaction
            // demanded, on chain, a profit derived from a price that no longer existed,
            // with one basis point of tolerance. Every attempt for days reverted at the
            // hop where the stale number ran out: 236,496 opportunities recorded, zero
            // ever filled, a 100% simulation-rejection rate that no market condition
            // explains and no amount of retrying could have fixed.
            //
            // Re-quoting does not manufacture an edge that has gone. It does something
            // better: it finds out *before* building a transaction, so a vanished edge
            // costs a decode instead of a revert, a breaker strike, and a halt.
            let fresh = Self::fresh_quote(
                plan.pools[i].1,
                plan.pools[i].0,
                &pool_data[i],
                &plan.mints[i],
                plan.fee_ppm[i],
                spend,
            )
            .with_context(|| {
                format!("hop {i} could not be re-priced against the state just fetched")
            })?;

            let floor = haircut(fresh, self.opts.slippage_tenth_bps);
            if floor == 0 {
                bail!(
                    "hop {i} floors at zero after {} tenths of a bp of slippage",
                    self.opts.slippage_tenth_bps
                );
            }
            let amount_in = u64::try_from(spend).context("hop input exceeds u64")?;
            let min_amount_out = u64::try_from(floor).context("hop floor exceeds u64")?;

            hops.push(Hop {
                pool: to_pubkey(&plan.pools[i].0),
                dex: plan.pools[i].1,
                pool_data: pool_data[i].clone(),
                input_mint: to_pubkey(&plan.mints[i]),
                output_mint: to_pubkey(&plan.mints[i + 1]),
                // Token A is the pool's own mint_a, which for both venues is the lower
                // of the two by the venue's own ordering — read from the account rather
                // than assumed, because getting it backwards reverses the swap.
                input_is_a: input_is_token_a(plan.pools[i].1, &pool_data[i], &plan.mints[i])?,
                amount_in,
                min_amount_out,
                tick_arrays: arrays[i],
            });

            // The next hop spends exactly what this one guarantees. See the module docs.
            spend = floor;
        }
        Ok(hops)
    }

    /// Fetch state, build, simulate, and — unless this is a dry run — submit.
    ///
    /// `size_usd` and `expected_net_usd` are what the *risk gate* judges, and they are
    /// parameters rather than fields on the plan because only the caller knows the USD
    /// index. Passing zero disables every per-trade limit by making the trade look
    /// free, so the gate treats a non-positive size as a refusal — which is how the
    /// first version of this function was caught: it hardcoded both to zero, every unit
    /// test passed, and the pipeline was refused at the gate before it ever reached the
    /// chain.
    ///
    /// # Errors
    /// If the chain cannot be reached or the accounts cannot be read. A refusal is not
    /// an error and comes back as [`Attempt::Refused`].
    pub async fn attempt(
        &mut self,
        plan: &CyclePlan,
        size_usd: f64,
        expected_net_usd: f64,
    ) -> Result<Attempt> {
        // Cheapest and most fatal first, same principle as the gate itself: a halted
        // run used to still pay for a full account fetch, tick-array resolution, and
        // a blockhash — several RPC round trips — only to be refused at the very last
        // step inside `execute()`. Checked here too so a halt costs nothing while it
        // stands, however long the cooldown before it clears itself.
        self.exec.gate.tick_auto_resume();
        if let Some(why) = self.exec.gate.halted() {
            return Ok(Attempt::Refused(format!("trading is halted: {why}")));
        }

        if let Some(dex) = plan.blocking_venue() {
            return Ok(Attempt::Refused(format!("{} has no encoder", dex.name())));
        }
        if plan.pools.len() > MAX_EXECUTABLE_HOPS {
            return Ok(Attempt::Refused(format!(
                "{} hops will not fit in one transaction without an address lookup table                  (the ceiling is {MAX_EXECUTABLE_HOPS})",
                plan.pools.len()
            )));
        }

        let rpc = &self.exec.rpc;
        let token_program = pk(programs::SPL_TOKEN);

        // Re-read every pool now. The sweep's copy is as old as the last websocket
        // update, and an instruction must name the vaults belonging to the state it is
        // priced against — but more importantly the simulation is about to price this
        // against *current* state anyway, so building from anything else just widens
        // the gap between what we ask for and what the chain will do.
        // Everything that does not depend on the pools is fetched *beside* them, and
        // everything that does is fetched all at once.
        //
        // What matters is not the number of round trips but how many of them sit
        // between reading a price and asking the chain to honour it. That window used
        // to be the pool read, then one tick-array fetch per hop in sequence, then a
        // blockhash — four serial trips for a two-hop cycle, several hundred
        // milliseconds during which the price this trade was built on kept moving. The
        // floors then missed by about a basis point, which is precisely the size of
        // what moves in that window. The blockhash never needed to be in there at all:
        // it depends on nothing here, and simulation replaces it anyway.
        // Everything the chain has to answer before this trade can be built, asked at
        // once wherever the questions do not actually depend on each other.
        //
        // This used to be four serial round trips: pools, then tick arrays, then the
        // wallet balance, then the simulation. A round trip to the configured Helius
        // endpoint measures 98 ms from this machine — 296 ms to the public fallback —
        // so that is about 390 ms, one whole Solana slot, between reading a price and
        // asking the chain to honour it. The edge being chased is one to two basis
        // points and the price moves a real fraction of that inside a slot, which is
        // what the refusals kept saying: floors missed by roughly a basis point.
        //
        // Only one of those dependencies is real. The balance depends on nothing, so it
        // is read here. The tick arrays depend on the pool's current tick, but only to
        // the precision of an array — hundreds of basis points wide — so they are
        // *predicted* from the last tick each pool was seen at and checked against the
        // fresh one below. Two round trips remain: this, and the simulation.
        let base_mint = to_pubkey(&plan.mints[0]);
        let wsol = pk(programs::WSOL_MINT);
        let wrapping = self.opts.wsol == WsolPolicy::WrapAndClose && base_mint == wsol;
        // The account this route's profit will be read from, whichever kind it is. The
        // owner's lamports and a token account's data come back from the same call, and
        // at the same commitment `balance()` used, so nothing about the wrap-shortfall
        // check below has changed except when it is asked.
        let profit_key =
            if wrapping { self.owner } else { associated_token_address(&self.owner, &base_mint, &token_program) };

        let n = plan.pools.len();
        let mut keys: Vec<Pubkey> = plan.pools.iter().map(|(k, _)| to_pubkey(k)).collect();
        keys.push(profit_key);

        // Where each hop's predicted tick-array candidates sit in `keys`. `None` for a
        // pool this trader has not decoded before, which falls back to asking.
        let mut predicted: Vec<Prefetched> = Vec::with_capacity(n);
        for (i, (pool_raw, dex)) in plan.pools.iter().enumerate() {
            let Some(hint) = self.ticks_seen.get(pool_raw) else {
                predicted.push(None);
                continue;
            };
            let cands = ticks::prefetch_candidates(
                *dex,
                &to_pubkey(pool_raw),
                &program_for(*dex),
                hint.tick,
                hint.spacing,
                plan.mints[i] == hint.mint_a,
            );
            let at = keys.len();
            keys.extend(cands.iter().map(|(_, k)| *k));
            predicted.push(Some((at, cands)));
        }

        let (fetched, (blockhash, _)) =
            tokio::try_join!(rpc.accounts_full(&keys), rpc.latest_blockhash())?;
        let mut pool_data = Vec::with_capacity(n);
        for (key, acc) in keys.iter().zip(fetched.iter()).take(n) {
            let Some(a) = acc.as_ref() else {
                return Ok(Attempt::Refused(format!("pool {key} vanished between sweep and build")));
            };
            pool_data.push(a.data.clone());
        }
        let profit_account = fetched.get(n).and_then(Option::as_ref);

        // Which tick arrays actually exist, in the direction each hop will move the
        // price. Measured per attempt rather than cached: an array is created the
        // moment somebody opens a position, so a cached answer goes stale in the
        // direction that matters.
        //
        // Resolved for every hop concurrently. The hops do not depend on each other —
        // each needs only its own pool's current tick — so doing them in sequence spent
        // one full round trip per hop widening the very gap that then made the floors
        // unreachable.
        let mut arrays: Vec<Option<[Pubkey; 3]>> = vec![None; n];
        let mut pending = Vec::new();
        let mut learned: Vec<(Pubkey32, TickHint)> = Vec::with_capacity(n);
        for (i, (pool_raw, dex)) in plan.pools.iter().enumerate() {
            let pool = to_pubkey(pool_raw);
            let program = program_for(*dex);
            let (tick, spacing) = tick_and_spacing(*dex, &pool_data[i])?;
            let falling = input_is_token_a(*dex, &pool_data[i], &plan.mints[i])?;
            // Whatever happens below, next time this pool comes round we can predict.
            learned.push((
                *pool_raw,
                TickHint { tick, spacing, mint_a: mint_a_of(*dex, &pool_data[i])? },
            ));

            // The window the *fresh* tick actually asks for.
            let real = ticks::candidates(*dex, &pool, &program, tick, spacing, falling);
            let from_prefetch = predicted[i].as_ref().and_then(|(at, cands)| {
                let known: HashMap<Pubkey, bool> = cands
                    .iter()
                    .enumerate()
                    .map(|(j, (_, key))| {
                        let live = fetched
                            .get(at + j)
                            .and_then(Option::as_ref)
                            .is_some_and(|a| a.owner == program);
                        (*key, live)
                    })
                    .collect();
                // Every address the real window names has to be one we already asked
                // about. Missing even one means we do not know whether it exists, and
                // guessing is how a swap gets handed an array that is not there.
                if !real.iter().all(|(_, key)| known.contains_key(key)) {
                    return None;
                }
                let live: Vec<(i32, Pubkey)> =
                    real.iter().copied().filter(|(_, key)| known[key]).collect();
                ticks::choose(&real, &live)
            });

            match from_prefetch {
                Some(chosen) if chosen.found > 0 => arrays[i] = Some(chosen.arrays),
                Some(_) => {
                    return Ok(Attempt::Refused(format!(
                        "pool {pool} has no initialised tick arrays to swap through"
                    )))
                }
                None => pending.push((i, pool, program, tick, spacing, falling)),
            }
        }
        for (pool_raw, hint) in learned {
            self.ticks_seen.insert(pool_raw, hint);
        }

        // Only the hops the prediction could not answer, and only on the first attempt
        // against a pool or after a tick has walked clean out of the prefetched window.
        if !pending.is_empty() {
            let asked = pending.iter().map(|&(i, pool, program, tick, spacing, falling)| {
                let dex = plan.pools[i].1;
                async move {
                    let chosen =
                        ticks::resolve(rpc, dex, &pool, &program, tick, spacing, falling).await?;
                    anyhow::Ok((i, pool, chosen))
                }
            });
            for (i, pool, chosen) in futures::future::try_join_all(asked).await? {
                if chosen.found == 0 {
                    return Ok(Attempt::Refused(format!(
                        "pool {pool} has no initialised tick arrays to swap through"
                    )));
                }
                arrays[i] = Some(chosen.arrays);
            }
        }
        let arrays: Vec<[Pubkey; 3]> = arrays.into_iter().flatten().collect();

        let hops = match self.hops_for(plan, &pool_data, &arrays) {
            Ok(h) => h,
            Err(e) => return Ok(Attempt::Refused(e.to_string())),
        };

        // The balance the profit is measured from, read from wherever this route will
        // actually report it. Reading a token account for a route that ends by closing
        // that account would measure the wrong thing entirely — and would do it
        // quietly, since both are just numbers.
        //
        // Fetched above, beside the pools, rather than in a round trip of its own: it
        // depends on nothing this function has learned.
        let pre_balance = if wrapping {
            let bal = profit_account.map_or(0, |a| a.lamports);

            // Reserve rent for every distinct mint this cycle touches, plus fee
            // headroom, before trusting the plan's amount_in as affordable.
            //
            // This is the bug a live run found. The wrap-transfer moves `amount_in`
            // lamports from the owner into the wSOL account, and that instruction runs
            // *after* the transaction has already paid rent to create any ATA that did
            // not exist yet — out of the same balance. Sizing that only knew about
            // `capital_usd` had no idea those two payments were about to compete for
            // the same lamports, and a route built against the full quoted amount
            // failed in simulation with the System Program's `ResultWithNegativeLamports`
            // (Custom(1)) on the transfer instruction: three times, on real mainnet
            // state, until the risk gate's own three-strike breaker halted trading.
            //
            // Conservative on purpose: it reserves for every distinct mint regardless
            // of whether that mint's account already exists, because confirming which
            // do would cost another round trip and refusing a fundable trade is far
            // cheaper than sending one that fails mid-transaction. "Refuse rather than
            // extrapolate" is this codebase's first design principle for exactly this
            // reason — overstating what is affordable is the direction that loses.
            let distinct_mints = plan.mints.iter().collect::<std::collections::HashSet<_>>().len();
            let amount_in_u64 = u64::try_from(plan.amount_in).unwrap_or(u64::MAX);
            if let Some(reserved) = wrap_shortfall(amount_in_u64, distinct_mints, bal) {
                return Ok(Attempt::Refused(format!(
                    "wrapping {amount_in_u64} lamports would leave less than the                      {reserved} lamports this transaction needs for account rent and                      fees, against a balance of {bal} — sizing must leave that headroom,                      not spend into it"
                )));
            }
            bal
        } else {
            profit_account
                .and_then(|a| a.data.get(64..72).and_then(|b| b.try_into().ok()))
                .map(u64::from_le_bytes)
                .unwrap_or(0)
        };

        let opts = RouteOptions {
            compute_units: self.opts.compute_units,
            priority_micro_lamports: self.opts.priority_micro_lamports,
            wsol: self.opts.wsol,
            create_token_accounts: self.opts.create_token_accounts,
            venue: VenueExtra { token_program, bitmap_policy: BitmapPolicy::Include },
        };

        let built = match route::build(&self.owner, &hops, pre_balance, &opts) {
            Ok(r) => r,
            // A route that refuses to build is the guard working, not a failure.
            Err(e) => return Ok(Attempt::Refused(e.to_string())),
        };

        let assembled = match tx::assemble(&self.exec.wallet, &built.instructions, blockhash) {
            Ok(a) => a,
            Err(e) => return Ok(Attempt::Refused(e.to_string())),
        };

        let plan_to_run = Plan {
            size_usd,
            expected_net_usd,
            profit: built.profit,
            min_post_balance: built.min_post_balance,
            tx_base64: assembled.tx_base64,
        };
        plan_to_run.execute(&mut self.exec.gate, rpc, self.opts.dry_run).await
    }

    /// Report an outcome to the risk gate. Called by the caller, because only it knows
    /// whether a signature actually landed.
    pub fn record(&mut self, outcome: cb_executor::risk::Outcome) {
        self.exec.gate.record(outcome);
    }

    #[must_use]
    pub fn halted(&self) -> Option<String> {
        self.exec.gate.halted()
    }

    /// Clear a halt on its own once the configured cooldown has elapsed.
    ///
    /// `attempt()` already does this before doing any work, but only when a cycle is
    /// actually up for consideration. Called once per sweep too so a halt clears on
    /// schedule — and the status the UI reads reflects it — even during a sweep that
    /// finds nothing worth attempting.
    pub fn tick_auto_resume(&mut self) {
        self.exec.gate.tick_auto_resume();
    }
}

/// Read the current tick and spacing out of a pool account.
fn tick_and_spacing(dex: Dex, data: &[u8]) -> Result<(i32, u16)> {
    match dex {
        Dex::OrcaWhirlpool => {
            let w = cb_dex::orca_whirlpool::decode(data)?;
            Ok((w.tick_current, w.tick_spacing))
        }
        Dex::RaydiumClmm => {
            let p = cb_dex::raydium_clmm::decode(data)?;
            Ok((p.tick_current, p.tick_spacing))
        }
        other => bail!("{} is not encodable", other.name()),
    }
}

/// Whether `mint` is the pool's token A / token 0, read from the account.
///
/// Not inferred from byte ordering or from the registry. Both venues store their own
/// ordering and it is the only authority; getting it backwards swaps the direction of
/// the trade, which is the one error that fails silently in a shape-only test.
fn input_is_token_a(dex: Dex, data: &[u8], mint: &Pubkey32) -> Result<bool> {
    let (a, b) = match dex {
        Dex::OrcaWhirlpool => {
            let w = cb_dex::orca_whirlpool::decode(data)?;
            (w.mint_a, w.mint_b)
        }
        Dex::RaydiumClmm => {
            let p = cb_dex::raydium_clmm::decode(data)?;
            (p.mint_0, p.mint_1)
        }
        other => bail!("{} is not encodable", other.name()),
    };
    if *mint == a {
        Ok(true)
    } else if *mint == b {
        Ok(false)
    } else {
        bail!("this pool does not trade the mint the cycle says it does")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_executor::rpc::Rpc;

    /// Mint `i` of a cycle. Distinct per hop, and the last equals the first so the
    /// cycle closes the way `route::build` insists on.
    fn mint(i: usize, n: usize) -> Pubkey32 {
        [if i == n { 0xA0 } else { 0xA0 + i as u8 }; 32]
    }

    pub(super) fn plan(n: usize) -> CyclePlan {
        CyclePlan {
            pools: (0..n).map(|i| ([i as u8 + 1; 32], Dex::OrcaWhirlpool)).collect(),
            mints: (0..=n).map(|i| mint(i, n)).collect(),
            amount_in: 1_000_000,
            leg_out: (0..n).map(|_| 1_010_000).collect(),
            fee_ppm: (0..n).map(|_| 3_000).collect(),
        }
    }

    /// Pool accounts that actually trade the mints the plan names. A fixture whose
    /// mints disagree with its plan is rejected by `input_is_token_a`, which is the
    /// check working rather than the test being awkward.
    pub(super) fn pools_for(p: &CyclePlan) -> Vec<Vec<u8>> {
        let n = p.pools.len();
        (0..n).map(|i| whirlpool_with(p.mints[i], p.mints[i + 1])).collect()
    }

    #[test]
    fn a_haircut_rounds_down_and_never_up() {
        assert_eq!(haircut(1_000_000, 0), 1_000_000);
        // 300 tenths is 30 bps.
        assert_eq!(haircut(1_000_000, 300), 997_000);
        assert_eq!(haircut(1_000_000, 100_000), 0);
        // Rounding must not produce a floor above the quote.
        for amount in [1u128, 7, 999, 1_000_001] {
            for tenths in [1u32, 3, 300, 5_000] {
                assert!(
                    haircut(amount, tenths) <= amount,
                    "{amount} at {tenths} tenths of a bp rounded up"
                );
            }
        }
        // A nonsense slippage cannot wrap around into a huge floor.
        assert_eq!(haircut(1_000_000, u32::MAX), 0);
    }

    /// The change this unit exists for, stated as the arithmetic that was failing.
    ///
    /// A two-hop cycle only builds when its last floor beats its first input, so the
    /// total haircut has to fit inside the edge. Over a fifteen-hour live run the
    /// cycles that were still profitable when re-priced against fresh state measured
    /// +0.05, +0.09, +0.17, +1.49, +1.61, +1.69, +1.71 and +1.93 bps — and all eight
    /// were refused, because one basis point per hop is two, and two is more than the
    /// edge. This asserts the property in both directions: the shipped floor clears
    /// the edges that were being thrown away, and the old one does not.
    #[test]
    fn the_shipped_floor_fits_inside_the_edges_that_were_being_refused() {
        const SHIPPED: u32 = 3; // 0.3 bps per hop
        const OLD: u32 = 10; // the whole basis point that refused all eight
        let survives = |edge_bps: f64, tenths: u32| {
            let input = 1_000_000_000u128;
            let quoted = |amount: u128| {
                // The whole edge arrives on the first hop; the second is a wash. Where
                // it lands does not matter, only that the round trip carries it.
                amount + (amount as f64 * edge_bps / 10_000.0) as u128
            };
            let after_first = haircut(quoted(input), tenths);
            let after_second = haircut(after_first, tenths);
            after_second > input
        };

        for edge in [1.49f64, 1.61, 1.69, 1.71, 1.93] {
            assert!(
                survives(edge, SHIPPED),
                "a {edge} bp cycle must build at 0.3 bps per hop — this is one of the \
                 eight the old floor refused"
            );
            assert!(
                !survives(edge, OLD),
                "a {edge} bp cycle cannot build at 1 bp per hop; if this passes, the \
                 measurement that motivated the tenths unit was wrong"
            );
        }

        // And the floor still has to bite: an edge smaller than the total haircut is
        // refused, which is the invariant, not a regression.
        assert!(!survives(0.5, SHIPPED), "0.5 bps does not cover 0.6 bps of floor");
    }

    /// The sizing rule. Each hop must spend exactly what the one before guarantees,
    /// which is what makes an underfunded hop impossible.
    #[test]
    fn each_hop_spends_exactly_what_the_previous_one_guarantees() {
        let t = Trader {
            exec: unreachable_executor(),
            opts: TradeOptions { slippage_tenth_bps: 300, ..Default::default() },
            owner: Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
        };
        let p = plan(3);
        let data = pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 3];

        let hops = t.hops_for(&p, &data, &arrays).expect("a well formed plan");
        assert_eq!(hops.len(), 3);
        assert_eq!(hops[0].amount_in, 1_000_000);
        for w in hops.windows(2) {
            assert_eq!(
                w[1].amount_in, w[0].min_amount_out,
                "a hop must spend exactly its predecessor's floor"
            );
        }
        // And every floor is below its quote, never above.
        for h in &hops {
            assert!(u128::from(h.min_amount_out) < 1_010_000);
        }
    }

    #[test]
    fn a_malformed_plan_is_refused_rather_than_encoded() {
        let t = Trader {
            exec: unreachable_executor(),
            opts: TradeOptions::default(),
            owner: Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
        };
        let data = pools_for(&plan(3));
        let arrays = vec![[Pubkey::new_unique(); 3]; 3];

        let mut short = plan(3);
        short.mints.pop();
        assert!(t.hops_for(&short, &data, &arrays).is_err());

        let mut mismatched = plan(3);
        mismatched.leg_out.pop();
        assert!(t.hops_for(&mismatched, &data, &arrays).is_err());

        // Fewer accounts than pools must not silently build a shorter cycle.
        assert!(t.hops_for(&plan(3), &data[..2], &arrays).is_err());
    }

    /// Slippage wide enough to zero a floor must refuse, not encode a swap that would
    /// accept anything.
    #[test]
    fn a_floor_that_collapses_to_zero_is_refused() {
        let t = Trader {
            exec: unreachable_executor(),
            opts: TradeOptions { slippage_tenth_bps: 100_000, ..Default::default() },
            owner: Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
        };
        let data = pools_for(&plan(2));
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];
        let e = t.hops_for(&plan(2), &data, &arrays).unwrap_err().to_string();
        assert!(e.contains("zero"), "{e}");
    }

    #[test]
    fn unencodable_venues_are_named_before_anything_is_fetched() {
        let mut p = plan(3);
        p.pools[1].1 = Dex::RaydiumAmmV4;
        assert!(!p.encodable());
        assert_eq!(p.blocking_venue(), Some(Dex::RaydiumAmmV4));

        let clean = plan(3);
        assert!(clean.encodable());
        assert_eq!(clean.blocking_venue(), None);
    }

    /// A mint the pool does not trade must be an error, not a coin flip on direction.
    #[test]
    fn the_direction_comes_from_the_account_and_rejects_a_foreign_mint() {
        let data = whirlpool_bytes();
        assert!(input_is_token_a(Dex::OrcaWhirlpool, &data, &[0xAA; 32]).unwrap());
        assert!(!input_is_token_a(Dex::OrcaWhirlpool, &data, &[0xBB; 32]).unwrap());
        assert!(input_is_token_a(Dex::OrcaWhirlpool, &data, &[0xCC; 32]).is_err());
    }

    fn whirlpool_bytes() -> Vec<u8> {
        whirlpool_with([0xAA; 32], [0xBB; 32])
    }

    fn whirlpool_with(mint_a: Pubkey32, mint_b: Pubkey32) -> Vec<u8> {
        let mut d = vec![0u8; cb_dex::orca_whirlpool::WHIRLPOOL_LEN];
        let spacing: u16 = 64;
        d[41..43].copy_from_slice(&spacing.to_le_bytes());
        d[43..45].copy_from_slice(&spacing.to_le_bytes());
        d[45..47].copy_from_slice(&400u16.to_le_bytes());
        d[49..65].copy_from_slice(&1_000_000_000_000u128.to_le_bytes());
        // Parked *inside* a tick range, not on its edge.
        //
        // This used to sit at tick 0 with sqrt price exactly 1<<64 — the boundary — and
        // `bounds` deliberately shrinks to zero capacity in the pinned direction there,
        // so the pool decoded fine and then quoted nothing. Harmless while the fixture
        // was only asked which mint was token A; the moment floors started being priced
        // against real state it became a pool that cannot trade. Tick 32 with spacing 64
        // sits mid-range, where both directions quote.
        let tick: i32 = 32;
        let sqrt_price =
            cb_core::clmm::sqrt_price_at_tick(tick).expect("tick 32 has a sqrt price");
        d[65..81].copy_from_slice(&sqrt_price.to_le_bytes());
        d[81..85].copy_from_slice(&tick.to_le_bytes());
        d[101..133].copy_from_slice(&mint_a);
        d[133..165].copy_from_slice(&[0xA1; 32]);
        d[181..213].copy_from_slice(&mint_b);
        d[213..245].copy_from_slice(&[0xB1; 32]);
        d
    }

    /// `hops_for` never touches the executor, so the tests do not need a wallet or a
    /// network. Constructing one that would panic if used documents that.
    pub(super) fn unreachable_executor() -> Executor {
        use cb_executor::risk::Limits;
        use solana_sdk::signer::keypair::Keypair;
        let bytes = Keypair::new().to_bytes();
        let w = cb_wallet::EncryptedKey::seal(&bytes, "t")
            .and_then(|e| e.unseal("t"))
            .expect("a throwaway wallet");
        Executor::new(w, Rpc::new("http://127.0.0.1:1").expect("client"), Limits::default(), true)
            .expect("default limits are valid")
    }
}

/// End-to-end checks against live mainnet. Ignored by default: they need a network,
/// and they are the only place the whole pipeline runs as one piece.
///
/// ```text
/// cargo test -p cb-bot mainnet -- --ignored --nocapture
/// ```
///
/// **Nothing is ever submitted here.** The trader is built with `dry_run: true`, which
/// `Plan::execute` honours by returning before `sendTransaction` even when the
/// simulation is profitable, and the wallet is generated in the test and discarded.
#[cfg(test)]
mod mainnet {
    use super::*;
    use cb_executor::rpc::Rpc;
    use cb_core::types::Dex;

    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    /// Orca SOL/USDC 4bp and Raydium CLMM WSOL/USDC 4bp — the deepest pool on each
    /// venue for the pair, so a round trip between them is the cheapest real cycle in
    /// the registry.
    const ORCA_SOL_USDC: &str = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE";
    const RAY_SOL_USDC: &str = "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv";

    fn raw(b58: &str) -> Pubkey32 {
        let mut out = [0u8; 32];
        out.copy_from_slice(&bs58::decode(b58).into_vec().expect("valid base58"));
        out
    }

    fn throwaway() -> Trader {
        use cb_executor::risk::Limits;
        use solana_sdk::signer::keypair::Keypair;
        let bytes = Keypair::new().to_bytes();
        let w = cb_wallet::EncryptedKey::seal(&bytes, "t")
            .and_then(|e| e.unseal("t"))
            .expect("a throwaway wallet");
        let rpc = Rpc::new("https://api.mainnet-beta.solana.com").expect("client");
        let exec = Executor::new(w, rpc, Limits::default(), true).expect("valid limits");
        Trader::new(exec, TradeOptions { dry_run: true, ..Default::default() })
    }

    /// A two-hop SOL → USDC → SOL cycle across Orca and Raydium, built and simulated
    /// against live state.
    ///
    /// The assertion is not that it profits — it will not; the measured edge is
    /// negative and this wallet holds nothing. The assertion is that the pipeline runs
    /// end to end and produces a *reasoned* answer rather than a panic: fetch, decode,
    /// resolve tick arrays, size the hops, build the route, assemble, sign, simulate.
    #[tokio::test]
    #[ignore = "hits mainnet"]
    async fn a_real_two_hop_cycle_builds_and_simulates_without_submitting() {
        let mut t = throwaway();
        // Deliberately tiny: 0.001 SOL. Nothing is submitted, and a size that cannot
        // move a $25m pool keeps the simulation honest about account validity rather
        // than about depth.
        let plan = CyclePlan {
            pools: vec![
                (raw(ORCA_SOL_USDC), Dex::OrcaWhirlpool),
                (raw(RAY_SOL_USDC), Dex::RaydiumClmm),
            ],
            mints: vec![raw(WSOL), raw(USDC), raw(WSOL)],
            amount_in: 1_000_000,
            // Rough, and it does not need to be right: the route refuses if the last
            // floor does not clear the first input, which is the check being exercised.
            leg_out: vec![90_000, 1_010_000],
            // Orca 0.3% and Raydium CLMM 0.25%, the tiers these two pools actually run.
            fee_ppm: vec![3_000, 2_500],
        };

        // Real USD figures: a zero size is refused by the gate before anything is
        // fetched, which is what this test caught the first time it ran.
        let outcome = t.attempt(&plan, 0.18, 0.05).await.expect("the chain answered");
        println!("outcome: {outcome:?}");

        match outcome {
            Attempt::Submitted { .. } => {
                panic!("dry_run must never submit — this is the one unacceptable result")
            }
            Attempt::Refused(why) => {
                // Refused means it never reached the chain — the risk gate, an
                // unencodable venue, or a route that would not close. All legitimate,
                // but this test exists to exercise the whole path, so say so loudly
                // enough that a permanent refusal is not mistaken for a pass.
                assert!(!why.is_empty(), "a refusal must say why");
                println!("refused before reaching the chain: {why}");
            }
            Attempt::SimulationRejected { reason, .. } => {
                assert!(!reason.is_empty(), "a rejection must say why");
                // The expected result. The wallet is generated in this test and has
                // never been funded, so it has no account at all — and a fee payer that
                // does not exist is rejected by the runtime before the program loads.
                // Reaching *this* error means fetch, decode, tick resolution, sizing,
                // routing, assembly and signing all ran and the chain answered.
                println!("the chain answered: {reason}");
                assert!(
                    reason.contains("AccountNotFound") || reason.contains("Custom"),
                    "unexpected rejection for an unfunded payer: {reason}"
                );
            }
        }
    }

    /// The tick-array resolver and the decoders, against the two real pools the cycle
    /// above uses. Separated so a failure here points at state rather than at routing.
    #[tokio::test]
    #[ignore = "hits mainnet"]
    async fn both_pools_decode_and_have_tick_arrays_to_swap_through() {
        let rpc = Rpc::new("https://api.mainnet-beta.solana.com").expect("client");
        for (b58, dex) in
            [(ORCA_SOL_USDC, Dex::OrcaWhirlpool), (RAY_SOL_USDC, Dex::RaydiumClmm)]
        {
            let key = to_pubkey(&raw(b58));
            let data = rpc
                .accounts_full(&[key])
                .await
                .expect("rpc")
                .into_iter()
                .next()
                .flatten()
                .expect("the pool exists")
                .data;
            let (tick, spacing) = tick_and_spacing(dex, &data).expect("decodes");
            let program = match dex {
                Dex::OrcaWhirlpool => pk(cb_dex::orca_whirlpool::PROGRAM_ID),
                _ => pk(cb_dex::raydium_clmm::PROGRAM_ID),
            };
            let chosen = ticks::resolve(&rpc, dex, &key, &program, tick, spacing, true)
                .await
                .expect("rpc");
            println!(
                "{dex:?} {b58}: tick {tick} spacing {spacing}, {} live arrays, current {}",
                chosen.found, chosen.current_exists
            );
            assert!(chosen.found > 0, "{b58} has no tick arrays to swap through");
        }
    }

    /// Wrap-and-close, simulated against a **funded** address.
    ///
    /// The other mainnet test signs with a keypair generated in the test, which has
    /// never been funded and so has no account at all — the runtime rejects it before
    /// the program loads, which proves the pipeline runs but says nothing about whether
    /// the wrap itself works. This one compiles unsigned and simulates as an address
    /// that really holds SOL, which is the only way to see the wrap execute.
    ///
    /// No key is involved: `sigVerify` is off, so a placeholder signature is as good as
    /// a real one and only the public address is needed.
    ///
    /// ```text
    /// CB_SIM_AS=<funded pubkey> cargo test -p cb-bot wrap_and_close -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "hits mainnet; needs CB_SIM_AS"]
    async fn wrap_and_close_builds_and_runs_against_a_funded_address() {
        use cb_executor::tx;
        let Ok(who) = std::env::var("CB_SIM_AS") else {
            println!("set CB_SIM_AS to a funded public address; skipping");
            return;
        };
        let owner: Pubkey = who.parse().expect("CB_SIM_AS must be a public key");
        let rpc = Rpc::new("https://api.mainnet-beta.solana.com").expect("client");

        // Read the balance first. The public endpoint closes keep-alive connections
        // after a handful of calls, and this one failed reliably when it came last —
        // a transport error, not a 429, so the client does not retry it. It must not:
        // retrying a dropped `sendTransaction` could submit the same trade twice.
        let pre = rpc.balance(&owner).await.expect("rpc");

        let plan = CyclePlan {
            pools: vec![
                (raw(ORCA_SOL_USDC), Dex::OrcaWhirlpool),
                (raw(RAY_SOL_USDC), Dex::RaydiumClmm),
            ],
            mints: vec![raw(WSOL), raw(USDC), raw(WSOL)],
            // 0.002 SOL. Small enough to be affordable, large enough not to be dust.
            amount_in: 2_000_000,
            leg_out: vec![180_000, 2_010_000],
            fee_ppm: vec![3_000, 2_500],
        };

        // Resolve tick arrays the way the executor does.
        let keys: Vec<Pubkey> = plan.pools.iter().map(|(k, _)| to_pubkey(k)).collect();
        let fetched = rpc.accounts_full(&keys).await.expect("rpc");
        let pool_data: Vec<Vec<u8>> =
            fetched.into_iter().map(|a| a.expect("pool exists").data).collect();

        let mut arrays = Vec::new();
        for (i, (praw, dex)) in plan.pools.iter().enumerate() {
            let pool = to_pubkey(praw);
            let program = match dex {
                Dex::OrcaWhirlpool => pk(cb_dex::orca_whirlpool::PROGRAM_ID),
                _ => pk(cb_dex::raydium_clmm::PROGRAM_ID),
            };
            let (tick, spacing) = tick_and_spacing(*dex, &pool_data[i]).expect("decodes");
            let falling = input_is_token_a(*dex, &pool_data[i], &plan.mints[i]).expect("mint");
            let chosen = ticks::resolve(&rpc, *dex, &pool, &program, tick, spacing, falling)
                .await
                .expect("rpc");
            assert!(chosen.found > 0, "no arrays for {pool}");
            arrays.push(chosen.arrays);
        }

        let t = Trader {
            exec: super::tests::unreachable_executor(),
            opts: TradeOptions { slippage_tenth_bps: 300, ..Default::default() },
            owner,
            ticks_seen: HashMap::new(),
        };
        let hops = t.hops_for(&plan, &pool_data, &arrays).expect("hops");

        let opts = RouteOptions {
            compute_units: 600_000,
            priority_micro_lamports: 0,
            wsol: WsolPolicy::WrapAndClose,
            create_token_accounts: true,
            venue: VenueExtra {
                token_program: pk(programs::SPL_TOKEN),
                bitmap_policy: BitmapPolicy::Include,
            },
        };
        let built = route::build(&owner, &hops, pre, &opts).expect("route builds");
        assert!(
            matches!(built.profit, route::Profit::Lamports(k) if k == owner),
            "wrapping must measure profit in lamports, not a token account it just closed"
        );

        let (bh, _) = rpc.latest_blockhash().await.expect("rpc");
        let compiled = tx::compile_unsigned(&owner, &built.instructions, bh).expect("fits");
        println!(
            "wrap-and-close route: {} instructions, {} accounts, {} bytes of {}",
            built.instructions.len(),
            compiled.account_count,
            compiled.size_bytes,
            tx::PACKET_LIMIT
        );

        let sim = rpc.simulate(&compiled.tx_base64, &[owner]).await.expect("rpc");
        match &sim.err {
            None => println!(
                "simulated clean: {} CU, post-lamports {:?}",
                sim.units_consumed.unwrap_or(0),
                sim.post_lamports.first()
            ),
            Some(e) => {
                for l in sim.logs.iter().rev().take(10).rev() {
                    println!("  log: {l}");
                }
                println!("rejected: {e}");
            }
        }
        // The assertion is structural: it must fit and it must measure lamports. Whether
        // this particular cycle profits is a market question, not an encoding one.
        assert!(compiled.size_bytes <= tx::PACKET_LIMIT);
    }

    /// The exact numbers from the live run this fix came from: a wallet holding
    /// 132,746,877 lamports, a 2-mint wrapping cycle sized to spend 128,808,539 — which
    /// the old code approved and the chain rejected with `ResultWithNegativeLamports`
    /// on the wrap transfer, three times, until the risk gate halted trading.
    #[test]
    fn the_live_failure_is_caught_before_it_repeats() {
        let shortfall = wrap_shortfall(128_808_539, 2, 132_746_877);
        assert!(shortfall.is_some(), "the sizing that failed on mainnet must now be refused");
        let reserved = shortfall.unwrap();
        assert_eq!(reserved, 2 * 2_039_280 + 100_000);
        // And it must actually be a shortfall by the numbers, not a coincidence.
        assert!(128_808_539 + reserved > 132_746_877);
    }

    #[test]
    fn a_wrap_with_real_headroom_is_not_refused() {
        // Same wallet, a size that leaves the reserve intact.
        assert!(wrap_shortfall(100_000_000, 2, 132_746_877).is_none());
    }

    /// More mints touched means more rent reserved: a wallet with exactly the reserve
    /// for two mints has room to spare for one and none at all for two.
    #[test]
    fn more_distinct_mints_reserve_more() {
        let bal_for_two = 2 * 2_039_280 + 100_000;
        assert!(wrap_shortfall(0, 1, bal_for_two).is_none(), "1 mint fits inside 2 mints' reserve");
        assert!(wrap_shortfall(0, 2, bal_for_two).is_none(), "exactly enough is enough");
        assert!(wrap_shortfall(0, 2, bal_for_two - 1).is_some(), "one lamport short must refuse");
        assert!(wrap_shortfall(0, 3, bal_for_two).is_some(), "3 mints must not fit 2 mints' reserve");
    }
}

#[cfg(test)]
mod fresh_quote_tests {
    use super::tests::unreachable_executor;
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    /// The bug this was written for, stated as a test.
    ///
    /// A plan carries a detection-time quote. If the pool has moved since — which is
    /// the normal case, because a websocket update and a transaction are separated by
    /// several RPC round trips — the floors must follow the *pool*, not the plan.
    /// Building them from `leg_out` demanded, on chain, a price that no longer existed,
    /// and every attempt for days reverted at exactly that hop.
    #[test]
    fn floors_follow_the_pool_and_not_the_stale_quote_in_the_plan() {
        let t = Trader {
            exec: unreachable_executor(),
            opts: TradeOptions { slippage_tenth_bps: 0, ..Default::default() },
            owner: Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
        };

        let mut p = tests::plan(2);
        let data = tests::pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];

        let honest = t.hops_for(&p, &data, &arrays).expect("a well formed plan");

        // Now claim, in the plan only, that every leg returns a hundred times more.
        // The pools handed to `hops_for` are unchanged, so nothing about what the chain
        // would actually pay has moved.
        for q in &mut p.leg_out {
            *q *= 100;
        }
        let inflated = t.hops_for(&p, &data, &arrays).expect("a well formed plan");

        assert_eq!(
            honest.iter().map(|h| h.min_amount_out).collect::<Vec<_>>(),
            inflated.iter().map(|h| h.min_amount_out).collect::<Vec<_>>(),
            "a floor moved when only the plan's stale quote changed — it is being read \
             from the plan rather than priced against the pool"
        );
    }

    /// The chaining rule still has to hold once the numbers come from the pool: each
    /// hop spends exactly what the hop before it guarantees, so none can be underfunded.
    #[test]
    fn each_hop_still_spends_exactly_what_the_previous_one_guarantees() {
        let t = Trader {
            exec: unreachable_executor(),
            opts: TradeOptions { slippage_tenth_bps: 100, ..Default::default() },
            owner: Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
        };
        let p = tests::plan(3);
        let hops = t
            .hops_for(&p, &tests::pools_for(&p), &vec![[Pubkey::new_unique(); 3]; 3])
            .expect("a well formed plan");

        assert_eq!(u128::from(hops[0].amount_in), p.amount_in);
        for w in hops.windows(2) {
            assert_eq!(
                w[1].amount_in, w[0].min_amount_out,
                "a hop must spend exactly what the one before it guaranteed"
            );
        }
    }

    /// A venue with no encoder has no quote math reachable here either. It must fail
    /// loudly rather than fall back to the stale number, which is how the original bug
    /// would quietly reappear.
    #[test]
    fn a_venue_without_an_encoder_cannot_be_re_priced() {
        assert!(Trader::fresh_quote(
            Dex::RaydiumAmmV4,
            [7u8; 32],
            &[0u8; 300],
            &[1u8; 32],
            2_500,
            1_000_000,
        )
        .is_none());
    }
}
