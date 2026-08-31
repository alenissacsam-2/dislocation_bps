//! Turning a detected cycle into one atomic transaction.
//!
//! # The two invariants that make a cycle safe to sign
//!
//! Each hop is encoded with a **fixed** input amount, decided when the transaction is
//! built. The chain does not thread one swap's output into the next swap's input; it
//! just runs three swaps that happen to share token accounts. That has two consequences
//! which are easy to miss and expensive to get wrong:
//!
//! 1. **A hop's output floor must cover the next hop's input.** If leg one is allowed
//!    to return less than leg two intends to spend, leg two fails on insufficient funds
//!    — after leg one has already moved money. Atomicity saves the balance, but the
//!    attempt is wasted and the failure looks like a bug in the encoder rather than a
//!    floor set too low. [`build`] refuses to encode a route that permits this.
//!
//! 2. **The last hop's floor must exceed the first hop's input.** This is the whole
//!    safety argument in one comparison. If it holds, a transaction that lands is
//!    profitable *by construction*, whatever happened to the quote in between — the
//!    programs themselves enforce it, and a cycle that has become unprofitable reverts
//!    instead of filling. If it does not hold, we have signed a transaction whose
//!    successful execution loses money.
//!
//! Neither check depends on this codebase's arithmetic being right. That is what makes
//! them worth more than the quote that motivated the trade.

use crate::encode::{pk, programs, to_pubkey};
use crate::pda::associated_token_address;
use crate::tx;
use crate::venue::{build_swap, SwapContext, VenueExtra};
use anyhow::{bail, ensure, Result};
use cb_core::types::Dex;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

/// Rent-exempt minimum for a 165-byte SPL token account, in lamports.
///
/// The same figure `cb_desk::balances` uses for the same reason: it is fixed by the
/// rent schedule and does not move, so a round trip to `getMinimumBalanceForRentExemption`
/// would ask the chain a question whose answer this codebase already knows.
pub const TOKEN_ACCOUNT_RENT: u64 = 2_039_280;

/// One leg of a cycle, with the pool account bytes that priced it.
#[derive(Debug, Clone)]
pub struct Hop {
    pub pool: Pubkey,
    pub dex: Dex,
    /// The pool account exactly as the node returned it.
    pub pool_data: Vec<u8>,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// True when `input_mint` is the pool's token A / token 0.
    pub input_is_a: bool,
    pub amount_in: u64,
    pub min_amount_out: u64,
    /// The tick arrays this hop will name. See [`crate::ticks::resolve`].
    pub tick_arrays: [Pubkey; crate::pda::TICK_ARRAYS_PER_SWAP],
}

/// How wrapped SOL is handled across the cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WsolPolicy {
    /// Assume the wrapped-SOL account already holds enough, and leave it open.
    ///
    /// The default, and the only one that lets the profit check read a token balance:
    /// a closed account has no balance to read, so the simulation comes back with
    /// nothing where the proof of profit should be.
    #[default]
    Reuse,
    /// Wrap `amount_in` lamports at the start and close the account at the end,
    /// recovering everything as native SOL.
    ///
    /// Correct and fully atomic, but the profit must then be read from the owner's
    /// lamport balance rather than a token account — see [`Route::profit`].
    WrapAndClose,
}

/// Where the proof of profit is to be read from after simulating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profit {
    /// Watch this token account's SPL amount.
    TokenAccount(Pubkey),
    /// Watch this address's native lamport balance. Nets out the transaction fee,
    /// because the fee comes out of the same balance.
    Lamports(Pubkey),
}

impl Profit {
    #[must_use]
    pub fn address(&self) -> Pubkey {
        match self {
            Profit::TokenAccount(k) | Profit::Lamports(k) => *k,
        }
    }
}

/// Options that are not per-hop.
#[derive(Debug, Clone, Copy)]
pub struct RouteOptions {
    pub compute_units: u32,
    pub priority_micro_lamports: u64,
    pub wsol: WsolPolicy,
    /// Emit an idempotent create for every token account the route touches.
    ///
    /// Costs one instruction and six accounts per mint. Worth it on the first trade of
    /// a session and pure overhead afterwards, so it is a switch rather than always-on.
    pub create_token_accounts: bool,
    pub venue: VenueExtra,
}

impl Default for RouteOptions {
    fn default() -> Self {
        Self {
            // Three CLMM swaps measured around 250k; the rest is headroom for a swap
            // that crosses more ticks than the quote expected.
            compute_units: 400_000,
            priority_micro_lamports: 0,
            wsol: WsolPolicy::default(),
            create_token_accounts: false,
            venue: VenueExtra::default(),
        }
    }
}

/// A cycle compiled to instructions, with the facts needed to judge its simulation.
#[derive(Debug, Clone)]
pub struct Route {
    pub instructions: Vec<Instruction>,
    pub profit: Profit,
    /// The balance the profit account must reach for this to have been worth doing.
    ///
    /// A **post-balance**, not a gain: it already includes whatever was there before.
    pub min_post_balance: u64,
    /// What the cycle starts and ends holding.
    pub base_mint: Pubkey,
    pub amount_in: u64,
}

/// Compile a cycle into the instruction list for one atomic transaction.
///
/// `pre_balance` is what the profit account holds now, and is what the required
/// post-balance is measured from. Passing zero when the account is not empty makes the
/// profit check trivially satisfiable, which is the failure mode this parameter exists
/// to make impossible to reach by accident.
///
/// # Errors
/// If the hops do not form a cycle, if either floor invariant in the module docs is
/// violated, or if a venue cannot be encoded.
pub fn build(
    owner: &Pubkey,
    hops: &[Hop],
    pre_balance: u64,
    opts: &RouteOptions,
) -> Result<Route> {
    ensure!(hops.len() >= 2, "a cycle needs at least two hops, got {}", hops.len());

    let base_mint = hops[0].input_mint;
    ensure!(
        hops[hops.len() - 1].output_mint == base_mint,
        "these hops do not close: they start at {base_mint} and end at {}",
        hops[hops.len() - 1].output_mint
    );

    // Invariant 1: each hop must be guaranteed to fund the next.
    for (i, pair) in hops.windows(2).enumerate() {
        let (this, next) = (&pair[0], &pair[1]);
        ensure!(
            this.output_mint == next.input_mint,
            "hop {i} outputs {} but hop {} spends {} — the chain is broken",
            this.output_mint,
            i + 1,
            next.input_mint
        );
        ensure!(
            this.min_amount_out >= next.amount_in,
            "hop {i} guarantees only {} but hop {} spends {} — the route can fail \
             part way through with money already moved",
            this.min_amount_out,
            i + 1,
            next.amount_in
        );
    }

    // Invariant 2: the whole point.
    let amount_in = hops[0].amount_in;
    let guaranteed_out = hops[hops.len() - 1].min_amount_out;
    ensure!(
        guaranteed_out > amount_in,
        "this route spends {amount_in} and guarantees only {guaranteed_out} back — \
         signing it would authorise a loss"
    );

    let token_program = opts.venue.token_program;
    let wsol = pk(programs::WSOL_MINT);
    let base_ata = associated_token_address(owner, &base_mint, &token_program);
    let wrapping = opts.wsol == WsolPolicy::WrapAndClose && base_mint == wsol;

    let mut ixs = vec![tx::set_compute_limit(opts.compute_units)];
    if opts.priority_micro_lamports > 0 {
        ixs.push(tx::set_compute_price(opts.priority_micro_lamports));
    }

    if opts.create_token_accounts {
        // Every mint the route touches, in first-seen order so the list is stable.
        let mut seen: Vec<Pubkey> = Vec::new();
        for h in hops {
            for m in [h.input_mint, h.output_mint] {
                if !seen.contains(&m) {
                    seen.push(m);
                }
            }
        }
        for mint in seen {
            let ata = associated_token_address(owner, &mint, &token_program);
            ixs.push(tx::create_ata_idempotent(owner, &ata, owner, &mint, &token_program));
        }
    }

    if wrapping {
        ixs.push(tx::transfer_lamports(owner, &base_ata, amount_in));
        ixs.push(tx::sync_native(&base_ata));
    }

    for h in hops {
        let ctx = SwapContext {
            owner: *owner,
            pool: h.pool,
            user_source: associated_token_address(owner, &h.input_mint, &token_program),
            user_dest: associated_token_address(owner, &h.output_mint, &token_program),
            amount_in: h.amount_in,
            min_amount_out: h.min_amount_out,
            input_is_a: h.input_is_a,
            tick_arrays: h.tick_arrays,
        };
        ixs.push(build_swap(h.dex, &ctx, &h.pool_data, &opts.venue)?);
    }

    let (profit, min_post_balance) = if wrapping {
        ixs.push(tx::close_account(&base_ata, owner, owner));
        // The lamport balance already contains everything else the owner holds, and
        // the fee will come out of it, so the floor is the current balance plus the
        // guaranteed gain minus nothing — the fee is what makes this conservative.
        let gain = guaranteed_out.saturating_sub(amount_in);
        (Profit::Lamports(*owner), pre_balance.saturating_add(gain))
    } else {
        let gain = guaranteed_out.saturating_sub(amount_in);
        (Profit::TokenAccount(base_ata), pre_balance.saturating_add(gain))
    };

    Ok(Route { instructions: ixs, profit, min_post_balance, base_mint, amount_in })
}

/// Convenience: the raw-key form the pure crates use.
#[must_use]
pub fn mint_key(raw: &cb_core::types::Pubkey32) -> Pubkey {
    to_pubkey(raw)
}

/// Refuse a venue we cannot encode, with the count of pools it covers, so the caller
/// can decide whether the cycle is worth skipping or the venue is worth building.
///
/// # Errors
/// Always — this exists to produce a uniform message.
pub fn unsupported(dex: Dex) -> Result<()> {
    bail!("{} is not encodable; the cycle containing it cannot be executed", dex.name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venue::raydium::BitmapPolicy;
    use solana_sdk::hash::Hash;

    pub(super) fn whirlpool(spacing: u16) -> Vec<u8> {
        let mut d = vec![0u8; cb_dex::orca_whirlpool::WHIRLPOOL_LEN];
        d[41..43].copy_from_slice(&spacing.to_le_bytes());
        d[43..45].copy_from_slice(&spacing.to_le_bytes());
        d[45..47].copy_from_slice(&400u16.to_le_bytes());
        d[49..65].copy_from_slice(&1_000_000_000_000u128.to_le_bytes());
        d[65..81].copy_from_slice(&(1u128 << 64).to_le_bytes());
        d[81..85].copy_from_slice(&0i32.to_le_bytes());
        // Distinct mints and vaults per pool, so nothing collides by accident.
        d[101..133].copy_from_slice(&[0xA0; 32]);
        d[133..165].copy_from_slice(&[0xA1; 32]);
        d[181..213].copy_from_slice(&[0xB0; 32]);
        d[213..245].copy_from_slice(&[0xB1; 32]);
        d
    }

    /// A cycle of `n` hops over distinct mints, each guaranteeing enough for the next
    /// and the last returning more than the first spent.
    pub(super) fn cycle(n: usize) -> Vec<Hop> {
        let mints: Vec<Pubkey> = (0..n).map(|_| Pubkey::new_unique()).collect();
        (0..n)
            .map(|i| Hop {
                pool: Pubkey::new_unique(),
                dex: Dex::OrcaWhirlpool,
                pool_data: whirlpool(64),
                input_mint: mints[i],
                output_mint: mints[(i + 1) % n],
                input_is_a: i % 2 == 0,
                amount_in: 1_000_000,
                // Each hop guarantees a touch more than the next one spends, and the
                // last more than the first spent.
                min_amount_out: 1_000_001 + i as u64,
                tick_arrays: [
                    Pubkey::new_unique(),
                    Pubkey::new_unique(),
                    Pubkey::new_unique(),
                ],
            })
            .collect()
    }

    pub(super) fn opts() -> RouteOptions {
        RouteOptions {
            venue: VenueExtra {
                token_program: pk(programs::SPL_TOKEN),
                bitmap_policy: BitmapPolicy::Include,
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_well_formed_cycle_compiles() {
        let owner = Pubkey::new_unique();
        let r = build(&owner, &cycle(3), 0, &opts()).expect("a closing, profitable cycle");
        // compute limit + three swaps.
        assert_eq!(r.instructions.len(), 4);
        assert_eq!(r.amount_in, 1_000_000);
        assert!(matches!(r.profit, Profit::TokenAccount(_)));
        assert!(r.min_post_balance > 0);
    }

    /// Invariant 2, stated as plainly as it can be: a route that returns less than it
    /// spends must not be encodable at all.
    #[test]
    fn a_route_that_guarantees_a_loss_is_refused() {
        let owner = Pubkey::new_unique();
        let mut hops = cycle(3);
        let last = hops.len() - 1;
        hops[last].min_amount_out = hops[0].amount_in; // exactly break even
        let e = build(&owner, &hops, 0, &opts()).unwrap_err().to_string();
        assert!(e.contains("authorise a loss"), "wrong refusal: {e}");

        hops[last].min_amount_out = hops[0].amount_in - 1; // an outright loss
        assert!(build(&owner, &hops, 0, &opts()).is_err());
    }

    /// Invariant 1. A middle hop that does not guarantee the next hop's input lets the
    /// transaction fail after money has moved.
    #[test]
    fn a_hop_that_cannot_fund_the_next_one_is_refused() {
        let owner = Pubkey::new_unique();
        let mut hops = cycle(3);
        hops[0].min_amount_out = hops[1].amount_in - 1;
        let e = build(&owner, &hops, 0, &opts()).unwrap_err().to_string();
        assert!(e.contains("part way through"), "wrong refusal: {e}");
    }

    #[test]
    fn hops_that_do_not_close_into_a_cycle_are_refused() {
        let owner = Pubkey::new_unique();
        let mut hops = cycle(3);
        hops[2].output_mint = Pubkey::new_unique();
        assert!(build(&owner, &hops, 0, &opts()).unwrap_err().to_string().contains("do not close"));

        let mut broken = cycle(3);
        broken[1].input_mint = Pubkey::new_unique();
        assert!(build(&owner, &broken, 0, &opts()).unwrap_err().to_string().contains("broken"));
    }

    #[test]
    fn a_single_hop_is_not_a_cycle() {
        assert!(build(&Pubkey::new_unique(), &cycle(3)[..1], 0, &opts()).is_err());
    }

    /// The required post-balance must be measured from what is already there, or a
    /// funded account satisfies the check without the trade having gained anything.
    #[test]
    fn the_profit_floor_is_measured_from_the_existing_balance() {
        let owner = Pubkey::new_unique();
        let hops = cycle(3);
        let gain = hops[2].min_amount_out - hops[0].amount_in;

        let empty = build(&owner, &hops, 0, &opts()).unwrap();
        assert_eq!(empty.min_post_balance, gain);

        let funded = build(&owner, &hops, 5_000_000, &opts()).unwrap();
        assert_eq!(funded.min_post_balance, 5_000_000 + gain);
    }

    /// Wrapping is only meaningful when the base mint is actually wrapped SOL, and it
    /// moves the profit check off the token account onto lamports.
    #[test]
    fn wrapping_applies_only_to_wrapped_sol_and_moves_the_profit_check() {
        let owner = Pubkey::new_unique();
        let o = RouteOptions { wsol: WsolPolicy::WrapAndClose, ..opts() };

        // A cycle whose base is not wSOL must not sprout wrap instructions.
        let plain = build(&owner, &cycle(3), 0, &o).unwrap();
        assert!(matches!(plain.profit, Profit::TokenAccount(_)));
        assert_eq!(plain.instructions.len(), 4);

        // One whose base is wSOL must wrap, sync, swap, and close.
        let mut hops = cycle(3);
        let wsol = pk(programs::WSOL_MINT);
        hops[0].input_mint = wsol;
        hops[2].output_mint = wsol;
        let wrapped = build(&owner, &hops, 0, &o).unwrap();
        assert!(matches!(wrapped.profit, Profit::Lamports(k) if k == owner));
        // compute + transfer + sync + 3 swaps + close
        assert_eq!(wrapped.instructions.len(), 7);
    }

    #[test]
    fn account_creation_covers_every_mint_exactly_once() {
        let owner = Pubkey::new_unique();
        let o = RouteOptions { create_token_accounts: true, ..opts() };
        let r = build(&owner, &cycle(3), 0, &o).unwrap();
        // compute + 3 creates + 3 swaps. Three mints, each seen twice, created once.
        assert_eq!(r.instructions.len(), 7);
    }

    /// The measurement `tx.rs` defers to. A real cycle shares the signer, the token
    /// program and the token accounts across legs, so it fits further than the
    /// no-sharing bound suggests — and this records exactly how far.
    #[test]
    fn a_real_three_hop_cycle_fits_in_a_packet_and_a_four_hop_one_does_not() {
        let owner = Pubkey::new_unique();

        let three = build(&owner, &cycle(3), 0, &opts()).unwrap();
        let size = tx::measure(&owner, &three.instructions, Hash::default()).unwrap();
        assert!(
            size <= tx::PACKET_LIMIT,
            "a three-hop cycle measured {size} bytes, over the {} limit",
            tx::PACKET_LIMIT
        );

        let four = build(&owner, &cycle(4), 0, &opts()).unwrap();
        let big = tx::measure(&owner, &four.instructions, Hash::default()).unwrap();
        assert!(
            big > tx::PACKET_LIMIT,
            "a four-hop cycle measured {big} bytes and fits, which contradicts the \
             hop ceiling documented in tx.rs — re-measure before relying on it"
        );
    }

    /// A venue with no encoder must stop the route rather than being skipped.
    #[test]
    fn a_cycle_through_an_unencodable_venue_refuses() {
        let owner = Pubkey::new_unique();
        let mut hops = cycle(3);
        hops[1].dex = Dex::RaydiumAmmV4;
        let e = build(&owner, &hops, 0, &opts()).unwrap_err().to_string();
        assert!(e.contains("Raydium AMM v4"), "refusal must name the venue: {e}");
    }
}

#[cfg(test)]
mod sizes {
    use super::tests::*;
    use super::*;
    use solana_sdk::hash::Hash;

    /// Prints the measured packet cost of each cycle length. Not an assertion — the
    /// assertions are in `a_real_three_hop_cycle_fits_in_a_packet_and_a_four_hop_one_does_not`;
    /// this exists so `cargo test -- --nocapture sizes` answers "how close is it?".
    #[test]
    fn report_cycle_sizes() {
        let owner = Pubkey::new_unique();
        for n in 2..=4 {
            let r = build(&owner, &cycle(n), 0, &opts()).unwrap();
            let size = tx::measure(&owner, &r.instructions, Hash::default()).unwrap();
            println!(
                "{n}-hop cycle: {size} bytes of {} ({} spare)",
                tx::PACKET_LIMIT,
                tx::PACKET_LIMIT as i64 - size as i64
            );
        }
    }
}
