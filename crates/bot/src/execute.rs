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
use cb_executor::pda::{self, associated_token_address};
use cb_executor::route::{self, Hop, RouteOptions, WsolPolicy};
use cb_executor::venue::raydium::BitmapPolicy;
use cb_executor::venue::VenueExtra;
use cb_executor::{ticks, tx, Attempt, Executor, Plan};
use solana_sdk::instruction::Instruction;
use solana_sdk::message::AddressLookupTableAccount;
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
    /// Each pool's fee tier, in parts per million. For a Raydium CLMM pool this is the
    /// config fee alone: its dynamic fee is re-read with the pool at execution.
    ///
    /// Carried from detection because it is pool *configuration*, not pool *price*: it
    /// lives in a separate config account that a swap does not touch, so unlike a
    /// reserve or a sqrt-price it does not go stale between detection and execution.
    /// Re-pricing a Raydium CLMM leg needs it and re-fetching it would be a round trip
    /// spent on a number that cannot have changed.
    pub fee_ppm: Vec<u32>,
}

/// How often [`Trader::keep_warm`] runs: well inside the 45 s the warm pool keeps an idle
/// connection, and the 60 s both hosts were measured to.
pub const KEEP_WARM_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether [`cb_executor::venue::build_swap`] can encode a swap on this venue.
///
/// One definition rather than a `matches!` repeated at each of the places that need to
/// know. The two used to be written out separately and adding a venue meant finding
/// every one of them; missing a single site does not fail to compile, it produces a
/// plan the router accepts and the encoder then refuses at the last moment.
#[must_use]
pub const fn has_encoder(dex: Dex) -> bool {
    matches!(
        dex,
        Dex::OrcaWhirlpool
            | Dex::RaydiumClmm
            | Dex::RaydiumAmmV4
            | Dex::MeteoraDlmm
            | Dex::PumpSwap
            | Dex::MeteoraDammV2
    )
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

/// Whether a venue keeps its liquidity in bin arrays, and so needs those resolved
/// before a swap on it can be built or priced.
///
/// A third shape beside [`is_concentrated`] and the vault-backed venues, and it has to
/// be its own predicate rather than a branch off either: a binned pool has no tick to
/// predict and no vaults to cache, but it does have three ordered auxiliary accounts
/// that must arrive in the same read as the pool — which is exactly the concentrated
/// shape's requirement and exactly not the vault shape's.
#[must_use]
pub const fn is_binned(dex: Dex) -> bool {
    matches!(dex, Dex::MeteoraDlmm)
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
/// path fetches one account per pool. It now fetches three for this venue — the
/// vault addresses are cached per pool and join the same call (0f67a67) — but this
/// guard was not updated with it, and refused every v4 cycle up front for a fortnight:
/// 13 times in one 10-hour run, before the working path could be reached.
#[must_use]
pub const fn can_reprice(dex: Dex) -> bool {
    matches!(
        dex,
        Dex::OrcaWhirlpool
            | Dex::RaydiumClmm
            | Dex::MeteoraDlmm
            | Dex::RaydiumAmmV4
            | Dex::PumpSwap
            | Dex::MeteoraDammV2
    )
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

    /// A four-hop loop that enters a hub, runs a round trip behind it and comes back:
    /// SOL → USDC → X → USDC → SOL. See `cb_scanner::multi::lollipops`.
    #[must_use]
    pub fn is_lollipop(&self) -> bool {
        self.pools.len() == 4 && self.mints.len() == 5 && self.mints[1] == self.mints[3]
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
    /// Where a trade that clears every check goes. See [`Submit`].
    pub submit: Submit,
}

/// How a live trade reaches a leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Submit {
    /// An ordinary `sendTransaction`, with a priority bid. A floor missed by the time it
    /// lands reverts on chain and pays the whole fee.
    #[default]
    Rpc,
    /// Jito's block engine as a bundle of one, tipping instead of bidding.
    ///
    /// A floor missed by the time a leader reaches it drops the bundle and costs
    /// nothing, and the last hop's floor is raised by the base fee and the tip so that
    /// one which does land has paid for itself on chain. Together those make every
    /// submission either net positive or free. See [`cb_executor::rpc::Rpc::send_jito`].
    Jito {
        /// The most any one tip may be, in lamports. The tip itself is a share of the
        /// trade's own gross, never under [`cb_executor::jito::MIN_TIP_LAMPORTS`].
        tip_max_lamports: u64,
        /// Simulate before sending. See [`cb_executor::SendVia::Jito`].
        simulate_first: bool,
    },
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
            submit: Submit::Rpc,
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

/// Hops a cycle may have when one of them is on a binned venue.
///
/// # Why two and not three
///
/// A DLMM `swap2` names nineteen accounts where a Whirlpool swap names eleven, and a
/// transaction is capped at 1,232 bytes with every distinct account costing 32 of them. A
/// two-hop wSOL cycle through one measures **1,143 bytes** — 89 bytes of headroom, which
/// is not another account, let alone another swap. See
/// `a_two_hop_cycle_through_a_binned_pool_fits_in_one_packet`, which pins the number.
///
/// `route::build` would refuse such a cycle anyway, and refuse it correctly, naming the
/// packet limit. But it refuses *after* the pools, the bins, the balance and a blockhash
/// have been fetched, and a refusal that costs two round trips every time the router
/// proposes a shape that cannot exist is a refusal worth moving earlier.
pub const MAX_HOPS_WITH_A_BINNED_LEG: usize = 2;

/// Binned hops one cycle may have. Two of them do not fit beside each other either.
pub const MAX_BINNED_HOPS: usize = 1;

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
pub fn wrap_shortfall(
    amount_in: u64,
    distinct_mints: usize,
    balance: u64,
    account_rent: u64,
) -> Option<u64> {
    let reserved = distinct_mints as u64 * account_rent + FEE_ALLOWANCE_LAMPORTS;
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

/// What a binned pool looked like the last time this trader decoded it.
///
/// The same idea as [`TickHint`] and for the same reason: the bin arrays a swap needs
/// are derivable, but *which* ones depends on where the price currently sits, and
/// finding that out first would put a round trip between reading a price and asking the
/// chain to honour it. So the last known active bin predicts a window, the window is
/// asked about alongside the pools, and the arrays finally used are chosen from the
/// active bin read out of the account fetched this attempt.
///
/// A DLMM array spans seventy bins, so a window two arrays wide either side covers 350
/// bins — 3.5% of price on a one-basis-point pool. A price that moves further than that
/// between two attempts falls back to asking, which is a round trip rather than a wrong
/// account.
/// Nothing but the active bin, because the window asked about is symmetric: which way
/// the swap will walk is decided from the account fetched this attempt, not predicted.
#[derive(Debug, Clone, Copy)]
struct BinHint {
    active_id: i32,
}

/// Bin arrays either side of the predicted one that a prefetch asks about.
const BIN_PREFETCH_MARGIN: i64 = 2;

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

/// A Jito tip no larger than `room`, the gain the floor can guarantee, leaves after the
/// base fee — and never below the block engine's minimum, where the floor then refuses
/// the trade and says so.
#[must_use]
pub fn cap_tip(wanted: u64, room: u128) -> u64 {
    let afford = room.saturating_sub(BASE_FEE_LAMPORTS + 1);
    wanted.min(u64::try_from(afford).unwrap_or(u64::MAX)).max(cb_executor::jito::MIN_TIP_LAMPORTS)
}

/// The largest share of a trade's own gross profit that may go on the priority bid.
///
/// # Why a share and not a number
///
/// The configured bid is a constant, and a constant is wrong in both directions at once
/// on this book. Measured from `getRecentPrioritizationFees` over the pools traded here,
/// the p75 *winning* fee is 27,673 lamports — $0.0028 at SOL $102.70 — while a one to two
/// basis point window on a $12 trade is worth about $0.0015. Bidding to win a contested
/// race therefore costs nearly twice what the race pays, and bidding nothing gets a
/// transaction nobody has a reason to include: the bot's first ever submission came back
/// with a signature and was never picked up.
///
/// So the bid is priced off the prize. A quarter is the ceiling because the rest of the
/// gross has to cover the base fee, the slippage margin, and being wrong — and because a
/// trade that hands more than a quarter of its profit to a validator is close enough to
/// break-even that losing the race is the better outcome.
///
/// The configured `priority_micro_lamports` stays a **ceiling** rather than a target, so
/// the operator's number still bounds the worst case and the prize decides everything
/// below it.
const MAX_BID_SHARE_PERCENT: u128 = 25;

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
        Dex::MeteoraDlmm => pk(cb_dex::meteora_dlmm::PROGRAM_ID),
        Dex::PumpSwap => pk(cb_dex::pumpswap::PROGRAM_ID),
        Dex::MeteoraDammV2 => pk(cb_dex::meteora_damm_v2::PROGRAM_ID),
        _ => pk(cb_dex::raydium_clmm::PROGRAM_ID),
    }
}

/// The pool's own token A, read from the account rather than from the registry.
fn mint_a_of(dex: Dex, data: &[u8]) -> Result<Pubkey32> {
    match dex {
        Dex::OrcaWhirlpool => Ok(cb_dex::orca_whirlpool::decode(data)?.mint_a),
        Dex::RaydiumClmm => Ok(cb_dex::raydium_clmm::decode(data)?.mint_0),
        Dex::RaydiumAmmV4 => Ok(cb_dex::raydium_v4::decode_amm_info(data)?.base_mint),
        Dex::MeteoraDlmm => Ok(cb_dex::meteora_dlmm::decode(data)?.token_x_mint),
        Dex::PumpSwap => Ok(cb_dex::pumpswap::decode_pool(data)?.base_mint),
        Dex::MeteoraDammV2 => Ok(cb_dex::meteora_damm_v2::decode_layout(data)?.mint_a),
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
    /// Where a binned pool's price last sat, so its bin arrays can be predicted rather
    /// than discovered. See [`BinHint`].
    bins_seen: HashMap<Pubkey32, BinHint>,
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
    /// Where [`Submit::Jito`] sends. The block engine's default unless configured.
    jito_url: String,
    /// When the last Jito send went, so the next waits out the block engine's
    /// one-a-second limit here rather than being answered with a 429 there.
    last_jito_send: Option<std::time::Instant>,
    /// PumpSwap's fee recipients, read from its GlobalConfig at start. `None` until
    /// read, and a PumpSwap hop is refused while it is.
    pump_fees: Option<cb_executor::venue::pumpswap::PumpFeeRecipients>,
    /// What opening one token account deposits, as the chain charges it. Starts at
    /// [`route::TOKEN_ACCOUNT_RENT`] and is replaced at startup by the live figure:
    /// rent fell from 2,039,280 to 1,488,440 lamports for 165 bytes, and a constant
    /// that overstates it by 37% shrinks every trade the wrap reservation sizes.
    account_rent: u64,
    /// The lookup table trades are compiled against, as far as it is usable now.
    lookup: Option<AddressLookupTableAccount>,
    /// Addresses appended to the table but not usable until the slot after they were
    /// added; promoted into `lookup` once [`LOOKUP_WARMUP`] has passed.
    lookup_pending: Vec<(std::time::Instant, Vec<Pubkey>)>,
    /// What the last trade refused for size would have needed from the table.
    oversize: Option<Vec<Pubkey>>,
}

/// Move addresses whose warm-up has passed into the usable table. A free function so it
/// can run while an attempt holds the trader's RPC client borrowed.
fn promote_lookup(
    lookup: &mut Option<AddressLookupTableAccount>,
    pending: &mut Vec<(std::time::Instant, Vec<Pubkey>)>,
) {
    let Some(table) = lookup.as_mut() else { return };
    let (ready, waiting): (Vec<_>, Vec<_>) =
        std::mem::take(pending).into_iter().partition(|(at, _)| at.elapsed() >= LOOKUP_WARMUP);
    for (_, addrs) in ready {
        table.addresses.extend(addrs);
    }
    *pending = waiting;
}

/// How long after an extend its addresses are treated as usable. The program makes
/// them available from the slot after the extending transaction's; one confirmed
/// round trip plus this is comfortably past it.
pub const LOOKUP_WARMUP: std::time::Duration = std::time::Duration::from_millis(1_500);

impl Trader {
    #[must_use]
    pub fn new(exec: Executor, opts: TradeOptions) -> Self {
        let owner = exec.pubkey();
        Self {
            exec,
            opts,
            owner,
            ticks_seen: HashMap::new(),
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        }
    }

    /// Use the chain's current rent for a token account. Zero keeps the constant.
    pub fn set_account_rent(&mut self, lamports: u64) {
        if lamports > 0 {
            self.account_rent = lamports;
        }
    }

    /// What one token account deposits, as this trader currently prices it.
    #[must_use]
    pub fn account_rent(&self) -> u64 {
        self.account_rent
    }

    /// Ask the chain what a token account deposits now and use that.
    ///
    /// Priced at 200 bytes rather than the classic 165: a Token-2022 account carries
    /// its extensions in the same account, and a reservation a little high only trims
    /// a trade, where one too low lets a wrap leave nothing for the rent it needs.
    ///
    /// # Errors
    /// If the chain cannot be reached; the previous figure then stands.
    pub async fn learn_account_rent(&mut self) -> Result<u64> {
        const TOKEN_ACCOUNT_BYTES_WITH_EXTENSIONS: usize = 200;
        let rent = self.exec.rpc.rent_exempt_minimum(TOKEN_ACCOUNT_BYTES_WITH_EXTENSIONS).await?;
        self.set_account_rent(rent);
        Ok(rent)
    }

    /// Send [`Submit::Jito`] trades to this block engine URL instead of the default.
    ///
    /// An empty string keeps the default, so a config that leaves the key blank cannot
    /// point trades at nothing.
    pub fn set_jito_url(&mut self, url: &str) {
        if !url.trim().is_empty() {
            self.jito_url = url.trim().to_string();
        }
    }

    /// The Jito tip a trade grossing `gross` lamports will pay if it lands; zero off Jito.
    ///
    /// The same quarter-of-the-prize rule as [`Trader::bid_for`], for the same reasons,
    /// with the block engine's minimum as a floor and the configured ceiling above.
    ///
    /// `gross` must be the *fresh* one — see [`Trader::fresh_gross`]. The plan's own
    /// figure is the detection-time quote, and three quarters of detections are priced
    /// behind the feed's head: a lagged 30 bp "edge" would tip twenty thousand lamports
    /// on a trade whose real edge is under one basis point, the tip would join the gain
    /// the last floor must guarantee, and a trade that could pay a small tip would be
    /// refused for failing to pay a large one.
    #[must_use]
    fn tip_for(&self, gross: u128) -> u64 {
        let Submit::Jito { tip_max_lamports, .. } = self.opts.submit else {
            return 0;
        };
        let min = cb_executor::jito::MIN_TIP_LAMPORTS;
        let share = gross.saturating_mul(MAX_BID_SHARE_PERCENT) / 100;
        u64::try_from(share).unwrap_or(u64::MAX).clamp(min, tip_max_lamports.max(min))
    }

    /// What this cycle grosses against the state just fetched, at the size the pools
    /// can carry now and with no haircut. Zero when it grosses nothing or cannot be
    /// priced; the attempt then refuses further on, with the reason.
    fn fresh_gross(
        plan: &CyclePlan,
        pool_data: &[Vec<u8>],
        vaults: &[Option<(u64, u64)>],
        bins: &[Vec<Vec<u8>>],
    ) -> u128 {
        let Ok(legs) = Self::fresh_legs(plan, pool_data, vaults, bins) else { return 0 };
        let spend = cb_core::path::largest_feasible(&legs, plan.amount_in);
        Self::floor_chain(&legs, spend, 0).map_or(0, |(quoted, _)| quoted.saturating_sub(spend))
    }

    /// What the last floor can guarantee past the spend at the configured haircut, on
    /// the state just fetched. The most the fee and tip together can be paid out of.
    fn fresh_floor_gain(
        plan: &CyclePlan,
        pool_data: &[Vec<u8>],
        vaults: &[Option<(u64, u64)>],
        bins: &[Vec<Vec<u8>>],
        tenth_bps: u32,
    ) -> u128 {
        let Ok(legs) = Self::fresh_legs(plan, pool_data, vaults, bins) else { return 0 };
        let spend = cb_core::path::largest_feasible(&legs, plan.amount_in);
        Self::floor_chain(&legs, spend, tenth_bps).map_or(0, |(_, floor)| floor.saturating_sub(spend))
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

    /// The priority bid this specific trade can afford, in micro-lamports per compute
    /// unit.
    ///
    /// `wrapping` says whether the cycle's profit is denominated in lamports, which on a
    /// wSOL cycle it is. When it is not, the gross is in some other token and converting
    /// it would need a price this function has no business holding, so the configured
    /// constant stands — a small, known overpayment on the cycles that are not the main
    /// case anyway.
    ///
    /// See [`MAX_BID_SHARE_PERCENT`] for why the answer is a share of the prize.
    #[must_use]
    fn bid_for(&self, plan: &CyclePlan, wrapping: bool) -> u64 {
        let configured = self.opts.priority_micro_lamports;
        if !wrapping || self.opts.compute_units == 0 {
            return configured;
        }
        let gross = plan
            .leg_out
            .last()
            .copied()
            .unwrap_or(0)
            .saturating_sub(plan.amount_in)
            .saturating_mul(MAX_BID_SHARE_PERCENT)
            / 100;
        // Invert `priority_fee_lamports`, which charges `ceil(micro · units / 1e6)`.
        // Flooring here is deliberate: a bid rounded up would spend a lamport more than
        // the share allows, which is the one direction this calculation must not err in.
        let micro = gross.saturating_mul(1_000_000) / u128::from(self.opts.compute_units);
        u64::try_from(micro.min(u128::from(configured))).unwrap_or(configured)
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
        bins: &[&[u8]],
    ) -> Option<Leg> {
        let state = match dex {
            Dex::OrcaWhirlpool => cb_dex::orca_whirlpool::to_pool_state(address, data, 0).ok()?,
            Dex::RaydiumClmm => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                cb_dex::raydium_clmm::to_pool_state(address, data, fee_ppm, 0, now).ok()?
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
            // The price is in the pool account and the depth is in separate bin
            // accounts, so this venue needs both in the same read for the same reason v4
            // needs its vaults. The fee comes from the pool too, not from the plan: a
            // DLMM's fee moves with its own volatility, so a cached one is the one
            // number here that certainly is stale.
            Dex::MeteoraDlmm => {
                cb_dex::meteora_dlmm::to_pool_state(address, data, bins, 0).ok()?
            }
            // Vault-backed like v4, and the virtual quote reserve is in the pool account
            // read beside them. The fee is the plan's: the tier the market cap had at
            // detection, which only changes when the cap crosses a tier boundary.
            // Self-contained: price, range and the whole fee are in the pool account.
            Dex::MeteoraDammV2 => cb_dex::meteora_damm_v2::to_pool_state(address, data, 0).ok()?,
            Dex::PumpSwap => {
                let (base, quote) = vaults?;
                let pool = cb_dex::pumpswap::decode_pool(data).ok()?;
                cb_dex::pumpswap::to_pool_state(address, &pool, base, quote, u64::from(fee_ppm) / 100, 0)
                    .ok()?
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
    /// `required_gain` is how far past `spend` the last floor must reach — zero for the
    /// bare invariant, the cost of landing when that is paid from the same balance.
    fn widest_buildable_haircut(
        legs: &[Leg],
        spend: u128,
        floor_tenth_bps: u32,
        required_gain: u128,
    ) -> Option<u32> {
        // Wider is better on every axis, and the floor falls monotonically as the
        // haircut grows, so the first hit walking down is the answer. The ceiling never
        // sits below a deliberately configured floor.
        let ceiling = MAX_HAIRCUT_TENTH_BPS.max(floor_tenth_bps);
        let needed = spend.saturating_add(required_gain);
        (floor_tenth_bps.max(1)..=ceiling)
            .rev()
            .find(|t| Self::floor_chain(legs, spend, *t).is_some_and(|(_, f)| f > needed))
    }

    /// Every leg of this cycle as the chain has it right now.
    fn fresh_legs(
        plan: &CyclePlan,
        pool_data: &[Vec<u8>],
        vaults: &[Option<(u64, u64)>],
        bins: &[Vec<Vec<u8>>],
    ) -> Result<Vec<Leg>> {
        let empty: Vec<Vec<u8>> = Vec::new();
        (0..plan.pools.len())
            .map(|i| {
                let hop_bins = bins.get(i).unwrap_or(&empty);
                let refs: Vec<&[u8]> = hop_bins.iter().map(Vec::as_slice).collect();
                Self::fresh_leg(
                    plan.pools[i].1,
                    plan.pools[i].0,
                    &pool_data[i],
                    &plan.mints[i],
                    plan.fee_ppm[i],
                    vaults.get(i).copied().flatten(),
                    &refs,
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
    #[allow(clippy::too_many_arguments)]
    pub fn hops_for(
        &self,
        plan: &CyclePlan,
        pool_data: &[Vec<u8>],
        arrays: &[[Pubkey; 3]],
        vaults: &[Option<(u64, u64)>],
        bins: &[Vec<Vec<u8>>],
        fee_headroom: u128,
        required_gain: u128,
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
        let legs = Self::fresh_legs(plan, pool_data, vaults, bins)?;
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
            required_gain,
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

            let mut floor = haircut(fresh, tenth_bps);
            if floor == 0 {
                bail!("hop {i} floors at zero after {tenth_bps} tenths of a bp of slippage");
            }
            // When the cost of landing is guaranteed on chain, the last floor need be no
            // higher than exactly that: input, plus the cost, plus one. Anything above it
            // is tolerance given away for nothing — the trade is already net positive at
            // that floor — and tolerance is what decides whether a trade still lands a
            // slot after it was priced. Lowered only, never raised: if the haircut floor
            // sits below this line, `route::build` refuses the trade and says why.
            if required_gain > 0 && i + 1 == n {
                let break_even = spend_total.saturating_add(required_gain).saturating_add(1);
                floor = floor.min(break_even);
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
        let attempt_started = std::time::Instant::now();
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
            // The block engine allows one send a second from this address. Refused
            // before any round trip, since a trade built now could not be sent until its
            // price was a second old.
            if matches!(self.opts.submit, Submit::Jito { .. }) && !self.opts.dry_run {
                if let Some(at) = self.last_jito_send {
                    let gap = at.elapsed().as_millis();
                    if gap < u128::from(cb_executor::jito::MIN_SEND_GAP_MS) {
                        return Ok(Attempt::Refused(format!(
                            "the last Jito send was {gap} ms ago and the block engine \
                             allows one a second"
                        )));
                    }
                }
            }
        }

        // A cycle whose intermediate mint the wallet has no account for cannot clear
        // its own profit check, because creating that account costs rent out of the
        // very balance the profit is measured in. Said here, once, for nothing, rather
        // than discovered two round trips later as an unexplained balance shortfall.
        if let Some(mint) = self.mint_without_an_account(plan) {
            return Ok(Attempt::Refused(format!(
                "the wallet holds no token account for {}, and opening one costs {} lamports \
                 of rent out of the same balance this trade's profit is measured in — the \
                 bot opens one itself once attempts keep asking for this mint",
                to_pubkey(&mint),
                self.account_rent
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
        // A lollipop is let through to be re-priced even though four hops measured 64 bytes
        // over the packet: it shares its hub's token account between two legs, which is
        // one account nearer, and whether any of them ever clears on fresh state is the
        // measurement that decides whether an address lookup table is worth its rent. One
        // that clears and still does not fit is refused by `tx::assemble`, naming the size.
        if plan.pools.len() > MAX_EXECUTABLE_HOPS && !plan.is_lollipop() {
            return Ok(Attempt::Refused(format!(
                "{} hops will not fit in one transaction without an address lookup table \
                 (the ceiling is {MAX_EXECUTABLE_HOPS})",
                plan.pools.len()
            )));
        }

        // A binned hop is nineteen accounts, and two hops through one already fill 1,143
        // of the 1,232 bytes a packet allows. Refused here rather than after four
        // accounts fetches and a blockhash — see [`MAX_HOPS_WITH_A_BINNED_LEG`].
        let binned = plan.pools.iter().filter(|(_, d)| is_binned(*d)).count();
        if binned > 0 && plan.pools.len() > self.max_hops_with_a_binned_leg() {
            return Ok(Attempt::Refused(format!(
                "{} hops will not fit in one transaction beside a {} leg, which needs \
                 nineteen accounts of the {} bytes a packet holds — this shape needs an \
                 address lookup table, not a smaller encoding",
                plan.pools.len(),
                Dex::MeteoraDlmm.name(),
                cb_executor::tx::PACKET_LIMIT
            )));
        }
        if binned > MAX_BINNED_HOPS {
            return Ok(Attempt::Refused(format!(
                "{binned} binned legs will not fit in one transaction; one is already \
                 nineteen accounts"
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
        let jito = matches!(self.opts.submit, Submit::Jito { .. });
        // A bundle's fee and tip are lamports, and the floors can only promise to cover
        // them when the profit is counted in lamports too.
        if jito && !wrapping {
            return Ok(Attempt::Refused(format!(
                "this cycle starts in {base_mint}, and a Jito trade can only guarantee on \
                 chain that it covers its fee when it starts and ends in SOL"
            )));
        }
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

        // Bin arrays for the binned hops, in this same round trip, predicted from where
        // the price last was.
        //
        // The prediction is the same trick the tick arrays use and rests on the same
        // property: the *address* of a bin array only needs to be right to the precision
        // of seventy bins, so a window two arrays either side of the last known active
        // bin covers 3.5% of price movement on the tightest pool in the registry. What
        // the instruction finally names is chosen from the active bin read out of the
        // account fetched this attempt, never from the hint.
        let mut bin_at: Vec<Option<(usize, Vec<i64>)>> = vec![None; n];
        for (i, (pool_raw, dex)) in plan.pools.iter().enumerate() {
            if !is_binned(*dex) {
                continue;
            }
            let Some(hint) = self.bins_seen.get(pool_raw) else { continue };
            let centre = cb_dex::meteora_dlmm::bin_array_index(hint.active_id);
            let indices: Vec<i64> =
                (-BIN_PREFETCH_MARGIN..=BIN_PREFETCH_MARGIN).map(|d| centre + d).collect();
            let program = program_for(*dex);
            let pool = to_pubkey(pool_raw);
            let at = keys.len();
            keys.extend(indices.iter().map(|ix| pda::meteora_bin_array(&pool, *ix, &program)));
            bin_at[i] = Some((at, indices));
        }

        let (fetched, (blockhash, _)) =
            tokio::try_join!(rpc.accounts_latest(&keys), rpc.latest_blockhash())?;
        {
            let bytes: usize = fetched.iter().flatten().map(|a| a.data.len()).sum();
            tracing::info!(
                "re-price read: {} accounts, {} KiB, {} ms",
                keys.len(),
                bytes / 1024,
                attempt_started.elapsed().as_millis()
            );
        }
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
                    let learned = match dex {
                        Dex::RaydiumAmmV4 => cb_dex::raydium_v4::decode_amm_info(&pool_data[i])
                            .ok()
                            .map(|info| [to_pubkey(&info.base_vault), to_pubkey(&info.quote_vault)]),
                        Dex::PumpSwap => cb_dex::pumpswap::decode_pool(&pool_data[i])
                            .ok()
                            .map(|p| [to_pubkey(&p.base_vault), to_pubkey(&p.quote_vault)]),
                        _ => None,
                    };
                    if let Some(v) = learned {
                        self.vaults_seen.insert(*pool_raw, v);
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

        // Turn the fetched bin arrays into the three each binned hop will walk, and the
        // bytes its quote will be priced from.
        //
        // Both come out of the same place on purpose. The encoder has to name the arrays
        // the program will traverse, and the quote has to be computed from the contents
        // of those same accounts; splitting the two would let an instruction walk bins
        // that a different read priced.
        let mut bins: Vec<Vec<Vec<u8>>> = vec![Vec::new(); n];
        let mut bin_arrays: Vec<Option<[Pubkey; 3]>> = vec![None; n];
        let mut bins_pending: Vec<(usize, Pubkey, i32, bool)> = Vec::new();
        let mut bins_learned: Vec<(Pubkey32, BinHint)> = Vec::with_capacity(n);
        for (i, (pool_raw, dex)) in plan.pools.iter().enumerate() {
            if !is_binned(*dex) {
                continue;
            }
            let pair = cb_dex::meteora_dlmm::decode(&pool_data[i])?;
            let falling = plan.mints[i] == pair.token_x_mint;
            bins_learned.push((*pool_raw, BinHint { active_id: pair.active_id }));
            let pool = to_pubkey(pool_raw);
            let program = program_for(*dex);
            let wanted = pda::meteora_bin_array_indices(pair.active_id, falling);

            // Every index the fresh active bin asks for has to be one the prefetch
            // already asked about. Missing even one means we do not know whether that
            // account exists, and an unknown bin array is not one a swap may name.
            let covered = bin_at[i]
                .as_ref()
                .is_some_and(|(_, asked)| wanted.iter().all(|ix| asked.contains(ix)));
            if !covered {
                bins_pending.push((i, pool, pair.active_id, falling));
                continue;
            }
            let (at, asked) = bin_at[i].as_ref().expect("covered implies present");
            let mut addresses = [pool; 3];
            let mut data: Vec<Vec<u8>> = Vec::with_capacity(3);
            for (slot, ix) in addresses.iter_mut().zip(wanted) {
                let j = asked.iter().position(|a| *a == ix).expect("checked above");
                *slot = pda::meteora_bin_array(&pool, ix, &program);
                if let Some(acc) =
                    fetched.get(at + j).and_then(Option::as_ref).filter(|a| a.owner == program)
                {
                    data.push(acc.data.clone());
                }
            }
            if data.is_empty() {
                return Ok(Attempt::Refused(format!(
                    "pool {pool} has no initialised bin arrays to swap through"
                )));
            }
            bins[i] = data;
            bin_arrays[i] = Some(addresses);
        }

        // Only the binned hops the prediction could not answer: a pool this trader has
        // never decoded, or one whose price walked clean out of the prefetched window.
        if !bins_pending.is_empty() {
            let asked = bins_pending.iter().map(|&(i, pool, active_id, falling)| {
                let program = program_for(plan.pools[i].1);
                let wanted = pda::meteora_bin_array_indices(active_id, falling);
                let addresses: Vec<Pubkey> =
                    wanted.iter().map(|ix| pda::meteora_bin_array(&pool, *ix, &program)).collect();
                async move {
                    let got = rpc.accounts_latest(&addresses).await?;
                    let data: Vec<Vec<u8>> = got
                        .iter()
                        .filter_map(|a| a.as_ref().filter(|a| a.owner == program))
                        .map(|a| a.data.clone())
                        .collect();
                    let mut fixed = [pool; 3];
                    for (slot, key) in fixed.iter_mut().zip(&addresses) {
                        *slot = *key;
                    }
                    anyhow::Ok((i, pool, fixed, data))
                }
            });
            for (i, pool, addresses, data) in futures::future::try_join_all(asked).await? {
                if data.is_empty() {
                    return Ok(Attempt::Refused(format!(
                        "pool {pool} has no initialised bin arrays to swap through"
                    )));
                }
                bins[i] = data;
                bin_arrays[i] = Some(addresses);
            }
        }
        for (pool_raw, hint) in bins_learned {
            self.bins_seen.insert(pool_raw, hint);
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
            // A binned hop's three auxiliary accounts were resolved above, from the
            // same read that priced it. It has no tick and no spacing, so it must not
            // fall into the sweep.
            if is_binned(*dex) {
                arrays[i] = bin_arrays[i];
                continue;
            }
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
        //
        // The bid itself is priced off this trade's own gross rather than off a constant
        // — see [`MAX_BID_SHARE_PERCENT`]. The headroom then follows the bid, which is the
        // point: a smaller prize bids less *and* is asked to clear less.
        //
        // Through Jito all of that changes shape. There is no priority bid — the tip
        // buys the place in the block — and the fee is not a margin the simulation
        // checks but a gain the last floor guarantees, so the chain itself refuses a
        // trade that would land short of it. The headroom is then zero, because nothing
        // is left for it to protect.
        //
        // And no more than the floor can pay. A quarter of the gross is the bid a trade
        // wants to make; what it can make is whatever the guaranteed gain leaves after
        // the base fee. Sized the other way round, 9 of 39 near-misses on 2026-09-24
        // guaranteed 6,069-7,331 lamports and were refused against costs of
        // 6,530-7,338 that only the tip had pushed past them. A smaller tip lands less
        // often; a refused trade never does.
        let tip = if jito {
            cap_tip(
                self.tip_for(Self::fresh_gross(plan, &pool_data, &vaults, &bins)),
                Self::fresh_floor_gain(
                    plan,
                    &pool_data,
                    &vaults,
                    &bins,
                    self.opts.slippage_tenth_bps,
                ),
            )
        } else {
            0
        };
        let bid = if jito { 0 } else { self.bid_for(plan, wrapping) };
        let priority = priority_fee_lamports(bid, self.opts.compute_units);
        let fee_headroom =
            if wrapping && !jito { (BASE_FEE_LAMPORTS + priority) * 5 / 4 } else { 0 };
        let required_gain = if jito { BASE_FEE_LAMPORTS + u128::from(tip) } else { 0 };
        let (hops, spent) = match self.hops_for(
            plan,
            &pool_data,
            &arrays,
            &vaults,
            &bins,
            fee_headroom,
            required_gain,
        ) {
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
            if let Some(reserved) = wrap_shortfall(amount_in_u64, distinct_mints, bal, self.account_rent) {
                return Ok(Attempt::Refused(format!(
                    "wrapping {amount_in_u64} lamports would leave less than the {reserved} \
                     lamports this transaction needs for account rent and fees, against a \
                     balance of {bal} — sizing must leave that headroom, not spend into it"
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
            // The same bid the headroom above was computed from. Two different numbers
            // here would mean the route cleared a fee it did not then pay, or paid one it
            // had not cleared.
            priority_micro_lamports: bid,
            wsol: self.opts.wsol,
            create_token_accounts: self.opts.create_token_accounts,
            others_exist: !self.accounts_held.is_empty(),
            venue: VenueExtra { token_program, bitmap_policy: BitmapPolicy::Auto, pump: self.pump_fees },
            min_gain: u64::try_from(required_gain).unwrap_or(u64::MAX),
            // Any of the eight accounts will do; spreading by the blockhash keeps
            // successive tips off one write lock without a random-number generator.
            tip: jito.then(|| {
                let seed = u64::from_le_bytes(
                    blockhash.to_bytes()[..8].try_into().expect("a hash is 32 bytes"),
                );
                (cb_executor::jito::tip_account(seed), tip)
            }),
        };

        let built = match route::build(&self.owner, &hops, pre_balance, &opts) {
            Ok(r) => r,
            // A route that refuses to build is the guard working, not a failure.
            Err(e) => return Ok(Attempt::Refused(e.to_string())),
        };

        promote_lookup(&mut self.lookup, &mut self.lookup_pending);
        let tables: Vec<AddressLookupTableAccount> = self.lookup.iter().cloned().collect();
        let assembled = match tx::assemble_with(&self.exec.wallet, &built.instructions, blockhash, &tables) {
            Ok(a) => a,
            Err(e) => {
                // Cleared its floor and did not fit: remember what the table would need,
                // so the caller can add it and the next attempt at this cycle fits.
                if e.to_string().contains("byte limit") {
                    let held: Vec<Pubkey> =
                        self.lookup.as_ref().map(|t| t.addresses.clone()).unwrap_or_default();
                    let mut held = held;
                    for (_, p) in &self.lookup_pending {
                        held.extend(p.iter().copied());
                    }
                    self.oversize = Some(cb_executor::alt::candidates(&built.instructions, &held));
                }
                return Ok(Attempt::Refused(e.to_string()));
            }
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
                let via = match self.opts.submit {
                    Submit::Rpc => cb_executor::SendVia::Rpc,
                    Submit::Jito { simulate_first, .. } => cb_executor::SendVia::Jito {
                        url: self.jito_url.clone(),
                        simulate_first,
                    },
                };
                let r = plan_to_run.execute(&mut self.exec.gate, rpc, self.opts.dry_run, &via).await;
                // Counted from any send that reached the wire or failed trying: a send
                // that errored may still have been received, and was a request either way.
                if jito && matches!(r, Ok(Attempt::Submitted { .. }) | Err(_)) {
                    self.last_jito_send = Some(std::time::Instant::now());
                }
                r
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
                missing.len() as u64 * self.account_rent
            );
        }

        let cost = missing.len() as u64 * self.account_rent;
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

        let what = format!("opening {} token account(s)", missing.len());
        let sent = self.send_setup(&ixs, &what).await?;
        if let Some(signature) = &sent {
            tracing::warn!("opened {} token account(s), confirmed on chain: {signature}", missing.len());
        }
        Ok(sent)
    }

    /// Use the lookup table at `key`, reading its addresses from the chain.
    ///
    /// # Errors
    /// If the chain cannot be reached or the account is not a lookup table.
    pub async fn load_lookup(&mut self, key: Pubkey) -> Result<usize> {
        let got = self.exec.rpc.accounts_full(std::slice::from_ref(&key)).await?;
        let acc = got
            .into_iter()
            .next()
            .flatten()
            .with_context(|| format!("lookup table {key} does not exist"))?;
        anyhow::ensure!(
            acc.owner == pk(cb_executor::alt::PROGRAM_ID),
            "{key} is not a lookup table"
        );
        let table = cb_executor::alt::parse(key, &acc.data)?;
        let n = table.addresses.len();
        self.lookup = Some(table);
        Ok(n)
    }

    /// The addresses the last trade refused for size would have needed, once.
    pub fn take_oversize(&mut self) -> Option<Vec<Pubkey>> {
        self.oversize.take()
    }

    /// How many addresses the table holds, usable or warming up.
    #[must_use]
    pub fn lookup_len(&self) -> usize {
        self.lookup.as_ref().map_or(0, |t| t.addresses.len())
            + self.lookup_pending.iter().map(|(_, a)| a.len()).sum::<usize>()
    }

    /// Add `addresses` to this trader's lookup table, creating the table first if there
    /// is none. Returns the table's address when it was just created, so the caller can
    /// remember it across restarts.
    ///
    /// The table grows only from trades that cleared their floor and did not fit, so
    /// the rent it holds — refundable when the table is closed — is spent on cycles
    /// that have shown they can clear, and never past `cap` addresses.
    ///
    /// # Errors
    /// If the chain cannot be reached or a setup transaction does not land.
    pub async fn grow_lookup(&mut self, addresses: Vec<Pubkey>, cap: usize) -> Result<Option<Pubkey>> {
        let room = cap.min(cb_executor::alt::MAX_ADDRESSES).saturating_sub(self.lookup_len());
        let adding: Vec<Pubkey> = addresses.into_iter().take(room).collect();
        if adding.is_empty() {
            return Ok(None);
        }
        let mut created = None;
        let table = match &self.lookup {
            Some(t) => t.key,
            None => {
                let slot = self.exec.rpc.slot().await?;
                let (ix, key) = cb_executor::alt::create(&self.owner, &self.owner, slot);
                if self.send_setup(&[ix], "creating a lookup table").await?.is_none() {
                    return Ok(None);
                }
                tracing::warn!("created lookup table {key}");
                self.lookup = Some(AddressLookupTableAccount { key, addresses: Vec::new() });
                created = Some(key);
                key
            }
        };
        for chunk in adding.chunks(cb_executor::alt::EXTEND_CHUNK) {
            let ix = cb_executor::alt::extend(&table, &self.owner, &self.owner, chunk)?;
            let what = format!("adding {} addresses to lookup table {table}", chunk.len());
            if self.send_setup(&[ix], &what).await?.is_none() {
                return Ok(created);
            }
            self.lookup_pending.push((std::time::Instant::now(), chunk.to_vec()));
        }
        tracing::warn!(
            "lookup table {table} now holds {} addresses; cycles that did not fit a packet can",
            self.lookup_len()
        );
        Ok(created)
    }

    /// Send a one-off setup transaction — opening accounts, building a lookup table —
    /// and wait until the chain has it. `None` in a dry run, which simulates and stops.
    ///
    /// Sent until it actually lands, unlike a trade.
    ///
    /// `Rpc::send` asks the node for three rebroadcasts of the same signed bytes
    /// and then stops, because a stale arbitrage is worthless and chasing one is
    /// worse than dropping it. This is the opposite case. There is no race, nothing
    /// here goes stale but the blockhash, and the transaction only has to arrive —
    /// so three rebroadcasts of one blockhash is not enough on its own.
    ///
    /// Reusing the fire-and-forget path cost a whole run. The first send returned a
    /// signature, the account was reported open, and the transaction was never
    /// included — `getSignatureStatuses` with full history search had never heard of
    /// it, and the wallet's newest transaction was still nine days old. A signature
    /// is a receipt for having asked.
    ///
    /// So: re-sign against a fresh blockhash each round, and believe nothing until
    /// the chain confirms it.
    ///
    /// # Errors
    /// If it does not simulate cleanly, reverts, or is never included.
    async fn send_setup(&self, ixs: &[Instruction], what: &str) -> Result<Option<String>> {
        const ROUNDS: u32 = 4;
        let mut last_signature = None;
        for round in 1..=ROUNDS {
            let (blockhash, _) = self.exec.rpc.latest_blockhash().await?;
            let assembled = tx::assemble(&self.exec.wallet, ixs, blockhash)?;

            let sim = self.exec.rpc.simulate(&assembled.tx_base64, &[]).await?;
            if !sim.succeeded() {
                let ctx = sim.error_context().unwrap_or_default();
                anyhow::bail!(
                    "{what} did not simulate cleanly, so nothing was sent: {} {ctx}",
                    sim.err.unwrap_or_else(|| "unknown".into())
                );
            }
            if self.opts.dry_run {
                tracing::info!("dry run — {what} simulated cleanly and was not sent");
                return Ok(None);
            }

            let signature = self.exec.rpc.send(&assembled.tx_base64, true).await?;
            last_signature = Some(signature.clone());
            // Fifteen tries at two seconds is thirty seconds of patience, which is
            // generous for inclusion and costs nothing: setup runs rarely.
            match self.confirm(&signature, 15).await {
                Some(true) => return Ok(Some(signature)),
                Some(false) => anyhow::bail!("{what} reverted on chain: {signature}"),
                None if round < ROUNDS => tracing::warn!(
                    "{signature} ({what}) has not been included; re-sending against a fresh blockhash ({round} of {ROUNDS})"
                ),
                None => {}
            }
        }
        anyhow::bail!(
            "sent {what} {ROUNDS} times and none was included; the last was {}",
            last_signature.unwrap_or_else(|| "—".into())
        )
    }

    /// The accounts [`crate::demand`] may close: held, and not in `protected` (the base
    /// mints and whatever the operator pinned). wSOL is never one of them.
    #[must_use]
    pub fn managed_accounts(
        &self,
        protected: &std::collections::HashSet<Pubkey32>,
    ) -> std::collections::HashSet<Pubkey32> {
        let wsol = *pk(programs::WSOL_MINT).as_array();
        self.accounts_held
            .iter()
            .filter(|m| **m != wsol && !protected.contains(*m))
            .copied()
            .collect()
    }

    /// Whether a cycle of this shape could ever be sent by this trader, whatever its
    /// price: the refusals in [`Trader::attempt`] that do not depend on the market.
    ///
    /// What the token rotation counts as demand. Counting the rest opened an account
    /// for STONK on 2026-09-24 off 486 asks from USDC-start loops that Jito refuses
    /// before pricing them, and closed JLP to make room.
    #[must_use]
    pub fn could_execute(&self, plan: &CyclePlan) -> bool {
        let binned = plan.pools.iter().filter(|(_, d)| is_binned(*d)).count();
        let starts_in_sol = plan.mints.first().is_some_and(|m| to_pubkey(m) == pk(programs::WSOL_MINT));
        plan.encodable()
            && plan.pools.iter().all(|(_, d)| can_reprice(*d))
            // A lollipop is four hops and fits once its accounts are in the lookup table.
            && (plan.pools.len() <= MAX_EXECUTABLE_HOPS || plan.is_lollipop())
            && (binned == 0 || plan.pools.len() <= self.max_hops_with_a_binned_leg())
            && binned <= MAX_BINNED_HOPS
            && (!self.sends_via_jito() || (starts_in_sol && self.opts.wsol == WsolPolicy::WrapAndClose))
    }

    /// How many hops a cycle with a binned leg may have. Two without a lookup table,
    /// where a DLMM's nineteen accounts leave no room for a third hop; three with one,
    /// where the accounts cost a byte each and the cycle is refused at assembly only if
    /// it still does not fit — after which the table grows to take it.
    #[must_use]
    pub fn max_hops_with_a_binned_leg(&self) -> usize {
        if self.lookup.is_some() { MAX_EXECUTABLE_HOPS } else { MAX_HOPS_WITH_A_BINNED_LEG }
    }

    /// The first mint of `plan` the wallet holds no account for, if the set is known.
    #[must_use]
    pub fn missing_account(&self, plan: &CyclePlan) -> Option<Pubkey32> {
        self.mint_without_an_account(plan)
    }

    /// Open the account for one mint, mid-run, and start treating it as held.
    ///
    /// The same path as the startup open — simulated first, re-sent until confirmed —
    /// so everything said on [`Trader::ensure_token_accounts`] holds. Returns whether
    /// the account exists afterwards; `false` in a dry run, which opens nothing.
    ///
    /// # Errors
    /// If the chain cannot be reached or the open does not simulate cleanly.
    pub async fn open_account(&mut self, mint: &Pubkey32) -> Result<bool> {
        self.ensure_token_accounts(std::slice::from_ref(mint)).await?;
        let missing = self.learn_token_accounts(std::slice::from_ref(mint)).await?;
        Ok(missing.is_empty())
    }

    /// Close the wallet's account for `mint` and take its deposit back, if it is empty.
    ///
    /// Returns whether the account is gone afterwards. An account that still holds a
    /// balance is left alone and reported as `false`: closing one would fail on chain,
    /// and emptying it is a trade, which is not this function's to make.
    ///
    /// # Errors
    /// If the chain cannot be reached, or the close does not simulate cleanly.
    pub async fn close_account(&mut self, mint: &Pubkey32) -> Result<bool> {
        let program = self.token_program_of(mint);
        let ata = associated_token_address(&self.owner, &to_pubkey(mint), &program);
        let found = self.exec.rpc.accounts_full(std::slice::from_ref(&ata)).await?;
        let Some(acc) = found.into_iter().next().flatten() else {
            self.accounts_held.remove(mint);
            return Ok(true);
        };
        // A token account's amount sits at bytes 64..72 under both programs.
        let amount = acc
            .data
            .get(64..72)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map_or(u64::MAX, u64::from_le_bytes);
        if amount != 0 {
            tracing::info!(
                "kept the account for {}: it holds {amount} base units, and only an empty \
                 account can be closed",
                to_pubkey(mint)
            );
            return Ok(false);
        }
        let ixs = vec![
            tx::set_compute_limit(20_000),
            tx::close_token_account(&ata, &self.owner, &self.owner, &program),
        ];
        let (blockhash, _) = self.exec.rpc.latest_blockhash().await?;
        let assembled = tx::assemble(&self.exec.wallet, &ixs, blockhash)?;
        let sim = self.exec.rpc.simulate(&assembled.tx_base64, &[]).await?;
        if !sim.succeeded() {
            anyhow::bail!(
                "closing the account for {} did not simulate cleanly, so nothing was sent: {} {}",
                to_pubkey(mint),
                sim.err.clone().unwrap_or_else(|| "unknown".into()),
                sim.error_context().unwrap_or_default()
            );
        }
        if self.opts.dry_run {
            tracing::info!("dry run — closing the account for {} simulated cleanly", to_pubkey(mint));
            return Ok(false);
        }
        let signature = self.exec.rpc.send(&assembled.tx_base64, true).await?;
        match self.confirm(&signature, 10).await {
            Some(true) => {
                self.accounts_held.remove(mint);
                tracing::warn!(
                    "closed the account for {} and took back its {} lamport deposit: {signature}",
                    to_pubkey(mint),
                    acc.lamports
                );
                Ok(true)
            }
            Some(false) => anyhow::bail!("closing the account reverted on chain: {signature}"),
            None => {
                // Not known is not failed — but a close that lands after the wait would
                // leave a mint marked held with no account behind it, and trades through
                // it would then skip the create they need. Forget it instead: if the
                // account survived, the next open finds it and costs nothing.
                self.accounts_held.remove(mint);
                tracing::warn!("{signature} (closing an account) has not confirmed yet");
                Ok(false)
            }
        }
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

    /// Read PumpSwap's fee recipients from its `GlobalConfig`, so a PumpSwap hop can be
    /// encoded. The first of each list; the program accepts any of the eight.
    ///
    /// # Errors
    /// If the config cannot be read or decoded.
    pub async fn load_pump_fees(&mut self) -> Result<()> {
        let data = self
            .exec
            .rpc
            .accounts(&[pk(cb_dex::pumpswap::GLOBAL_CONFIG)])
            .await?
            .pop()
            .flatten()
            .context("PumpSwap's GlobalConfig does not exist")?;
        let g = cb_dex::pumpswap::decode_global_config(&data)?;
        self.pump_fees = Some(cb_executor::venue::pumpswap::PumpFeeRecipients {
            protocol: to_pubkey(&g.protocol_fee_recipients[0]),
            buyback: to_pubkey(&g.buyback_fee_recipients[0]),
        });
        Ok(())
    }

    /// Create the wallet's PumpSwap volume accumulator if it does not exist.
    ///
    /// `buy_exact_quote_in` names it and creates it on first use at the payer's cost.
    /// Inside a trade that rent would come out of the balance the profit is measured
    /// in, and the floor would refuse the trade every time, so it is made here, once,
    /// as setup. Its rent comes back if it is ever closed. Returns whether one was made.
    ///
    /// # Errors
    /// If it cannot be read, or the setup transaction fails.
    pub async fn ensure_pump_volume_accumulator(&self) -> Result<bool> {
        let key = cb_executor::venue::pumpswap::user_volume_accumulator(&self.owner);
        if self.exec.rpc.accounts(&[key]).await?.pop().flatten().is_some() {
            return Ok(false);
        }
        let ix = cb_executor::venue::pumpswap::init_user_volume_accumulator(&self.owner);
        self.send_setup(&[ix], "creating the PumpSwap volume accumulator").await?;
        Ok(true)
    }

    /// Prove the Jito path can land anything at all: one bundle holding nothing but the
    /// minimum tip.
    ///
    /// # Why
    ///
    /// By 2026-09-26 about sixty trade bundles had gone out and none was included, and
    /// the block engine's own status call answered `Invalid` for every one, so nothing
    /// said whether they lost their races or never reached a leader at all. A tip-only
    /// bundle has no floor to miss and no race to lose: if it is not included, the
    /// sending path itself is broken and no trade could ever have landed.
    ///
    /// Costs the minimum tip and the base fee (6,000 lamports) when it lands; nothing
    /// when it does not. Returns whether it landed, or `None` when this trader does not
    /// send through Jito.
    ///
    /// # Errors
    /// If the blockhash cannot be read or the block engine refuses the transaction.
    pub async fn jito_probe(&mut self) -> Result<Option<bool>> {
        if !self.sends_via_jito() || self.opts.dry_run {
            return Ok(None);
        }
        let (blockhash, _) = self.exec.rpc.latest_blockhash().await?;
        let seed = u64::from_le_bytes(blockhash.to_bytes()[..8].try_into().expect("a hash is 32 bytes"));
        let tip = cb_executor::jito::tip_account(seed);
        let ixs = [tx::transfer_lamports(&self.owner, &tip, cb_executor::jito::MIN_TIP_LAMPORTS)];
        let assembled = tx::assemble(&self.exec.wallet, &ixs, blockhash)?;
        let receipt = self.exec.rpc.send_jito(&self.jito_url, &assembled.tx_base64).await?;
        self.last_jito_send = Some(std::time::Instant::now());
        tracing::info!(
            "Jito self-test: sent a tip-only bundle {} (bundle {}); waiting up to 30 s for it",
            receipt.signature,
            receipt.bundle_id.as_deref().unwrap_or("unnamed")
        );
        Ok(Some(self.confirm(&receipt.signature, 15).await == Some(true)))
    }

    /// Keep the connections a trade needs open between trades. See
    /// [`cb_executor::rpc::Rpc::keep_warm`].
    ///
    /// Skips Jito when a send went out recently, since that already kept it warm and
    /// the ping would only spend the one-a-second allowance; otherwise it waits out
    /// the gap like a send and restarts it.
    pub async fn keep_warm(&mut self) {
        let jito = self.sends_via_jito() && !self.opts.dry_run;
        let gap = std::time::Duration::from_millis(cb_executor::jito::MIN_SEND_GAP_MS);
        let recent = self.last_jito_send.is_some_and(|at| at.elapsed() < KEEP_WARM_EVERY);
        let ping_jito = jito && !recent;
        if ping_jito {
            if let Some(wait) = self.last_jito_send.and_then(|at| gap.checked_sub(at.elapsed())) {
                tokio::time::sleep(wait).await;
            }
        }
        let url = ping_jito.then_some(self.jito_url.as_str());
        if let Err(e) = self.exec.rpc.keep_warm(url).await {
            tracing::debug!("{e:#}");
        }
        if ping_jito {
            self.last_jito_send = Some(std::time::Instant::now());
        }
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
    ///
    /// The configured bid is now a ceiling rather than the bid actually placed — see
    /// [`Trader::bid_for`] — so this is an upper bound, which is the right side for a
    /// budget to be wrong on.
    #[must_use]
    pub fn submission_cost_lamports(&self) -> u128 {
        match self.opts.submit {
            Submit::Rpc => {
                BASE_FEE_LAMPORTS
                    + priority_fee_lamports(self.opts.priority_micro_lamports, self.opts.compute_units)
            }
            // A bundle that fails is dropped and costs nothing, so this is only ever
            // charged if one somehow lands and reverts — which revert protection exists
            // to prevent. Priced at the most it could be, in case it does.
            Submit::Jito { tip_max_lamports, .. } => BASE_FEE_LAMPORTS
                + u128::from(tip_max_lamports.max(cb_executor::jito::MIN_TIP_LAMPORTS)),
        }
    }

    /// Whether trades go out as Jito bundles, whose misses cost nothing.
    #[must_use]
    pub fn sends_via_jito(&self) -> bool {
        matches!(self.opts.submit, Submit::Jito { .. })
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
        // A DLMM calls them x and y. Spending x moves the price down into lower bin
        // ids, which is the same relationship token A has to the price on the
        // concentrated venues, so x is token A.
        Dex::MeteoraDlmm => {
            let p = cb_dex::meteora_dlmm::decode(data)?;
            (p.token_x_mint, p.token_y_mint)
        }
        // PumpSwap's base is token A: spending it is a sale, the encoder's `input_is_a`.
        Dex::PumpSwap => {
            let p = cb_dex::pumpswap::decode_pool(data)?;
            (p.base_mint, p.quote_mint)
        }
        Dex::MeteoraDammV2 => {
            let p = cb_dex::meteora_damm_v2::decode_layout(data)?;
            (p.mint_a, p.mint_b)
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

    #[test]
    fn a_tip_never_costs_a_trade_the_gain_it_could_have_kept() {
        // Guaranteed 7,331 wanting a 2,338 tip: 5,000 + 2,338 > 7,331, refused before.
        assert_eq!(cap_tip(2_338, 7_331), 2_330, "the most that leaves one lamport");
        assert!(u128::from(cap_tip(2_338, 7_331)) + BASE_FEE_LAMPORTS < 7_331);
        assert_eq!(cap_tip(1_500, 100_000), 1_500, "room to spare keeps the full bid");
        assert_eq!(cap_tip(3_000, 5_200), 1_000, "never under the block engine minimum");
        assert_eq!(cap_tip(3_000, 0), 1_000);
    }

    #[test]
    fn only_a_loop_that_returns_to_its_hub_is_a_lollipop() {
        let mut p = plan(4);
        assert!(!p.is_lollipop(), "four distinct mints is an ordinary four-hop cycle");
        p.mints[3] = p.mints[1];
        assert!(p.is_lollipop());
        assert!(!plan(3).is_lollipop());
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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        };
        let p = plan(3);
        let data = pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 3];

        let (hops, _) = t.hops_for(&p, &data, &arrays, &[], &[], 0, 0).expect("a well formed plan");
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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        };
        let data = pools_for(&plan(3));
        let arrays = vec![[Pubkey::new_unique(); 3]; 3];

        let mut short = plan(3);
        short.mints.pop();
        assert!(t.hops_for(&short, &data, &arrays, &[], &[], 0, 0).is_err());

        let mut mismatched = plan(3);
        mismatched.leg_out.pop();
        assert!(t.hops_for(&mismatched, &data, &arrays, &[], &[], 0, 0).is_err());

        // Fewer accounts than pools must not silently build a shorter cycle.
        assert!(t.hops_for(&plan(3), &data[..2], &arrays, &[], &[], 0, 0).is_err());
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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        };
        let data = pools_for(&plan(2));
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];
        let e = t.hops_for(&plan(2), &data, &arrays, &[], &[], 0, 0).unwrap_err().to_string();
        assert!(e.contains("zero"), "{e}");
    }

    #[test]
    fn unencodable_venues_are_named_before_anything_is_fetched() {
        let mut p = plan(3);
        p.pools[1].1 = Dex::RaydiumCpmm;
        assert!(!p.encodable());
        assert_eq!(p.blocking_venue(), Some(Dex::RaydiumCpmm));

        let clean = plan(3);
        assert!(clean.encodable());
        assert_eq!(clean.blocking_venue(), None);
    }

    /// This asserted the opposite until v4 got an encoder, which is the point of
    /// pinning it: the router's idea of what can be built and the encoder's have to
    /// move together, and nothing else makes them.
    #[test]
    fn every_venue_the_router_will_plan_is_one_the_encoder_accepts() {
        for dex in [Dex::OrcaWhirlpool, Dex::RaydiumClmm, Dex::RaydiumAmmV4, Dex::PumpSwap, Dex::MeteoraDammV2] {
            assert!(has_encoder(dex), "{} is planned but cannot be built", dex.name());
        }
        assert!(!has_encoder(Dex::RaydiumCpmm), "CP-Swap has an encoder now; say so here");
        // Only the two tick venues go near the tick-array resolver.
        assert!(is_concentrated(Dex::OrcaWhirlpool) && is_concentrated(Dex::RaydiumClmm));
        assert!(!is_concentrated(Dex::RaydiumAmmV4), "v4 is constant-product, it has no ticks");
    }

    /// The two facts are separate. v4 re-prices from its vaults, fetched beside the
    /// pool, so it is now both.
    #[test]
    fn a_venue_that_can_be_built_but_not_priced_is_refused_before_anything_is_fetched() {
        assert!(has_encoder(Dex::RaydiumAmmV4));
        assert!(can_reprice(Dex::RaydiumAmmV4), "its vaults join the pool in one read");
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

    pub(super) fn throwaway() -> Trader {
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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        };
        let (hops, _) = t.hops_for(&plan, &pool_data, &arrays, &vec![None; plan.pools.len()], &[], 0, 0).expect("hops");

        let opts = RouteOptions {
            compute_units: 600_000,
            priority_micro_lamports: 0,
            wsol: WsolPolicy::WrapAndClose,
            create_token_accounts: true,
            others_exist: false,
            venue: VenueExtra {
                token_program: pk(programs::SPL_TOKEN),
                bitmap_policy: BitmapPolicy::Include,
                pump: None,
            },
            min_gain: 0,
            tip: None,
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

    /// The Jito tip, run for real against mainnet state and never sent.
    ///
    /// Market-independent on purpose: it wraps, closes, and tips with no swap in between,
    /// so nothing about today's prices can make it pass or fail. What it proves is the
    /// accounting `route::build` relies on — that the tip executes inside the trade,
    /// after the wSOL account has closed, from the very balance the profit is read from,
    /// so the balance falls by exactly the base fee and the tip and nothing else.
    ///
    /// ```text
    /// CB_SIM_AS=<funded pubkey> cargo test -p cb-bot a_jito_tip -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "hits mainnet; needs CB_SIM_AS"]
    async fn a_jito_tip_leaves_the_profit_balance_by_exactly_itself() {
        use cb_executor::tx;
        let Ok(who) = std::env::var("CB_SIM_AS") else {
            println!("set CB_SIM_AS to a funded public address; skipping");
            return;
        };
        let owner: Pubkey = who.parse().expect("CB_SIM_AS must be a public key");
        let rpc = Rpc::new("https://api.mainnet-beta.solana.com").expect("client");

        let token = pk(programs::SPL_TOKEN);
        let wsol = pk(programs::WSOL_MINT);
        let ata = cb_executor::pda::associated_token_address(&owner, &wsol, &token);
        let tip = 1_000u64;
        let ixs = vec![
            tx::set_compute_limit(100_000),
            tx::create_ata_idempotent(&owner, &ata, &owner, &wsol, &token),
            tx::transfer_lamports(&owner, &ata, 2_000_000),
            tx::sync_native(&ata),
            tx::close_account(&ata, &owner, &owner),
            tx::transfer_lamports(&owner, &cb_executor::jito::tip_account(5), tip),
        ];
        // Compared against the same trade without the tip, simulated back to back,
        // rather than against a balance read: `getBalance` and `simulateTransaction`
        // answer at different commitments, and on a busy address those differ by
        // whatever moved in between. Two simulations of the same state differ by the tip
        // and nothing else — the base fee is charged to both. A few tries, because even
        // two back-to-back simulations can straddle a slot on an address this busy.
        let untipped = ixs[..ixs.len() - 1].to_vec();
        let mut seen = Vec::new();
        for _ in 0..5 {
            let (bh, _) = rpc.latest_blockhash().await.expect("rpc");
            let run = |set: Vec<solana_sdk::instruction::Instruction>| {
                let rpc = &rpc;
                async move {
                    let c = tx::compile_unsigned(&owner, &set, bh).expect("fits");
                    rpc.simulate(&c.tx_base64, &[owner]).await.expect("rpc")
                }
            };
            let (with, without) = tokio::join!(run(ixs.clone()), run(untipped.clone()));
            for sim in [&with, &without] {
                if let Some(e) = &sim.err {
                    for l in &sim.logs {
                        println!("  log: {l}");
                    }
                    panic!("the wrap-and-close failed: {e}");
                }
            }
            let (a, b) = (without.post_lamports[0], with.post_lamports[0]);
            println!("without tip {a}, with tip {b}");
            if a.checked_sub(b) == Some(tip) {
                return;
            }
            seen.push((a, b));
        }
        panic!("the tip never showed up as exactly {tip} lamports: {seen:?}");
    }

    /// The exact numbers from the live run this fix came from: a wallet holding
    /// 132,746,877 lamports, a 2-mint wrapping cycle sized to spend 128,808,539 — which
    /// the old code approved and the chain rejected with `ResultWithNegativeLamports`
    /// on the wrap transfer, three times, until the risk gate halted trading.
    #[test]
    fn the_live_failure_is_caught_before_it_repeats() {
        let shortfall = wrap_shortfall(128_808_539, 2, 132_746_877, route::TOKEN_ACCOUNT_RENT);
        assert!(shortfall.is_some(), "the sizing that failed on mainnet must now be refused");
        let reserved = shortfall.unwrap();
        assert_eq!(reserved, 2 * 2_039_280 + 100_000);
        // And it must actually be a shortfall by the numbers, not a coincidence.
        assert!(128_808_539 + reserved > 132_746_877);
    }

    #[test]
    fn a_wrap_with_real_headroom_is_not_refused() {
        // Same wallet, a size that leaves the reserve intact.
        assert!(wrap_shortfall(100_000_000, 2, 132_746_877, route::TOKEN_ACCOUNT_RENT).is_none());
    }

    /// More mints touched means more rent reserved: a wallet with exactly the reserve
    /// for two mints has room to spare for one and none at all for two.
    #[test]
    fn more_distinct_mints_reserve_more() {
        let bal_for_two = 2 * 2_039_280 + 100_000;
        assert!(wrap_shortfall(0, 1, bal_for_two, route::TOKEN_ACCOUNT_RENT).is_none(), "1 mint fits inside 2 mints' reserve");
        assert!(wrap_shortfall(0, 2, bal_for_two, route::TOKEN_ACCOUNT_RENT).is_none(), "exactly enough is enough");
        assert!(wrap_shortfall(0, 2, bal_for_two - 1, route::TOKEN_ACCOUNT_RENT).is_some(), "one lamport short must refuse");
        assert!(
            wrap_shortfall(0, 3, bal_for_two, route::TOKEN_ACCOUNT_RENT).is_some(),
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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        };

        let mut p = tests::plan(2);
        let data = tests::pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];

        let (honest, _) = t.hops_for(&p, &data, &arrays, &[], &[], 0, 0).expect("a well formed plan");

        // Now claim, in the plan only, that every leg returns a hundred times more.
        // The pools handed to `hops_for` are unchanged, so nothing about what the chain
        // would actually pay has moved.
        for q in &mut p.leg_out {
            *q *= 100;
        }
        let (inflated, _) = t.hops_for(&p, &data, &arrays, &[], &[], 0, 0).expect("a well formed plan");

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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        };
        let p = tests::plan(3);
        let (hops, spent) = t
            .hops_for(&p, &tests::pools_for(&p), &vec![[Pubkey::new_unique(); 3]; 3], &[], &[], 0, 0)
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
        assert!(Trader::fresh_leg(Dex::RaydiumAmmV4, [7u8; 32], &[0u8; 300], &[1u8; 32], 2_500, None, &[])
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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        };
        let mut p = tests::plan(2);
        let data = tests::pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];

        // Ask for far more than any tick interval can hold. Nothing about the pools
        // changed, so the honest answer is "trade what is there", not "trade nothing".
        p.amount_in = 100_000_000_000_000_000;
        let (hops, spent) =
            t.hops_for(&p, &data, &arrays, &[], &[], 0, 0).expect("an oversized plan is still tradeable");
        assert!(spent < p.amount_in, "it must not pretend the room is there");

        // The ceiling is a property of the pools, not of how much was asked for.
        let mut greedier = p.clone();
        greedier.amount_in = p.amount_in * 10;
        let (_, again) = t.hops_for(&greedier, &data, &arrays, &[], &[], 0, 0).expect("still tradeable");
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
            bins_seen: HashMap::new(),
                vaults_seen: HashMap::new(),
                token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
            };
            let _ = &t;
            let p = tests::plan(2);
            Trader::fresh_legs(&p, &tests::pools_for(&p), &[], &[]).expect("fixtures price")
        };
        let spend = tests::plan(2).amount_in;

        let widest = Trader::widest_buildable_haircut(&legs, spend, 1, 0).expect("some width works");
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

    fn jito_trader(tip_max_lamports: u64) -> Trader {
        Trader {
            exec: unreachable_executor(),
            opts: TradeOptions {
                submit: Submit::Jito { tip_max_lamports, simulate_first: true },
                ..Default::default()
            },
            owner: Pubkey::new_unique(),
            ticks_seen: HashMap::new(),
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        }
    }

    /// Through Jito the last floor carries the cost of landing, so the chain itself
    /// refuses a trade that would land short of its fee — and it carries exactly that,
    /// no more, because every lamport above it is tolerance given away.
    #[test]
    fn a_jito_floor_guarantees_the_cost_of_landing_and_not_a_lamport_more() {
        let t = jito_trader(20_000);
        let p = tests::plan(2);
        let data = tests::pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];

        // The fixture's two pools sit at tick 32 and return about 58 bps round trip on
        // a 1,000,000 input, so a 3,000-unit cost fits with room to spare.
        let cost = 3_000u128;
        let (hops, spent) =
            t.hops_for(&p, &data, &arrays, &[], &[], 0, cost).expect("a well formed plan");
        let last = hops.last().expect("two hops");
        assert_eq!(
            u128::from(last.min_amount_out),
            spent + cost + 1,
            "the last floor should sit exactly one unit above input plus cost"
        );
        // And every earlier hop still funds the next.
        assert!(hops[0].min_amount_out >= hops[1].amount_in);

        // The same trade with the cost stripped out keeps the old, higher haircut
        // floor: the lowering applies only where the cost is guaranteed.
        let (plain, _) = t.hops_for(&p, &data, &arrays, &[], &[], 0, 0).expect("well formed");
        assert!(plain.last().unwrap().min_amount_out > last.min_amount_out);
    }

    /// A cost the edge cannot carry is not lowered into a loss: the floor stays where
    /// the haircut put it, and `route::build` refuses the trade by name.
    #[test]
    fn a_cost_the_edge_cannot_carry_is_refused_rather_than_absorbed() {
        let t = jito_trader(20_000);
        let p = tests::plan(2);
        let data = tests::pools_for(&p);
        let arrays = vec![[Pubkey::new_unique(); 3]; 2];

        let cost = 50_000u128; // five per cent of the input: far past the edge.
        let (hops, spent) =
            t.hops_for(&p, &data, &arrays, &[], &[], 0, cost).expect("a well formed plan");
        assert!(u128::from(hops.last().unwrap().min_amount_out) < spent + cost);

        let mut wsol_hops = hops.clone();
        wsol_hops[0].input_mint = pk(programs::WSOL_MINT);
        wsol_hops[1].output_mint = pk(programs::WSOL_MINT);
        let opts = RouteOptions {
            wsol: WsolPolicy::WrapAndClose,
            min_gain: u64::try_from(cost).unwrap(),
            tip: Some((cb_executor::jito::tip_account(1), 1_000)),
            ..RouteOptions::default()
        };
        let e = route::build(&Pubkey::new_unique(), &wsol_hops, 1_000_000_000, &opts)
            .expect_err("a floor below the cost of landing must not build")
            .to_string();
        assert!(e.contains("loses its own fee"), "unexpected refusal: {e}");
    }

    /// A quarter of the prize, never under the block engine's minimum, never over the
    /// configured ceiling — and nothing at all off Jito.
    #[test]
    fn the_tip_is_a_quarter_of_the_prize_between_the_minimum_and_the_ceiling() {
        let t = jito_trader(20_000);
        let min = cb_executor::jito::MIN_TIP_LAMPORTS;

        assert_eq!(t.tip_for(400), min, "a small prize still pays the minimum");
        assert_eq!(t.tip_for(40_000), 10_000, "a quarter of 40,000");
        assert_eq!(t.tip_for(4_000_000), 20_000, "capped at the ceiling");
        assert_eq!(t.tip_for(0), min, "a cycle grossing nothing still tips the minimum");

        let rpc = Trader { opts: TradeOptions::default(), ..jito_trader(20_000) };
        assert_eq!(rpc.tip_for(40_000), 0, "no tip when not sending through Jito");
    }

    /// The tip follows what the pools pay now, not what the plan said they paid when it
    /// was detected, which is the number three quarters of detections have wrong.
    #[test]
    fn the_tip_is_priced_from_the_fresh_quote_and_not_the_plan() {
        let t = jito_trader(20_000);
        let mut p = tests::plan(2);
        let data = tests::pools_for(&p);
        let honest = Trader::fresh_gross(&p, &data, &[], &[]);
        assert!(honest > 0, "the fixture grosses about 58 bps");

        // Claim, in the plan only, a gross a hundred times larger.
        p.leg_out = vec![p.leg_out[0] * 100, p.amount_in * 100];
        assert_eq!(Trader::fresh_gross(&p, &data, &[], &[]), honest);
        assert_eq!(t.tip_for(Trader::fresh_gross(&p, &data, &[], &[])), t.tip_for(honest));
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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: std::collections::HashSet::new(),
            accounts_held: std::collections::HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
        };
        let p = tests::plan(2);
        // Pools that decode but trade the wrong mints cannot be re-priced at all.
        let wrong = vec![tests::whirlpool_with([9u8; 32], [8u8; 32]); 2];
        let e = t.hops_for(&p, &wrong, &[[Pubkey::new_unique(); 3]; 2], &[], &[], 0, 0).unwrap_err();
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
        let fetch = src.find("tokio::try_join!(rpc.accounts_latest(&keys)").expect("one round trip");
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
            bins_seen: HashMap::new(),
            vaults_seen: HashMap::new(),
            token_2022_mints: HashSet::new(),
            accounts_held: HashSet::new(),
            jito_url: cb_executor::jito::DEFAULT_URL.to_string(),
            last_jito_send: None,
            pump_fees: None,
            account_rent: cb_executor::route::TOKEN_ACCOUNT_RENT,
            lookup: None,
            lookup_pending: Vec::new(),
            oversize: None,
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

/// The binned venue, end to end through this file's own predicates and re-pricer.
#[cfg(test)]
mod meteora_dlmm_tests {
    use super::mainnet::throwaway;
    use super::tests::plan;
    use super::*;

    fn b64(s: &str) -> Vec<u8> {
        let table = |c: u8| -> i32 {
            match c {
                b'A'..=b'Z' => i32::from(c - b'A'),
                b'a'..=b'z' => i32::from(c - b'a') + 26,
                b'0'..=b'9' => i32::from(c - b'0') + 52,
                b'+' => 62,
                b'/' => 63,
                _ => -1,
            }
        };
        let (mut acc, mut bits, mut out) = (0i32, 0, Vec::new());
        for &c in s.trim().as_bytes() {
            let v = table(c);
            if v < 0 {
                continue;
            }
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from((acc >> bits) & 0xFF).unwrap_or(0));
            }
        }
        out
    }

    fn pool_and_array() -> (Vec<u8>, Vec<u8>) {
        (
            b64(include_str!("../../dex/tests/fixtures/meteora_dlmm_pair.b64")),
            b64(include_str!("../../dex/tests/fixtures/meteora_dlmm_bin_array.b64")),
        )
    }

    fn pool_key() -> Pubkey32 {
        let mut k = [0u8; 32];
        k.copy_from_slice(
            &bs58::decode("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR").into_vec().unwrap(),
        );
        k
    }

    /// The three predicates have to agree on what shape this venue is, because each one
    /// gates a different part of the attempt and disagreeing means an account is fetched
    /// for one purpose and missing for another.
    #[test]
    fn the_venue_is_binned_encodable_and_re_priceable_all_at_once() {
        assert!(has_encoder(Dex::MeteoraDlmm), "the swap2 encoder is wired in");
        assert!(can_reprice(Dex::MeteoraDlmm), "its bins arrive with its pool account");
        assert!(is_binned(Dex::MeteoraDlmm));
        assert!(!is_concentrated(Dex::MeteoraDlmm), "it has no ticks to sweep");
        // And no other venue may claim to be binned, or it would be sent looking for bin
        // arrays that do not exist.
        for other in [
            Dex::OrcaWhirlpool,
            Dex::RaydiumClmm,
            Dex::RaydiumAmmV4,
            Dex::RaydiumCpmm,
            Dex::MeteoraDammV2,
            Dex::PumpSwap,
        ] {
            assert!(!is_binned(other), "{other:?} is not a binned venue");
        }
    }

    /// A binned hop re-prices from the pool account *and* its bin array, and refuses
    /// without the array — which is the difference between this venue and the
    /// concentrated ones, and the reason it needed its own fetch path.
    #[test]
    fn a_binned_hop_prices_from_its_bins_and_refuses_without_them() {
        let (pool, array) = pool_and_array();
        let pair = cb_dex::meteora_dlmm::decode(&pool).unwrap();

        let leg = Trader::fresh_leg(
            Dex::MeteoraDlmm,
            pool_key(),
            &pool,
            &pair.token_x_mint,
            0,
            None,
            &[&array],
        )
        .expect("a real pool and its real active bin array must price");
        assert!(leg.max_in > 0, "and must carry the bins' own depth as its bound");
        assert!(leg.max_in < u128::MAX, "a binned leg is never unbounded");
        assert!(leg.fee_ppm >= 100, "a one-basis-point pool charges at least a basis point");

        assert!(
            Trader::fresh_leg(
                Dex::MeteoraDlmm,
                pool_key(),
                &pool,
                &pair.token_x_mint,
                0,
                None,
                &[]
            )
            .is_none(),
            "with no bin array there is no depth, and a price without depth is the mistake \
             this venue exists to avoid"
        );
    }

    /// The fee must come from the pool, not from the plan. A DLMM's fee moves with its own
    /// volatility, so the registry's number is the one field here that is certainly stale.
    #[test]
    fn the_fee_is_taken_from_the_pool_and_not_from_the_plan() {
        let (pool, array) = pool_and_array();
        let pair = cb_dex::meteora_dlmm::decode(&pool).unwrap();
        let absurd = 900_000; // 90%, which would make any cycle hopeless
        let leg = Trader::fresh_leg(
            Dex::MeteoraDlmm,
            pool_key(),
            &pool,
            &pair.token_x_mint,
            absurd,
            None,
            &[&array],
        )
        .expect("prices regardless");
        assert!(leg.fee_ppm < 1_000, "the plan's {absurd} ppm leaked into the leg");
    }

    /// The direction has to be read from the account. Token X is the one whose spending
    /// moves the price down, and getting it backwards reverses the swap.
    #[test]
    fn token_x_is_this_venues_token_a() {
        let (pool, _) = pool_and_array();
        let pair = cb_dex::meteora_dlmm::decode(&pool).unwrap();
        assert!(input_is_token_a(Dex::MeteoraDlmm, &pool, &pair.token_x_mint).unwrap());
        assert!(!input_is_token_a(Dex::MeteoraDlmm, &pool, &pair.token_y_mint).unwrap());
        assert!(input_is_token_a(Dex::MeteoraDlmm, &pool, &[0xAB; 32]).is_err());
        assert_eq!(mint_a_of(Dex::MeteoraDlmm, &pool).unwrap(), pair.token_x_mint);
        assert_eq!(program_for(Dex::MeteoraDlmm), pk(cb_dex::meteora_dlmm::PROGRAM_ID));
    }

    /// The whole point of the prize-scaled bid: a small window bids little, a large one
    /// bids up to the operator's ceiling and no further.
    #[test]
    fn the_bid_follows_the_prize_and_stops_at_the_configured_ceiling() {
        let mut t = throwaway();
        t.opts.priority_micro_lamports = 8_000;
        t.opts.compute_units = 300_000;

        // A window worth about $0.0015 at SOL $102.70 — 14,600 lamports of gross. A
        // quarter of that is 3,650 lamports, which at a 300,000 limit is 12,166
        // micro-lamports: more than the ceiling, so the ceiling stands.
        let mut p = plan(2);
        p.amount_in = 100_000_000;
        p.leg_out = vec![0, 100_014_600];
        assert_eq!(t.bid_for(&p, true), 8_000);

        // A window ten times smaller cannot afford the ceiling and must not pay it.
        p.leg_out = vec![0, 100_001_460];
        let small = t.bid_for(&p, true);
        assert!(small > 0 && small < 8_000, "expected a scaled bid, got {small}");
        assert_eq!(small, 1_216, "a quarter of 1,460 lamports over a 300,000 unit limit");

        // No prize, no bid. Paying to land a trade that earns nothing is the one case
        // where losing the race is strictly better.
        p.leg_out = vec![0, p.amount_in];
        assert_eq!(t.bid_for(&p, true), 0);

        // And when the profit is not in lamports the share cannot be computed, so the
        // configured number stands rather than being guessed at.
        p.leg_out = vec![0, 100_001_460];
        assert_eq!(t.bid_for(&p, false), 8_000);
    }

    /// A scaled bid must never cost more than either the operator's ceiling or its own
    /// share of the prize, at any size.
    #[test]
    fn a_scaled_bid_never_exceeds_the_ceiling_or_its_share() {
        let mut t = throwaway();
        t.opts.priority_micro_lamports = 8_000;
        t.opts.compute_units = 300_000;
        let ceiling = priority_fee_lamports(8_000, 300_000);
        let mut p = plan(2);
        p.amount_in = 100_000_000;
        for gross in [0u128, 100, 1_000, 10_000, 100_000, 10_000_000] {
            p.leg_out = vec![0, p.amount_in + gross];
            let bid = t.bid_for(&p, true);
            let cost = priority_fee_lamports(bid, 300_000);
            assert!(cost <= ceiling, "a scaled bid of {bid} costs {cost}, over the ceiling");
            assert!(
                cost <= gross * MAX_BID_SHARE_PERCENT / 100 + 1,
                "bid {bid} costs {cost} against a gross of {gross}"
            );
        }
    }
    /// Whether a cycle with a binned leg still fits in one packet.
    ///
    /// Not a formality. A DLMM `swap2` names nineteen accounts against a Whirlpool swap's
    /// eleven, and a transaction is capped at 1,232 bytes with every distinct account
    /// costing 32 of them. If a two-hop cycle through this venue did not fit, the encoder
    /// would be perfectly correct and the venue would still be untradeable — and that
    /// failure arrives as `route::build` refusing every cycle at run time, which reads
    /// like a market condition rather than an arithmetic one.
    #[test]
    fn a_two_hop_cycle_through_a_binned_pool_fits_in_one_packet() {
        use cb_executor::tx;
        use solana_sdk::hash::Hash;

        let (pool, _) = pool_and_array();
        let pair = cb_dex::meteora_dlmm::decode(&pool).unwrap();
        let owner = Pubkey::new_unique();
        let wsol = pk(programs::WSOL_MINT);
        let usdc = to_pubkey(&pair.token_y_mint);
        let token_program = pk(programs::SPL_TOKEN);
        let dlmm_pool = to_pubkey(&pool_key());

        let hops = vec![
            Hop {
                pool: dlmm_pool,
                dex: Dex::MeteoraDlmm,
                pool_data: pool.clone(),
                input_mint: wsol,
                output_mint: usdc,
                input_is_a: true,
                input_token_program: token_program,
                output_token_program: token_program,
                amount_in: 100_000_000,
                min_amount_out: 1,
                tick_arrays: cb_executor::venue::meteora_dlmm::bin_arrays_for(
                    &dlmm_pool,
                    pair.active_id,
                    true,
                ),
            },
            Hop {
                pool: Pubkey::new_unique(),
                dex: Dex::OrcaWhirlpool,
                pool_data: super::tests::whirlpool_with(pair.token_y_mint, wsol.to_bytes()),
                input_mint: usdc,
                output_mint: wsol,
                input_is_a: true,
                input_token_program: token_program,
                output_token_program: token_program,
                amount_in: 1,
                min_amount_out: 100_000_001,
                tick_arrays: [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()],
            },
        ];

        let opts = RouteOptions {
            compute_units: 300_000,
            priority_micro_lamports: 8_000,
            wsol: WsolPolicy::WrapAndClose,
            create_token_accounts: true,
            others_exist: false,
            venue: VenueExtra::default(),
            min_gain: 0,
            tip: None,
        };
        let built = route::build(&owner, &hops, 200_000_000, &opts)
            .expect("these hops close and guarantee more than they spend");
        let size = tx::measure(&owner, &built.instructions, Hash::default())
            .expect("a two-hop cycle must serialise");

        // The same cycle shaped for Jito: no price instruction, one tip transfer, and a
        // last floor that covers both. The tip account is a new key; the system program
        // is already present for the wrap. Must fit too, or sending this way would be
        // refused on the one venue that most needs the room.
        let mut jito_hops = hops.clone();
        jito_hops[1].min_amount_out = 100_010_000;
        let jito_opts = RouteOptions {
            priority_micro_lamports: 0,
            min_gain: 6_000,
            tip: Some((cb_executor::jito::tip_account(0), 1_000)),
            ..opts
        };
        let jito_built = route::build(&owner, &jito_hops, 200_000_000, &jito_opts)
            .expect("the jito shape closes and covers its costs");
        let jito_size = tx::measure(&owner, &jito_built.instructions, Hash::default())
            .expect("the jito shape must serialise");
        assert!(
            jito_size <= tx::PACKET_LIMIT,
            "a two-hop DLMM cycle sent through Jito serialises to {jito_size} bytes \
             against a {} byte packet",
            tx::PACKET_LIMIT
        );
        // One key and one small instruction in, one small instruction out.
        assert!(
            jito_size > size && jito_size - size <= 48,
            "the Jito shape should cost about one account more: {size} -> {jito_size}"
        );
        assert!(
            size <= tx::PACKET_LIMIT,
            "a two-hop cycle through a DLMM serialises to {size} bytes against a {} byte \
             packet — the venue would be correctly encoded and still untradeable",
            tx::PACKET_LIMIT
        );
        // Measured at 1,143 bytes on 2026-09-12: 89 bytes of headroom out of 1,232.
        // Worth pinning as a number rather than only as a bound, because the margin is
        // the real finding. Eighty-nine bytes is not another account, let alone another
        // hop, so a three-hop cycle through this venue — or a two-hop with a DLMM on
        // both legs — will not fit. `route::build` refuses those cleanly, so nothing is
        // lost but the attempt, and buying the room back needs an address lookup table
        // rather than a smaller encoding.
        assert!(
            (1_100..=1_180).contains(&size),
            "the packet budget moved to {size} bytes; if it grew, check what still fits"
        );
    }
    /// The shape gate, which exists so a cycle that cannot fit is refused before four
    /// account fetches rather than after them. The numbers it enforces are the ones
    /// `a_two_hop_cycle_through_a_binned_pool_fits_in_one_packet` measured.
    #[tokio::test]
    async fn a_cycle_too_wide_for_a_packet_is_refused_before_anything_is_fetched() {
        let mut t = throwaway();
        // Three hops, one of them binned: over the packet by construction.
        let mut p = plan(3);
        p.pools[1].1 = Dex::MeteoraDlmm;
        let refusal = t
            .attempt(&p, 5.0, 0.01, Intent::Measure)
            .await
            .expect("a refusal is not an error");
        match refusal {
            Attempt::Refused(why) => assert!(
                why.contains("address lookup table"),
                "unexpected refusal: {why}"
            ),
            other => panic!("a three-hop binned cycle must be refused, got {other:?}"),
        }

        // Two binned legs do not fit beside each other either, even at two hops.
        let mut two = plan(2);
        two.pools[0].1 = Dex::MeteoraDlmm;
        two.pools[1].1 = Dex::MeteoraDlmm;
        match t.attempt(&two, 5.0, 0.01, Intent::Measure).await.expect("no error") {
            Attempt::Refused(why) => {
                assert!(why.contains("binned legs"), "unexpected refusal: {why}")
            }
            other => panic!("two binned legs must be refused, got {other:?}"),
        }

        // And one binned leg in a two-hop cycle must get past this gate — the whole
        // point is that the shape we can trade is not the one being refused.
        let mut ok = plan(2);
        ok.pools[0].1 = Dex::MeteoraDlmm;
        // Anything other than a refusal means it got past the gate, which is what is
        // being asserted here.
        if let Ok(Attempt::Refused(why)) = t.attempt(&ok, 5.0, 0.01, Intent::Measure).await {
            assert!(
                !why.contains("address lookup table") && !why.contains("binned legs"),
                "a tradeable shape was refused by the shape gate: {why}"
            );
        }
    }
}
