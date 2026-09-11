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
use cb_core::path::Leg;
use cb_core::types::{Dex, Pubkey32};
use cb_executor::encode::{pk, programs, to_pubkey};
use cb_executor::pda::associated_token_address;
use cb_executor::route::{self, Hop, RouteOptions, WsolPolicy};
use cb_executor::venue::raydium::BitmapPolicy;
use cb_executor::venue::VenueExtra;
use cb_executor::{ticks, tx, Attempt, Executor, Plan};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

/// Whether a candidate is being traded or only measured.
///
/// The executable filters in `main` reject far more than they accept, and each rejection
/// is a claim about what would have happened. `Measure` is how those claims get tested:
/// the candidate is built and re-priced exactly as a trade would be, the chain is asked
/// what it thinks, and the answer is recorded instead of acted on. It costs an RPC round
/// trip and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Submit it if the chain agrees it profits.
    Trade,
    /// Find out what the chain thinks and stop, whatever it says.
    Measure,
}

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

/// Whether [`cb_executor::venue::build_swap`] can encode a swap on this venue.
///
/// One definition rather than a `matches!` repeated at each of the places that need to
/// know. The two used to be written out separately and adding a venue meant finding
/// every one of them; missing a single site does not fail to compile, it produces a
/// plan the router accepts and the encoder then refuses at the last moment.
#[must_use]
pub const fn has_encoder(dex: Dex) -> bool {
    matches!(dex, Dex::OrcaWhirlpool | Dex::RaydiumClmm | Dex::RaydiumAmmV4)
}

/// Whether a venue prices in ticks, and so needs tick arrays resolved before a swap
/// on it can be built.
///
/// Raydium AMM v4 is constant-product: its whole state is the pool account and two
/// vault balances, there is no tick to be in and nothing to resolve.
#[must_use]
pub const fn is_concentrated(dex: Dex) -> bool {
    matches!(dex, Dex::OrcaWhirlpool | Dex::RaydiumClmm)
}

/// Whether a cycle on this venue can be re-priced from the accounts an attempt
/// already fetches.
///
/// Separate from [`has_encoder`] because they are separate facts, and Raydium AMM v4
/// is currently one and not the other. Every floor in a route comes from re-pricing
/// the hop against state fetched moments before signing — the fix that made this bot
/// able to land anything at all — and for the concentrated venues the pool account
/// carries everything that needs, so one batched read serves both purposes.
///
/// A v4 pool's reserves are not in its pool account. They are the balances of two
/// separate vault accounts, minus the protocol fees accrued in them, and the attempt
/// path fetches one account per pool. Until it fetches three for this venue, a v4
/// cycle can be *built* — the encoder is verified against all five pools — and cannot
/// be *priced*, so it is refused before anything is fetched rather than after.
#[must_use]
pub const fn can_reprice(dex: Dex) -> bool {
    matches!(dex, Dex::OrcaWhirlpool | Dex::RaydiumClmm)
}

impl CyclePlan {
    /// Whether every venue in this cycle has an encoder.
    #[must_use]
    pub fn encodable(&self) -> bool {
        self.pools.iter().all(|(_, d)| has_encoder(*d))
    }

    /// The venue that stops this cycle being encodable, if any.
    #[must_use]
    pub fn blocking_venue(&self) -> Option<Dex> {
        self.pools.iter().find(|(_, d)| !has_encoder(*d)).map(|(_, d)| *d)
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

/// The widest per-hop output floor discount this will ever choose, in tenths of a
/// basis point.
///
/// Six basis points. Not a tolerance for losing money — the route still refuses to
/// build unless its last floor beats its first input, whatever the haircut — but a
/// bound on how much of each hop's output is left behind in the intermediate token
/// account, since each hop spends what the one before it *guaranteed* rather than what
/// it delivered. At 6 bps a $9 trade strands about half a cent, in the intermediate
/// mint, recoverable. The edge here rarely supports more than 1.5 bps anyway.
const MAX_HAIRCUT_TENTH_BPS: u32 = 60;

/// The Solana base fee for a one-signature transaction, in lamports.
///
/// Only matters when the profit is measured in lamports — a wSOL cycle wraps and
/// closes inside the transaction, so the fee comes out of the very balance the profit
/// is read from. A cycle based on a token pays its fee from lamports instead, which
/// the token balance never sees.
///
/// **That the simulation charges it at all was measured, not assumed.** A one-signature
/// transaction was simulated against mainnet with `sigVerify: false` and the fee payer
/// requested in the accounts array: the balance came back 132,741,877 against a real
/// 132,746,877, exactly 5,000 lamports lighter. So `min_post_balance` is compared
/// against a figure the fee has already been taken out of, and a route whose margin
/// does not cover it cannot pass however good the price is. Checking this mattered —
/// had simulation been fee-free, demanding the headroom would have refused good trades
/// for nothing.
pub const BASE_FEE_LAMPORTS: u128 = 5_000;

/// What the priority bid will cost, in lamports.
///
/// Solana charges the bid against the compute limit the transaction *requests*, not the
/// units it goes on to consume, so this is knowable before the trade is built — and a
/// generous limit is not free, it is a proportionally larger bid.
///
/// It has to be in the headroom for the same reason the base fee is. A route is allowed
/// to build only when what its last hop guarantees exceeds what its first hop spends by
/// more than the transaction costs; leaving the bid out of that sum would authorise
/// trades whose guaranteed gain is smaller than the fee collected for delivering it.
#[must_use]
pub fn priority_fee_lamports(micro_lamports_per_cu: u64, compute_units: u32) -> u128 {
    let total = u128::from(micro_lamports_per_cu) * u128::from(compute_units);
    // Rounded up: the chain does not discount the fraction, and rounding a cost down is
    // the direction that overstates what a trade can afford.
    total.div_ceil(1_000_000)
}

/// Where one hop's predicted tick-array candidates sit inside the batched fetch: the
/// offset of the first one, and the addresses asked for, in sweep order. `None` when
/// this pool has never been decoded here and there was nothing to predict from.
type Prefetched = Option<(usize, Vec<(i32, Pubkey)>)>;

/// The program that owns a venue's pools and tick arrays.
/// The program that owns a venue's pool accounts.
///
/// Written out per venue rather than defaulting, because the catch-all this replaced
/// answered "Raydium CLMM" for every venue that was not Orca — correct only while
/// those were the sole two callers. The address is used to decide whether a tick array
/// exists by checking its owner, so a wrong answer reads "no arrays here" and refuses
/// the trade rather than failing in a way that names the cause.
fn program_for(dex: Dex) -> Pubkey {
    match dex {
        Dex::OrcaWhirlpool => pk(cb_dex::orca_whirlpool::PROGRAM_ID),
        Dex::RaydiumAmmV4 => pk(cb_dex::raydium_v4::PROGRAM_ID),
        _ => pk(cb_dex::raydium_clmm::PROGRAM_ID),
    }
}

/// The pool's own token A, read from the account rather than from the registry.
fn mint_a_of(dex: Dex, data: &[u8]) -> Result<Pubkey32> {
    match dex {
        Dex::OrcaWhirlpool => Ok(cb_dex::orca_whirlpool::decode(data)?.mint_a),
        Dex::RaydiumClmm => Ok(cb_dex::raydium_clmm::decode(data)?.mint_0),
        Dex::RaydiumAmmV4 => Ok(cb_dex::raydium_v4::decode_amm_info(data)?.base_mint),
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
    /// The two vault accounts a constant-product pool prices from, per pool.
    ///
    /// They are written into the pool account and a swap never moves them, so one
    /// lookup is good for the life of the pool. Caching them is what makes Raydium AMM
    /// v4 re-priceable at all: the balances have to arrive in the *same* round trip as
    /// the pool account, and their addresses are only knowable by first decoding that
    /// account. Learning them costs the pool one refused attempt, once, ever.
    vaults_seen: HashMap<Pubkey32, [Pubkey; 2]>,
    /// Mints the classic token program does not own.
    ///
    /// Registry configuration, not chain state, so it is loaded once and never
    /// refreshed: a mint cannot change which program owns it. Anything absent is
    /// treated as classic, which is the safe direction — a classic assumption on a
    /// Token-2022 mint derives an address the program will reject, while the reverse
    /// would build a v2 instruction a classic pool has no handler for.
    token_2022_mints: std::collections::HashSet<Pubkey32>,
    /// Mints the wallet already holds a token account for.
    ///
    /// # Why a cycle through anything else cannot profit, at any edge
    ///
    /// A route creates the accounts it touches, idempotently, and an account that does
    /// not exist yet costs [`route::TOKEN_ACCOUNT_RENT`] — 2,039,280 lamports — to
    /// create. On a wSOL cycle the profit is read from the owner's *lamport* balance,
    /// and the rent comes out of that same balance, so the simulation's post-balance
    /// check fails by the price of the rent no matter how good the trade was.
    ///
    /// That is the correct behaviour and it was invisible: the refusal arrived as a
    /// balance comparison with no reason attached, after two round trips. Knowing the
    /// set up front turns it into one line, spent nothing, and says what opening it
    /// would cost — which at this book is 1.68% of the wallet per mint and therefore a
    /// decision somebody should make on purpose.
    ///
    /// Empty means never measured, which is deliberately not the same as "the wallet
    /// holds nothing": an unmeasured set refuses nothing and the old behaviour stands.
    accounts_held: std::collections::HashSet<Pubkey32>,
}

impl Trader {
    #[must_use]
    pub fn new(exec: Executor, opts: TradeOptions) -> Self {
        let owner = exec.pubkey();
        Self {
            exec,
            opts,
            owner,
            ticks_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        }
    }

    /// Tell this trader which mints belong to Token-2022, from the registry.
    ///
    /// Separate from `new` so the executor does not have to know what a registry is,
    /// and so a caller that never learns keeps the old behaviour exactly: every mint
    /// classic, every swap the v1 instruction.
    pub fn set_token_2022_mints(&mut self, mints: impl IntoIterator<Item = Pubkey32>) {
        self.token_2022_mints = mints.into_iter().collect();
    }

    /// Read, in one round trip, which of these mints the wallet already has an account
    /// for, and remember it. See [`Trader::accounts_held`].
    ///
    /// Configuration-like rather than state-like: an account that exists is not going
    /// to stop existing, and one the wallet opens later is picked up on the next start.
    /// Returns the mints it has no account for, so the caller can say what they cost.
    ///
    /// # Errors
    /// If the chain cannot be reached.
    pub async fn learn_token_accounts(&mut self, mints: &[Pubkey32]) -> Result<Vec<Pubkey32>> {
        let wsol = pk(programs::WSOL_MINT);
        let wanted: Vec<Pubkey32> =
            mints.iter().copied().filter(|m| to_pubkey(m) != wsol).collect();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let atas: Vec<Pubkey> = wanted
            .iter()
            .map(|m| associated_token_address(&self.owner, &to_pubkey(m), &self.token_program_of(m)))
            .collect();
        let found = self.exec.rpc.accounts_full(&atas).await?;

        let mut missing = Vec::new();
        // wSOL is always held in the sense that matters: `WrapAndClose` opens and closes
        // it inside the transaction, so it never needs to exist beforehand.
        self.accounts_held.insert(*pk(programs::WSOL_MINT).as_array());
        for (mint, acc) in wanted.iter().zip(found.iter()) {
            if acc.is_some() {
                self.accounts_held.insert(*mint);
            } else {
                missing.push(*mint);
            }
        }
        Ok(missing)
    }

    /// The first mint of this cycle the wallet cannot hold without paying rent.
    fn mint_without_an_account(&self, plan: &CyclePlan) -> Option<Pubkey32> {
        if self.accounts_held.is_empty() {
            return None;
        }
        plan.mints.iter().find(|m| !self.accounts_held.contains(*m)).copied()
    }

    /// The program that owns a mint, as an address the encoder can use directly.
    fn token_program_of(&self, mint: &Pubkey32) -> Pubkey {
        if self.token_2022_mints.contains(mint) {
            pk(programs::SPL_TOKEN_2022)
        } else {
            pk(programs::SPL_TOKEN)
        }
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
    /// `vaults` carries the two vault balances a constant-product venue prices from,
    /// and is `None` for a concentrated one, whose whole state is the pool account.
    fn fresh_leg(
        dex: Dex,
        address: Pubkey32,
        data: &[u8],
        input_mint: &Pubkey32,
        fee_ppm: u32,
        vaults: Option<(u64, u64)>,
    ) -> Option<Leg> {
        let state = match dex {
            Dex::OrcaWhirlpool => cb_dex::orca_whirlpool::to_pool_state(address, data, 0).ok()?,
            Dex::RaydiumClmm => {
                cb_dex::raydium_clmm::to_pool_state(address, data, fee_ppm, 0).ok()?
            }
            // The pool account holds the fee, the mints and the uncollected protocol
            // fees; the price lives entirely in two SPL token accounts it points at.
            // Both have to be read in the same round trip as the pool or this is not a
            // re-price, it is two prices from two moments — the failure this whole file
            // exists to stop making.
            Dex::RaydiumAmmV4 => {
                let (base, quote) = vaults?;
                let info = cb_dex::raydium_v4::decode_amm_info(data).ok()?;
                cb_dex::raydium_v4::to_pool_state(address, &info, base, quote, 0).ok()?
            }
            _ => return None,
        };
        state.leg_for_input(input_mint)
    }

    /// Run a candidate haircut through the whole route, returning the last hop's quote
    /// and the floor derived from it.
    ///
    /// The two differ by exactly one hop's haircut, which is the number that decides
    /// whether the trade can pay its own fee — see [`Trader::widest_affordable_haircut`].
    fn floor_chain(legs: &[Leg], spend: u128, tenth_bps: u32) -> Option<(u128, u128)> {
        let mut cur = spend;
        let mut last = (0u128, 0u128);
        for leg in legs {
            let quoted = leg.quote(cur)?;
            let floor = haircut(quoted, tenth_bps);
            if floor == 0 {
                return None;
            }
            last = (quoted, floor);
            cur = floor;
        }
        Some(last)
    }

    /// The most generous per-hop haircut this route can still afford.
    ///
    /// # Why a fixed number could not work
    ///
    /// The haircut is pulled in two directions at once and a constant satisfies
    /// neither.
    ///
    /// Downward, because the route only builds while its last floor beats its first
    /// input: for `n` hops at edge `e` that needs `n·s < e`, and this market's edge is
    /// one to two basis points.
    ///
    /// Upward, because of how the profit is checked. Each hop is instructed to spend
    /// exactly what the one before it *guaranteed*, not what it delivered, so the route
    /// ends holding the last hop's quote while having guaranteed only that quote less
    /// one haircut. The simulation must show a balance clearing `pre + guaranteed`, and
    /// on a wSOL cycle the transaction fee comes out of that same balance. So the whole
    /// margin between what arrives and what was promised — `spend × s`, **one** hop's
    /// worth, not `n` — has to cover the fee.
    ///
    /// At $9.20 the fee is 5,000 lamports against an input of about 89,000,000, so `s`
    /// must be at least 0.56 bps. The shipped value was 0.3. Every trade that survived
    /// long enough to be checked failed there, and nothing in the logs said so, because
    /// a balance that misses its floor is not an error the chain reports — it is a
    /// number this code compares and rejects.
    ///
    /// So the haircut is chosen per trade: as wide as the edge will bear, which is
    /// simultaneously the widest margin for the fee, the lowest floors for the AMMs to
    /// satisfy, and the most tolerance for the price moving before it lands. `None`
    /// when no width works, which is the honest answer for an edge that cannot cover
    /// its own costs.
    /// The widest haircut whose final floor still beats the input, ignoring fees.
    ///
    /// `None` when the edge cannot support any width at all — which is not this
    /// function's business to explain. Left to [`cb_executor::route::build`], whose
    /// refusal names the two amounts and is the one an operator can read.
    fn widest_buildable_haircut(legs: &[Leg], spend: u128, floor_tenth_bps: u32) -> Option<u32> {
        // Wider is better on every axis, and the floor falls monotonically as the
        // haircut grows, so the first hit walking down is the answer. The ceiling never
        // sits below a deliberately configured floor.
        let ceiling = MAX_HAIRCUT_TENTH_BPS.max(floor_tenth_bps);
        (floor_tenth_bps.max(1)..=ceiling)
            .rev()
            .find(|t| Self::floor_chain(legs, spend, *t).is_some_and(|(_, f)| f > spend))
    }

    /// Every leg of this cycle as the chain has it right now.
    fn fresh_legs(
        plan: &CyclePlan,
        pool_data: &[Vec<u8>],
        vaults: &[Option<(u64, u64)>],
    ) -> Result<Vec<Leg>> {
        (0..plan.pools.len())
            .map(|i| {
                Self::fresh_leg(
                    plan.pools[i].1,
                    plan.pools[i].0,
                    &pool_data[i],
                    &plan.mints[i],
                    plan.fee_ppm[i],
                    vaults.get(i).copied().flatten(),
                )
                .with_context(|| {
                    format!("hop {i} could not be re-priced against the state just fetched")
                })
            })
            .collect()
    }

    /// The token balance inside a vault account.
    ///
    /// Deferred to the venue crate, which owns the layout and already has the offset
    /// written down once. A second copy of `64` in this file is a second place to get
    /// it wrong.
    fn spl_amount(data: &[u8]) -> Option<u64> {
        cb_dex::raydium_v4::decode_token_amount(data).ok()
    }

    /// Build the hops for a cycle, with each hop funded by the previous one's floor.
    ///
    /// Returns the hops and the input actually used, which may be **smaller** than the
    /// plan asked for — see the sizing note below. The caller must scale its USD
    /// figures by the ratio before handing them to the risk gate.
    ///
    /// # Errors
    /// If the plan is malformed, if a hop cannot be re-priced against the state just
    /// fetched, or if a hop's floor collapses to zero under slippage.
    pub fn hops_for(
        &self,
        plan: &CyclePlan,
        pool_data: &[Vec<u8>],
        arrays: &[[Pubkey; 3]],
        vaults: &[Option<(u64, u64)>],
        fee_headroom: u128,
    ) -> Result<(Vec<Hop>, u128)> {
        let n = plan.pools.len();
        if n < 2 || plan.mints.len() != n + 1 || plan.leg_out.len() != n {
            bail!(
                "malformed cycle plan: {n} pools, {} mints, {} quotes",
                plan.mints.len(),
                plan.leg_out.len()
            );
        }
        if pool_data.len() != n || arrays.len() != n {
            bail!(
                "have {} accounts and {} array sets for {n} pools",
                pool_data.len(),
                arrays.len()
            );
        }

        // Trade what the pools can carry now, not what they could carry at detection.
        //
        // A concentrated leg only quotes inside its current tick interval, and the room
        // left in that interval is whatever sits between the price and the next
        // boundary — which moves continuously and can be a few cents. The plan's
        // `amount_in` was sized against the interval as it stood when the websocket
        // last spoke, so by the time the accounts are re-read the leg often cannot
        // honour it.
        //
        // The old answer was to refuse the whole trade, and that was throwing away
        // **thirty-five of ninety-one attempts** over six hours of live running — more
        // than a third of everything that reached the chain — with "hop N could not be
        // re-priced". Nothing was wrong with those cycles. They were profitable and
        // simply larger than the pool's remaining room.
        //
        // Sizing down is safe in a way that raising the size would not be: every floor
        // below is still derived from the fresh quote at the size actually used, the
        // route still refuses to build unless its last floor beats its first input, and
        // the simulation still has the final say. A smaller trade earns less; it cannot
        // earn something that is not there.
        let legs = Self::fresh_legs(plan, pool_data, vaults)?;
        let spend_total = cb_core::path::largest_feasible(&legs, plan.amount_in);
        if spend_total == 0 {
            bail!(
                "no hop of this cycle can carry any size at all against the state just \
                 fetched — the tightest leg has no room left in its tick"
            );
        }

        // Two different failures, and only one of them belongs here.
        //
        // If no width builds at all the edge is simply gone, and `route::build` says so
        // far better than this could — it names what the route spends against what it
        // guarantees. Fall through to it on the configured floor.
        //
        // If a width builds but none of them leaves enough margin for the fee, that is
        // this function's own arithmetic and nothing downstream will explain it: the
        // trade would be assembled, simulated, and then rejected by a balance
        // comparison that reports no reason at all. Say it here instead.
        let tenth_bps = match Self::widest_buildable_haircut(
            &legs,
            spend_total,
            self.opts.slippage_tenth_bps,
        ) {
            None => self.opts.slippage_tenth_bps,
            Some(widest) => {
                let (quoted, floor) = Self::floor_chain(&legs, spend_total, widest)
                    .context("the chosen floor stopped chaining")?;
                let margin = quoted.saturating_sub(floor);
                if margin < fee_headroom {
                    bail!(
                        "the widest floor this edge allows leaves {margin} base units between \
                         what the last hop delivers and what it guarantees, and the \
                         transaction fee needs {fee_headroom} of that — the gain does not \
                         cover the cost of collecting it"
                    );
                }
                widest
            }
        };

        let mut hops = Vec::with_capacity(n);
        let mut spend = spend_total;
        let mut drift_bps: Vec<(&'static str, f64)> = Vec::with_capacity(n);
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
            let fresh = legs[i].quote(spend).with_context(|| {
                format!(
                    "hop {i} could not be re-priced at {spend} against the state just \
                     fetched, though the whole route was sized to fit"
                )
            })?;

            let floor = haircut(fresh, tenth_bps);
            if floor == 0 {
                bail!("hop {i} floors at zero after {tenth_bps} tenths of a bp of slippage");
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
                input_token_program: self.token_program_of(&plan.mints[i]),
                output_token_program: self.token_program_of(&plan.mints[i + 1]),
                amount_in,
                min_amount_out,
                tick_arrays: arrays[i],
            });

            // Say how far this leg moved between the detection and this quote, per leg,
            // named by its venue.
            //
            // Everything upstream can only report that the route as a whole no longer
            // pays. That is the one fact that does not help: a cycle is two or three
            // legs, they come from different venues over different subscriptions, and
            // "the edge is gone" is equally consistent with both legs drifting a little
            // and with one venue's state being wrong every time. Eleven hours of running
            // could not tell those apart, because nothing recorded the legs separately.
            //
            // Rates rather than amounts, because `spend_total` is re-sized against the
            // room the pools have now and the raw outputs are therefore not comparable.
            // A rate is, to first order, and the second order is the price impact of a
            // size change this small.
            let planned_in = if i == 0 { plan.amount_in } else { plan.leg_out[i - 1] };
            if planned_in > 0 && spend > 0 {
                let planned_rate = plan.leg_out[i] as f64 / planned_in as f64;
                let fresh_rate = fresh as f64 / spend as f64;
                if planned_rate > 0.0 {
                    drift_bps.push((
                        plan.pools[i].1.name(),
                        (fresh_rate / planned_rate - 1.0) * 10_000.0,
                    ));
                }
            }

            // The next hop spends exactly what this one guarantees. See the module docs.
            spend = floor;
        }
        if !drift_bps.is_empty() {
            tracing::info!(
                "leg drift since detection: {}",
                drift_bps
                    .iter()
                    .map(|(venue, bps)| format!("{venue} {bps:+.2} bps"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            );
        }
        Ok((hops, spend_total))
    }

    /// Fetch state, build, simulate, and — depending on `intent` — submit.
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
        intent: Intent,
    ) -> Result<Attempt> {
        // Cheapest and most fatal first, same principle as the gate itself: a halted
        // run used to still pay for a full account fetch, tick-array resolution, and
        // a blockhash — several RPC round trips — only to be refused at the very last
        // step inside `execute()`. Checked here too so a halt costs nothing while it
        // stands, however long the cooldown before it clears itself.
        self.exec.gate.tick_auto_resume();
        if intent == Intent::Trade {
            if let Some(why) = self.exec.gate.halted() {
                return Ok(Attempt::Refused(format!("trading is halted: {why}")));
            }
        }

        // A cycle whose intermediate mint the wallet has no account for cannot clear
        // its own profit check, because creating that account costs rent out of the
        // very balance the profit is measured in. Said here, once, for nothing, rather
        // than discovered two round trips later as an unexplained balance shortfall.
        if let Some(mint) = self.mint_without_an_account(plan) {
            return Ok(Attempt::Refused(format!(
                "the wallet holds no token account for {}, and opening one costs {} lamports \
                 of rent out of the same balance this trade's profit is measured in — add \
                 the mint to extra_token_mints to pay that deposit on purpose",
                to_pubkey(&mint),
                route::TOKEN_ACCOUNT_RENT
            )));
        }

        if let Some(dex) = plan.blocking_venue() {
            return Ok(Attempt::Refused(format!("{} has no encoder", dex.name())));
        }
        // Before the fetch, for the same reason the halt check is: the alternative is
        // paying for a full account read and then failing inside `fresh_legs` with
        // "could not be re-priced against the state just fetched", which blames the
        // market for a feature that is not written yet.
        if let Some((_, dex)) = plan.pools.iter().find(|(_, d)| !can_reprice(*d)) {
            return Ok(Attempt::Refused(format!(
                "{} can be encoded but not yet re-priced — its reserves are two vault \
                 accounts this attempt does not fetch",
                dex.name()
            )));
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
        let profit_key = if wrapping {
            self.owner
        } else {
            associated_token_address(&self.owner, &base_mint, &token_program)
        };

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

        // Vault balances for the constant-product hops, in this same round trip.
        //
        // A pool whose vaults are not cached yet contributes nothing here and is
        // learned from its own account below, which costs it one attempt the first
        // time it is ever seen and nothing afterwards.
        let mut vault_at: Vec<Option<usize>> = vec![None; n];
        for (i, (pool_raw, dex)) in plan.pools.iter().enumerate() {
            if is_concentrated(*dex) {
                continue;
            }
            if let Some(v) = self.vaults_seen.get(pool_raw) {
                vault_at[i] = Some(keys.len());
                keys.push(v[0]);
                keys.push(v[1]);
            }
        }

        let (fetched, (blockhash, _)) =
            tokio::try_join!(rpc.accounts_full(&keys), rpc.latest_blockhash())?;
        let mut pool_data = Vec::with_capacity(n);
        for (key, acc) in keys.iter().zip(fetched.iter()).take(n) {
            let Some(a) = acc.as_ref() else {
                return Ok(Attempt::Refused(format!(
                    "pool {key} vanished between sweep and build"
                )));
            };
            pool_data.push(a.data.clone());
        }
        let profit_account = fetched.get(n).and_then(Option::as_ref);

        // Turn the fetched vault accounts into balances, and learn the addresses of any
        // we did not have. The learning branch refuses rather than fetching again: a
        // second round trip is exactly the latency this path is built to avoid, and the
        // cycle will come round again within a slot or two already knowing.
        let mut vaults: Vec<Option<(u64, u64)>> = vec![None; n];
        let mut unlearned: Option<Pubkey> = None;
        for (i, (pool_raw, dex)) in plan.pools.iter().enumerate() {
            if is_concentrated(*dex) {
                continue;
            }
            match vault_at[i] {
                Some(at) => {
                    let read = |k: usize| {
                        fetched.get(k).and_then(Option::as_ref).and_then(|a| Self::spl_amount(&a.data))
                    };
                    match (read(at), read(at + 1)) {
                        (Some(base), Some(quote)) => vaults[i] = Some((base, quote)),
                        _ => {
                            return Ok(Attempt::Refused(format!(
                                "pool {} priced from vaults that did not read back as token \
                                 accounts",
                                to_pubkey(pool_raw)
                            )))
                        }
                    }
                }
                None => {
                    if let Ok(info) = cb_dex::raydium_v4::decode_amm_info(&pool_data[i]) {
                        self.vaults_seen.insert(
                            *pool_raw,
                            [to_pubkey(&info.base_vault), to_pubkey(&info.quote_vault)],
                        );
                        unlearned = Some(to_pubkey(pool_raw));
                    }
                }
            }
        }
        if let Some(pool) = unlearned {
            return Ok(Attempt::Refused(format!(
                "learned where pool {pool} keeps its reserves; the next sweep can price it \
                 without a second round trip"
            )));
        }

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

            // A constant-product hop has no tick to be in and no arrays to find, so it
            // skips this whole apparatus — including the hint cache, which exists to
            // predict a tick and would only ever hold a meaningless one for such a
            // pool. The slot is filled rather than left empty because the `flatten`
            // below drops `None`s silently, and a dropped entry would shift every later
            // hop's arrays onto the wrong pool. The value is inert: this venue's
            // encoder never reads it.
            if !is_concentrated(*dex) {
                arrays[i] = Some([pool; 3]);
                continue;
            }

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

        // What the last hop's margin must cover before the profit check can pass. On a
        // wSOL cycle the fee leaves the same balance the profit is read from, so the
        // margin has to absorb it; on a token cycle it does not.
        //
        // A quarter over the fee, and the quarter is measured rather than chosen. This
        // was first set at double, on the median *detected* edge of 2.81 bps — which is
        // the wrong distribution, and the mistake this file's history is mostly made of.
        // What decides whether a trade can be built is the edge it still has when
        // re-priced against fresh state, and over nineteen hours that number reached
        // 1.93 bps at its very best. Every one of the twenty-one moments that re-priced
        // positive, against the margin its own edge could produce:
        //
        // | re-priced edge | margin it allows | 1.00× | 1.25× | 2.00× |
        // |----------------|------------------|-------|-------|-------|
        // | 1.93 bps       | 7,033 lamports   | ok    | ok    | fails |
        // | 1.69 bps       | 7,461            | ok    | ok    | fails |
        // | 1.61 bps       | 7,003            | ok    | ok    | fails |
        // | 1.49 bps       | 6,564            | ok    | ok    | fails |
        // | the other 17   | under 5,000      | fails | fails | fails |
        //
        // At double, **none of them build**. At a quarter over, the same four do as at
        // the bare fee, with a cushion for the price moving between the re-price and
        // the simulation instead of none at all. Raising it further buys nothing that
        // exists to be bought.
        //
        // The priority bid joins it for the same reason, and it is not optional in this
        // market: `getRecentPrioritizationFees` over 150 slots on the very pools traded
        // here charged something in 104 of them, median 2,418 micro-lamports per compute
        // unit. This bot bid zero, and its first ever submission —
        // `5tQg161Zf4dGFc3s...`, 15:07:16 UTC — came back with a signature and was never
        // included. A transaction nobody has a reason to pick up is not cheap, it is
        // free and worthless.
        let priority =
            priority_fee_lamports(self.opts.priority_micro_lamports, self.opts.compute_units);
        let fee_headroom = if wrapping { (BASE_FEE_LAMPORTS + priority) * 5 / 4 } else { 0 };
        let (hops, spent) = match self.hops_for(plan, &pool_data, &arrays, &vaults, fee_headroom) {
            Ok(h) => h,
            Err(e) => return Ok(Attempt::Refused(e.to_string())),
        };

        // The route may have been sized down to what the pools can still carry, so the
        // figures the risk gate judges have to follow it down.
        //
        // Scaled linearly, which understates the profit rather than overstating it:
        // cycle profit is concave in size and zero at zero, so a fraction `k` of the
        // input returns *at least* `k` times the profit. Being wrong in that direction
        // costs a refused trade; being wrong in the other direction is how a gate stops
        // meaning anything.
        let shrink = if plan.amount_in > 0 {
            spent as f64 / plan.amount_in as f64
        } else {
            return Ok(Attempt::Refused("this cycle plans to spend nothing".into()));
        };
        let size_usd = size_usd * shrink;
        let expected_net_usd = expected_net_usd * shrink;

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
            //
            // Measured against what the wrap will *actually* move, which is the first
            // hop's input after any sizing down rather than the plan's original figure.
            // Checking the larger number refused trades the wallet could comfortably
            // afford.
            let distinct_mints = plan.mints.iter().collect::<std::collections::HashSet<_>>().len();
            let amount_in_u64 = u64::try_from(spent).unwrap_or(u64::MAX);
            if let Some(reserved) = wrap_shortfall(amount_in_u64, distinct_mints, bal) {
                return Ok(Attempt::Refused(format!(
                    "wrapping {amount_in_u64} lamports would leave less than the                      {reserved} lamports this transaction needs for account rent and                      fees, against a balance of {bal} — sizing must leave that headroom,                      not spend into it"
                )));
            }
            bal
        } else {
            let held = profit_account
                .and_then(|a| a.data.get(64..72).and_then(|b| b.try_into().ok()))
                .map(u64::from_le_bytes)
                .unwrap_or(0);

            // A cycle the wallet cannot actually fund is refused here rather than sent
            // to fail on chain.
            //
            // Sizing knows `capital_usd`; it does not know which *mint* that capital is
            // in. This wallet holds SOL and 0.0079 USDC, so every cycle entered at USDC
            // or USDT — 195 of the 232 that cleared every other gate over nineteen
            // hours — was asking to spend nine dollars of a token it does not have.
            // Each would have failed in simulation with an insufficient-funds error
            // from the token program, been counted as a defect rather than a shortfall,
            // and taken the run a strike closer to a halt.
            //
            // The same loop is almost always reachable from SOL too, entered from the
            // other side of the same pools. Refusing the unfundable entry frees the
            // sweep's one attempt for the one that can be paid for.
            if u128::from(held) < spent {
                return Ok(Attempt::Refused(format!(
                    "this cycle starts by spending {spent} of a mint the wallet holds {held} \
                     of — the same loop entered from SOL can be funded, this one cannot"
                )));
            }
            held
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
        match intent {
            Intent::Trade => {
                plan_to_run.execute(&mut self.exec.gate, rpc, self.opts.dry_run).await
            }
            // No branch here reaches `Rpc::send`. See `Plan::probe`.
            Intent::Measure => Ok(Attempt::Probed(plan_to_run.probe(rpc).await?)),
        }
    }

    /// The most token accounts this will ever open in one go.
    ///
    /// Two was what a SOL/stable book needs — USDC and USDT — and the cap existed so a
    /// mistake in what gets passed in could not spend more than about forty cents of
    /// refundable deposit. Raised to five on 2026-09-10 to let the book reach the
    /// intermediate mints named in `extra_token_mints`, which is the only way a cycle
    /// through one of them can ever pass its profit check.
    ///
    /// Five accounts is 10,196,400 lamports, about $1.05, and it is a deposit rather
    /// than a spend: closing an account returns it in full. What it genuinely costs is
    /// the trading capital it stands in while it is parked, which is the reason for a
    /// cap at all.
    const MAX_ACCOUNTS_TO_OPEN: usize = 5;

    /// Open a token account for each of `mints` the wallet does not already own, once,
    /// before any trade has to pay for one mid-flight.
    ///
    /// # Why this is not housekeeping
    ///
    /// A cycle's profit is read from the owner's *lamport* balance, and the floor it
    /// must clear is `pre_balance + the gain the route guarantees`. Opening an
    /// associated token account costs 2,039,280 lamports of rent out of that same
    /// balance. So a trade that has to open one is asking the simulation to show a gain
    /// of about twenty-one cents, on a cycle worth a tenth of a cent.
    ///
    /// Measured on this wallet: the USDC account existed and the **USDT account did
    /// not**, while roughly seventy per cent of every opportunity that cleared all the
    /// other gates ran SOL↔USDT. Those trades could never have passed the balance check
    /// — short by a factor of 383 — however good the price was. No log line said so,
    /// because the transaction was abandoned before anything was submitted.
    ///
    /// Paying it here instead changes what the money is. Inside a trade it is a cost the
    /// trade cannot cover; paid once up front it is a deposit that sits in the account
    /// and comes back in full whenever the account is closed. Every trade afterwards
    /// emits the same idempotent instruction and pays nothing.
    ///
    /// Nothing is sent that has not simulated cleanly first, as everywhere else here.
    ///
    /// # Errors
    /// If the chain cannot be reached. A refusal to open — because nothing is missing,
    /// or because more are missing than the cap allows — is not an error.
    pub async fn ensure_token_accounts(&self, mints: &[Pubkey32]) -> Result<Option<String>> {
        let wsol = pk(programs::WSOL_MINT);
        // wSOL is deliberately absent: `WrapAndClose` opens and closes it inside every
        // transaction, so its rent is borrowed and returned in the same breath.
        //
        // Each mint carries its own program from here on. Opening a Token-2022 mint's
        // account under the classic program does not fail — it derives a different
        // address, funds it, and leaves the account the trade actually needs still
        // missing, having spent the rent.
        let wanted: Vec<(Pubkey, Pubkey)> = mints
            .iter()
            .filter(|m| to_pubkey(m) != wsol)
            .map(|m| (to_pubkey(m), self.token_program_of(m)))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if wanted.is_empty() {
            return Ok(None);
        }

        let atas: Vec<Pubkey> = wanted
            .iter()
            .map(|(m, p)| associated_token_address(&self.owner, m, p))
            .collect();
        let existing = self.exec.rpc.accounts_full(&atas).await?;

        let missing: Vec<(Pubkey, Pubkey, Pubkey)> = wanted
            .iter()
            .zip(atas.iter())
            .zip(existing.iter())
            .filter(|(_, acc)| acc.is_none())
            .map(|(((m, p), a), _)| (*m, *a, *p))
            .collect();
        if missing.is_empty() {
            tracing::info!("every token account this book needs already exists");
            return Ok(None);
        }
        if missing.len() > Self::MAX_ACCOUNTS_TO_OPEN {
            anyhow::bail!(
                "{} token accounts are missing, over the {} this will open at once — \
                 refusing rather than spending {} lamports of rent unasked",
                missing.len(),
                Self::MAX_ACCOUNTS_TO_OPEN,
                missing.len() as u64 * route::TOKEN_ACCOUNT_RENT
            );
        }

        let cost = missing.len() as u64 * route::TOKEN_ACCOUNT_RENT;
        tracing::warn!(
            "opening {} token account(s) the wallet does not have yet, at {cost} lamports of \
             rent. This is a deposit, not a fee: it stays in the account and returns in full \
             if the account is ever closed. Without it every cycle through those mints fails \
             its profit check by the price of the rent.",
            missing.len()
        );

        // Generous: creating an associated token account costs on the order of 25,000
        // compute units and this runs once. A limit set too low reverts a transaction
        // that had nothing wrong with it, which is a silly way to stay blocked.
        let mut ixs = vec![tx::set_compute_limit(120_000)];
        for (mint, ata, program) in &missing {
            ixs.push(tx::create_ata_idempotent(&self.owner, ata, &self.owner, mint, program));
        }

        // Sent until it actually lands, unlike a trade.
        //
        // `Rpc::send` asks the node for three rebroadcasts of the same signed bytes
        // and then stops, because a stale arbitrage is worthless and chasing one is
        // worse than dropping it. This is the opposite case. There is no race, nothing
        // here goes stale but the blockhash, and the transaction only has to arrive —
        // so three rebroadcasts of one blockhash is not enough on its own.
        //
        // Reusing the fire-and-forget path cost a whole run. The first send returned a
        // signature, the account was reported open, and the transaction was never
        // included — `getSignatureStatuses` with full history search had never heard of
        // it, and the wallet's newest transaction was still nine days old. Every
        // SOL↔USDT cycle stayed blocked behind a wall the log said had come down. A
        // signature is a receipt for having asked.
        //
        // So: re-sign against a fresh blockhash each round, and believe nothing until
        // the chain confirms it.
        const ROUNDS: u32 = 4;
        let mut last_signature = None;
        for round in 1..=ROUNDS {
            let (blockhash, _) = self.exec.rpc.latest_blockhash().await?;
            let assembled = tx::assemble(&self.exec.wallet, &ixs, blockhash)?;

            let sim = self.exec.rpc.simulate(&assembled.tx_base64, &[]).await?;
            if !sim.succeeded() {
                let ctx = sim.error_context().unwrap_or_default();
                anyhow::bail!(
                    "opening the token accounts did not simulate cleanly, so nothing was \
                     sent: {} {ctx}",
                    sim.err.unwrap_or_else(|| "unknown".into())
                );
            }
            if self.opts.dry_run {
                tracing::info!(
                    "dry run — the token accounts simulated cleanly and were not opened"
                );
                return Ok(None);
            }

            let signature = self.exec.rpc.send(&assembled.tx_base64, true).await?;
            last_signature = Some(signature.clone());
            // Fifteen tries at two seconds is thirty seconds of patience, which is
            // generous for inclusion and costs nothing: this runs once, at startup,
            // before any sweep is waiting on it.
            match self.confirm(&signature, 15).await {
                Some(true) => {
                    tracing::warn!(
                        "opened {} token account(s), confirmed on chain: {signature}",
                        missing.len()
                    );
                    return Ok(Some(signature));
                }
                Some(false) => anyhow::bail!(
                    "the transaction opening the token accounts reverted on chain: {signature}"
                ),
                None if round < ROUNDS => tracing::warn!(
                    "{signature} has not been included; re-sending against a fresh blockhash \
                     ({round} of {ROUNDS})"
                ),
                None => {}
            }
        }
        anyhow::bail!(
            "sent the token-account transaction {ROUNDS} times and none was included; the last \
             was {}. Cycles through those mints stay blocked until one lands.",
            last_signature.unwrap_or_else(|| "—".into())
        )
    }

    /// Wait briefly for a submitted signature to reach the chain, and say what happened.
    ///
    /// Polls `tries` times at two-second intervals. A trade wants a small number
    /// because a sweep is waiting behind it; the once-per-run setup transaction wants a
    /// patient one, because nothing is waiting and it has to actually arrive.
    ///
    /// `Some(true)` landed cleanly, `Some(false)` landed and reverted, `None` still
    /// unknown when the wait ran out — which is not the same as failed and must not be
    /// reported as one.
    ///
    /// # Why this is worth blocking the sweep for
    ///
    /// A signature is a receipt for having asked, not for having been paid. Without
    /// this the log's last word on the most important event this instrument can produce
    /// is "submitted", and whether the money moved has to be looked up by hand in an
    /// explorer. Six seconds of a sweep is a cheap price for the run being able to
    /// answer that itself, and submissions are rare enough that it costs nothing in
    /// aggregate — this bot has produced none at all in its history to date.
    ///
    /// # Errors
    /// Never: an RPC that cannot answer is reported as "not yet known", the same as a
    /// signature that has not landed. Nothing here decides whether money moves, so a
    /// failure to look is not a failure to trade.
    pub async fn confirm(&self, signature: &str, tries: u32) -> Option<bool> {
        const GAP: std::time::Duration = std::time::Duration::from_millis(2000);
        for attempt in 0..tries {
            if attempt > 0 {
                tokio::time::sleep(GAP).await;
            }
            match self.exec.rpc.signature_status(signature).await {
                Ok(Some(status)) => return Some(status.landed_cleanly()),
                Ok(None) => {}
                Err(e) => tracing::debug!("could not read the status of {signature} yet: {e:#}"),
            }
        }
        None
    }

    /// Report an outcome to the risk gate. Called by the caller, because only it knows
    /// whether a signature actually landed.
    pub fn record(&mut self, outcome: cb_executor::risk::Outcome) {
        self.exec.gate.record(outcome);
    }

    /// Fold a confirmed profit or loss into the trade already recorded for `signature`.
    ///
    /// Separate from [`Trader::record`] because the trade is counted when it is sent and
    /// priced when the chain answers, and those are different moments. See
    /// [`cb_executor::risk::RiskGate::settle`].
    pub fn settle(&mut self, net_usd: f64) {
        self.exec.gate.settle(net_usd);
    }

    /// What one submission costs in lamports, whatever becomes of it.
    ///
    /// The base fee plus the priority bid, which Solana charges on the compute limit
    /// *requested* rather than the amount consumed. A transaction that lands and reverts
    /// pays this in full and returns nothing, which is precisely the loss the daily
    /// budget needs to be able to see.
    #[must_use]
    pub fn submission_cost_lamports(&self) -> u128 {
        BASE_FEE_LAMPORTS
            + priority_fee_lamports(self.opts.priority_micro_lamports, self.opts.compute_units)
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
        // v4 calls them base and quote rather than 0 and 1, and the swap's account
        // list fixes coin before pc, so base is this venue's token A.
        Dex::RaydiumAmmV4 => {
            let p = cb_dex::raydium_v4::decode_amm_info(data)?;
            (p.base_mint, p.quote_mint)
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

    /// The bid is charged on the limit requested, so the arithmetic has to be the
    /// chain's and not an approximation of it — and it must round *up*, because a cost
    /// rounded down is a route allowed to build on money it does not have.
    #[test]
    fn the_priority_bid_is_priced_on_the_limit_requested_and_never_rounded_down() {
        // Bidding nothing costs nothing: the state this bot shipped in, and the reason
        // its first submission was never included.
        assert_eq!(priority_fee_lamports(0, 400_000), 0);
        // The measured median on the pools traded here, at the configured limit.
        assert_eq!(priority_fee_lamports(2_500, 400_000), 1_000);
        // A generous compute limit is a proportionally larger bid, not a free one.
        assert_eq!(priority_fee_lamports(2_500, 800_000), 2_000);
        // Anything with a fraction goes up, never down.
        assert_eq!(priority_fee_lamports(1, 1), 1, "a fraction of a lamport still costs one");
        assert_eq!(priority_fee_lamports(1, 1_000_000), 1);
        assert_eq!(priority_fee_lamports(1, 1_000_001), 2);
    }

    /// The headroom a route has to clear is the *whole* cost of collecting the gain.
    /// Leaving the bid out of it is how a trade lands and still loses money.
    #[test]
    fn the_headroom_grows_by_exactly_what_the_bid_adds() {
        let bare = (BASE_FEE_LAMPORTS + priority_fee_lamports(0, 400_000)) * 5 / 4;
        let bid = (BASE_FEE_LAMPORTS + priority_fee_lamports(2_500, 400_000)) * 5 / 4;
        assert_eq!(bare, 6_250, "the fee alone, a quarter over");
        assert_eq!(bid, 7_500, "the fee plus a 1,000-lamport bid, a quarter over");
        assert!(bid > bare, "bidding for inclusion cannot make a route cheaper to justify");
    }

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
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        };
        let p = plan(3);
        let data = pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 3];

        let (hops, _) = t.hops_for(&p, &data, &arrays, &[], 0).expect("a well formed plan");
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
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        };
        let data = pools_for(&plan(3));
        let arrays = vec![[Pubkey::new_unique(); 3]; 3];

        let mut short = plan(3);
        short.mints.pop();
        assert!(t.hops_for(&short, &data, &arrays, &[], 0).is_err());

        let mut mismatched = plan(3);
        mismatched.leg_out.pop();
        assert!(t.hops_for(&mismatched, &data, &arrays, &[], 0).is_err());

        // Fewer accounts than pools must not silently build a shorter cycle.
        assert!(t.hops_for(&plan(3), &data[..2], &arrays, &[], 0).is_err());
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
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        };
        let data = pools_for(&plan(2));
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];
        let e = t.hops_for(&plan(2), &data, &arrays, &[], 0).unwrap_err().to_string();
        assert!(e.contains("zero"), "{e}");
    }

    #[test]
    fn unencodable_venues_are_named_before_anything_is_fetched() {
        let mut p = plan(3);
        p.pools[1].1 = Dex::MeteoraDammV2;
        assert!(!p.encodable());
        assert_eq!(p.blocking_venue(), Some(Dex::MeteoraDammV2));

        let clean = plan(3);
        assert!(clean.encodable());
        assert_eq!(clean.blocking_venue(), None);
    }

    /// This asserted the opposite until v4 got an encoder, which is the point of
    /// pinning it: the router's idea of what can be built and the encoder's have to
    /// move together, and nothing else makes them.
    #[test]
    fn every_venue_the_router_will_plan_is_one_the_encoder_accepts() {
        for dex in [Dex::OrcaWhirlpool, Dex::RaydiumClmm, Dex::RaydiumAmmV4] {
            assert!(has_encoder(dex), "{} is planned but cannot be built", dex.name());
        }
        for dex in [Dex::RaydiumCpmm, Dex::MeteoraDammV2, Dex::PumpSwap] {
            assert!(!has_encoder(dex), "{} has an encoder now; say so here", dex.name());
        }
        // Only the two tick venues go near the tick-array resolver.
        assert!(is_concentrated(Dex::OrcaWhirlpool) && is_concentrated(Dex::RaydiumClmm));
        assert!(!is_concentrated(Dex::RaydiumAmmV4), "v4 is constant-product, it has no ticks");
    }

    /// The two facts are separate and v4 is currently one and not the other. When
    /// re-pricing learns to fetch its vaults, this test is the thing that says so.
    #[test]
    fn a_venue_that_can_be_built_but_not_priced_is_refused_before_anything_is_fetched() {
        assert!(has_encoder(Dex::RaydiumAmmV4));
        assert!(!can_reprice(Dex::RaydiumAmmV4));
        // Nothing may be repriceable without also being encodable; that pairing would
        // build a route the encoder then refuses at the last moment.
        for dex in [
            Dex::OrcaWhirlpool,
            Dex::RaydiumClmm,
            Dex::RaydiumAmmV4,
            Dex::RaydiumCpmm,
            Dex::MeteoraDammV2,
            Dex::PumpSwap,
        ] {
            assert!(
                !can_reprice(dex) || has_encoder(dex),
                "{} would be planned and priced with no way to build it",
                dex.name()
            );
        }
    }

    /// The catch-all this replaced answered "Raydium CLMM" for every venue that was
    /// not Orca, which was right only while those two were the only callers.
    #[test]
    fn each_venue_is_pointed_at_its_own_program() {
        assert_eq!(program_for(Dex::OrcaWhirlpool), pk(cb_dex::orca_whirlpool::PROGRAM_ID));
        assert_eq!(program_for(Dex::RaydiumClmm), pk(cb_dex::raydium_clmm::PROGRAM_ID));
        assert_eq!(program_for(Dex::RaydiumAmmV4), pk(cb_dex::raydium_v4::PROGRAM_ID));
        assert_ne!(program_for(Dex::RaydiumAmmV4), program_for(Dex::RaydiumClmm));
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

    pub(super) fn whirlpool_with(mint_a: Pubkey32, mint_b: Pubkey32) -> Vec<u8> {
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
        let sqrt_price = cb_core::clmm::sqrt_price_at_tick(tick).expect("tick 32 has a sqrt price");
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
    use cb_core::types::Dex;
    use cb_executor::rpc::Rpc;

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
        let outcome =
            t.attempt(&plan, 0.18, 0.05, Intent::Trade).await.expect("the chain answered");
        println!("outcome: {outcome:?}");

        match outcome {
            Attempt::Submitted { .. } => {
                panic!("dry_run must never submit — this is the one unacceptable result")
            }
            Attempt::Probed(found) => {
                panic!("a trade intent must never come back as a measurement: {found}")
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
        for (b58, dex) in [(ORCA_SOL_USDC, Dex::OrcaWhirlpool), (RAY_SOL_USDC, Dex::RaydiumClmm)] {
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
            let chosen =
                ticks::resolve(&rpc, dex, &key, &program, tick, spacing, true).await.expect("rpc");
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
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        };
        let (hops, _) = t.hops_for(&plan, &pool_data, &arrays, &vec![None; plan.pools.len()], 0).expect("hops");

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
        assert!(
            wrap_shortfall(0, 3, bal_for_two).is_some(),
            "3 mints must not fit 2 mints' reserve"
        );
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
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        };

        let mut p = tests::plan(2);
        let data = tests::pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];

        let (honest, _) = t.hops_for(&p, &data, &arrays, &[], 0).expect("a well formed plan");

        // Now claim, in the plan only, that every leg returns a hundred times more.
        // The pools handed to `hops_for` are unchanged, so nothing about what the chain
        // would actually pay has moved.
        for q in &mut p.leg_out {
            *q *= 100;
        }
        let (inflated, _) = t.hops_for(&p, &data, &arrays, &[], 0).expect("a well formed plan");

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
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        };
        let p = tests::plan(3);
        let (hops, spent) = t
            .hops_for(&p, &tests::pools_for(&p), &vec![[Pubkey::new_unique(); 3]; 3], &[], 0)
            .expect("a well formed plan");

        assert_eq!(spent, p.amount_in, "these fixtures have room for the whole size");
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
        assert!(Trader::fresh_leg(Dex::RaydiumAmmV4, [7u8; 32], &[0u8; 300], &[1u8; 32], 2_500, None)
            .is_none());
    }

    /// A plan larger than the pool's remaining room is traded small, not refused.
    ///
    /// A concentrated leg quotes only inside its current tick, and the room left there
    /// moves continuously. The plan is sized against the interval as it stood when the
    /// websocket last spoke, so by the time the accounts are re-read the leg often
    /// cannot honour it. Refusing outright threw away 35 of 91 attempts over six hours
    /// of live running — more than a third of everything that reached the chain — on
    /// cycles that were profitable and merely too big.
    #[test]
    fn a_plan_bigger_than_the_pool_is_sized_down_rather_than_refused() {
        let t = Trader {
            exec: unreachable_executor(),
            opts: TradeOptions { slippage_tenth_bps: 3, ..Default::default() },
            owner: Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        };
        let mut p = tests::plan(2);
        let data = tests::pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];

        // Ask for far more than any tick interval can hold. Nothing about the pools
        // changed, so the honest answer is "trade what is there", not "trade nothing".
        p.amount_in = 100_000_000_000_000_000;
        let (hops, spent) =
            t.hops_for(&p, &data, &arrays, &[], 0).expect("an oversized plan is still tradeable");
        assert!(spent < p.amount_in, "it must not pretend the room is there");

        // The ceiling is a property of the pools, not of how much was asked for.
        let mut greedier = p.clone();
        greedier.amount_in = p.amount_in * 10;
        let (_, again) = t.hops_for(&greedier, &data, &arrays, &[], 0).expect("still tradeable");
        assert_eq!(spent, again, "the ceiling is the pools', not the request's");
        assert_eq!(u128::from(hops[0].amount_in), spent, "the first hop spends what was chosen");

        // The invariant that makes the whole thing safe is untouched by sizing down.
        for w in hops.windows(2) {
            assert_eq!(
                w[1].amount_in, w[0].min_amount_out,
                "a hop must still spend exactly what the one before it guaranteed"
            );
        }
    }

    /// The floor must leave the trade enough margin to pay its own fee.
    ///
    /// Each hop spends what the one before it *guaranteed*, so the route ends holding
    /// the last hop's quote while having promised that quote less one haircut. On a
    /// wSOL cycle the fee comes out of the same balance the profit is read from, so
    /// that single haircut is all there is to absorb it. At $9.20 that means the
    /// haircut cannot be under about 0.56 bps — and the shipped value was 0.3, which is
    /// why nothing ever cleared the check.
    #[test]
    fn the_floor_widens_until_the_trade_can_pay_its_own_fee() {
        let legs = {
            let t = Trader {
                exec: unreachable_executor(),
                opts: TradeOptions::default(),
                owner: Pubkey::new_unique(),
                ticks_seen: HashMap::new(),
                vaults_seen: HashMap::new(),
                token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            };
            let _ = &t;
            let p = tests::plan(2);
            Trader::fresh_legs(&p, &tests::pools_for(&p), &[]).expect("fixtures price")
        };
        let spend = tests::plan(2).amount_in;

        let widest = Trader::widest_buildable_haircut(&legs, spend, 1).expect("some width works");
        let (quoted, floor) = Trader::floor_chain(&legs, spend, widest).expect("it chains");

        assert!(floor > spend, "however wide, the route must still guarantee a profit");
        assert!(
            quoted > floor,
            "and it must arrive holding more than it promised — that difference is the \
             only thing the fee can come out of"
        );

        // Widening is what buys the margin. A narrower floor leaves strictly less of it,
        // which is the whole reason a fixed 0.3 bps could never pay a 5,000-lamport fee.
        let narrow = widest / 2;
        if narrow >= 1 {
            let (nq, nf) = Trader::floor_chain(&legs, spend, narrow).expect("it chains");
            assert!(
                nq.saturating_sub(nf) < quoted.saturating_sub(floor),
                "halving the floor must halve the margin the fee comes out of"
            );
        }

        // And the search never returns something that would lose money.
        for t in [1u32, 2, 5, widest] {
            if let Some((_, f)) = Trader::floor_chain(&legs, spend, t) {
                if t <= widest {
                    assert!(f > spend, "width {t} is inside the buildable range");
                }
            }
        }
    }

    /// Sizing down has a floor of its own: a cycle with no room anywhere is refused,
    /// and says so in terms an operator can act on.
    #[test]
    fn a_cycle_with_no_room_at_all_is_still_refused() {
        let t = Trader {
            exec: unreachable_executor(),
            opts: TradeOptions::default(),
            owner: Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
        };
        let p = tests::plan(2);
        // Pools that decode but trade the wrong mints cannot be re-priced at all.
        let wrong = vec![tests::whirlpool_with([9u8; 32], [8u8; 32]); 2];
        let e = t.hops_for(&p, &wrong, &[[Pubkey::new_unique(); 3]; 2], &[], 0).unwrap_err();
        assert!(
            e.to_string().contains("could not be re-priced"),
            "an unpriceable hop must say so: {e}"
        );
    }
}

#[cfg(test)]
mod vault_backed_tests {
    use super::{has_encoder, is_concentrated, Trader};
    use cb_core::types::Dex;

    /// A constant-product venue has no ticks and does have vaults, and both branches in
    /// `attempt` key off this. If it were ever classed as concentrated, the tick loop
    /// would try to read a tick out of an `AmmInfo` and the vault fetch would be
    /// skipped, which is two failures wearing one mistake.
    #[test]
    fn raydium_v4_is_encodable_and_not_concentrated() {
        assert!(has_encoder(Dex::RaydiumAmmV4), "the encoder has existed since e654b7d");
        assert!(!is_concentrated(Dex::RaydiumAmmV4), "its price is two vault balances");
        assert!(is_concentrated(Dex::OrcaWhirlpool));
        assert!(is_concentrated(Dex::RaydiumClmm));
    }

    /// The balance is a little-endian `u64` at offset 64 of an SPL token account, and
    /// reading it from the wrong place would price a pool from somebody's mint or owner
    /// key read as a number — a wrong price rather than an error, which is the worst
    /// kind.
    #[test]
    fn a_vault_balance_is_read_from_the_documented_offset() {
        let mut account = vec![0u8; cb_dex::raydium_v4::SPL_TOKEN_ACCOUNT_LEN];
        account[64..72].copy_from_slice(&123_456_789_u64.to_le_bytes());
        assert_eq!(Trader::spl_amount(&account), Some(123_456_789));
        assert_eq!(Trader::spl_amount(&[0u8; 16]), None, "too short is not zero");
    }

    /// The property the whole vault cache exists to protect.
    ///
    /// A pool account and the vaults it points at are one price only if they are read
    /// together. Fetching the pool, decoding it to learn the vault addresses and then
    /// fetching those is two prices from two moments — the exact mistake this file's
    /// history is made of, and it would be invisible, because the arithmetic still
    /// produces a number. So the vault keys must join `keys` before the single round
    /// trip, never after it.
    #[test]
    fn vault_keys_are_fetched_with_the_pool_and_not_after_it() {
        let src = include_str!("execute.rs");
        let push = src.find("keys.push(v[0]);").expect("vault keys are collected");
        let fetch = src.find("tokio::try_join!(rpc.accounts_full(&keys)").expect("one round trip");
        assert!(
            push < fetch,
            "vault addresses must be added to the fetch, not read in a second one — \
             a pool and its vaults from different slots is a price nobody was offered"
        );
    }
}

#[cfg(test)]
mod account_rent_tests {
    use super::{tests::plan, Trader, TradeOptions};
    use cb_core::types::Pubkey32;
    use std::collections::{HashMap, HashSet};

    fn trader_holding(mints: &[Pubkey32]) -> Trader {
        let mut t = Trader {
            exec: super::tests::unreachable_executor(),
            opts: TradeOptions::default(),
            owner: solana_sdk::pubkey::Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: HashSet::new(),
            accounts_held: HashSet::new(),
        };
        t.accounts_held = mints.iter().copied().collect();
        t
    }

    /// A run that has not measured which accounts exist must refuse nothing. The set
    /// is used as a negative filter, so an empty one meaning "the wallet holds nothing"
    /// would refuse every cycle in the book on the strength of a reading never taken.
    #[test]
    fn an_unmeasured_wallet_blocks_nothing() {
        let t = trader_holding(&[]);
        assert!(t.mint_without_an_account(&plan(2)).is_none());
    }

    /// The rent is charged to the same lamport balance the profit is read from, so a
    /// cycle through a mint we cannot hold fails its profit check by 2,039,280 lamports
    /// however good the trade was. Name it before spending two round trips finding out.
    #[test]
    fn a_mint_with_no_account_is_named_before_any_round_trip() {
        let p = plan(2);
        // A two-hop cycle reads SOL, the intermediate, SOL — so holding only the base
        // is the real case: the wallet can start the loop and cannot finish it.
        let t = trader_holding(&[p.mints[0]]);
        assert_eq!(
            t.mint_without_an_account(&p),
            Some(p.mints[1]),
            "the intermediate is the mint that needs the deposit"
        );

        let all: Vec<Pubkey32> = p.mints.to_vec();
        assert!(
            trader_holding(&all).mint_without_an_account(&p).is_none(),
            "a wallet holding every mint of the cycle must not be blocked"
        );
    }
}
