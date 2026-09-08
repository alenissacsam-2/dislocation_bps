//! Choosing which tick arrays a swap should name, by asking the chain.
//!
//! # The measurement that made this necessary
//!
//! The obvious thing is to name the array containing the current tick and the two after
//! it in the direction the price will move. That is what the encoders did, and it is
//! wrong for a large minority of real pools.
//!
//! A tick array holds the *boundaries* of positions, not the liquidity between them. A
//! position spanning ticks 61000 to 62000 writes into the arrays containing 61000 and
//! 62000 and touches nothing between, so the array containing the current price is
//! created only if some position happens to start or end inside it. A deep, actively
//! trading pool can have no array at its current tick at all.
//!
//! Measured against the registry: of 48 Raydium CLMM pools, **23 had no array at the
//! derived current-tick address**. For one of them — WSOL/GEOD, spacing 1, tick 61364 —
//! the arrays at 61260, 61380 and 61440 all exist while the one at 61320, which
//! contains the tick, does not. The derivation was right; the assumption that the array
//! must exist was not.
//!
//! So this module sweeps a window of candidate addresses, asks which exist in one round
//! trip, and returns the ones that do. A pool with fewer than three initialised arrays
//! ahead of it is a pool whose swap may run out of liquidity, which is a fact worth
//! having before signing rather than after.

use crate::pda::{
    orca_tick_array, raydium_tick_array, tick_array_sweep, ORCA_TICKS_PER_ARRAY,
    RAYDIUM_TICKS_PER_ARRAY, TICK_ARRAYS_PER_SWAP,
};
use crate::rpc::Rpc;
use anyhow::{bail, Result};
use cb_core::types::Dex;
use solana_sdk::pubkey::Pubkey;

/// How far ahead to look for initialised arrays.
///
/// Wide enough that a pool with a gap in front of it is still tradeable, narrow enough
/// that the existence check stays one `getMultipleAccounts` call.
pub const SWEEP_WIDTH: usize = 12;

/// What the sweep found.
#[derive(Debug, Clone)]
pub struct Chosen {
    pub arrays: [Pubkey; TICK_ARRAYS_PER_SWAP],
    /// The start index of each chosen array, for logging and for diagnosing a swap that
    /// ran out of liquidity.
    pub starts: Vec<i32>,
    /// How many of the swept candidates existed at all.
    pub found: usize,
    /// True when the array containing the current tick was one of them.
    pub current_exists: bool,
}

/// How many ticks one array covers on this venue.
#[must_use]
fn per_array(dex: Dex) -> i32 {
    match dex {
        Dex::OrcaWhirlpool => ORCA_TICKS_PER_ARRAY,
        _ => RAYDIUM_TICKS_PER_ARRAY,
    }
}

/// Derive the candidate addresses for a pool, nearest first in the traversal direction.
#[must_use]
pub fn candidates(
    dex: Dex,
    pool: &Pubkey,
    program: &Pubkey,
    tick_current: i32,
    tick_spacing: u16,
    price_falling: bool,
) -> Vec<(i32, Pubkey)> {
    sweep(dex, pool, program, tick_current, tick_spacing, price_falling, SWEEP_WIDTH)
}

fn sweep(
    dex: Dex,
    pool: &Pubkey,
    program: &Pubkey,
    tick_current: i32,
    tick_spacing: u16,
    price_falling: bool,
    how_many: usize,
) -> Vec<(i32, Pubkey)> {
    tick_array_sweep(tick_current, tick_spacing, per_array(dex), price_falling, how_many)
        .into_iter()
        .map(|start| {
            let key = match dex {
                Dex::OrcaWhirlpool => orca_tick_array(pool, start, program),
                _ => raydium_tick_array(pool, start, program),
            };
            (start, key)
        })
        .collect()
}

/// Arrays to look behind the last known tick, so a price that moved *against* the
/// traversal direction is still inside the prefetched window.
const PREFETCH_BEHIND: i32 = 1;
/// How many arrays a prefetch asks for. Wider than [`SWEEP_WIDTH`] by the margin it
/// looks behind, plus one, so the real window is a subset from either side.
const PREFETCH_WIDTH: usize = SWEEP_WIDTH + 2;

/// A superset of the candidates a *slightly stale* tick implies, for asking about the
/// tick arrays in the same round trip that fetches the pools.
///
/// # Why this exists
///
/// [`resolve`] cannot run until the pool account has been read, because it needs the
/// pool's current tick — so an attempt pays for the pool fetch, then pays again for the
/// arrays, then again for the balance, then again for the simulation. Measured from the
/// machine this runs on, a round trip to the configured Helius endpoint is 98 ms (the
/// public fallback is 296 ms), so four of them is about 390 ms — one Solana slot
/// between reading a price and asking the chain to honour it, against an edge of one to
/// two basis points. The price moves a meaningful fraction of that edge in that window,
/// and the refusals said so: floors missed by roughly a basis point.
///
/// The dependency is real but the *precision* it needs is not. An array spans
/// `tick_spacing × ticks_per_array` ticks — hundreds of basis points on the pools this
/// trades — so a tick that moved between the sweep and the attempt lands in the same
/// window with room to spare. Prefetching from the last known tick, one array wider on
/// each side, and then checking the fresh tick's real window against what came back,
/// turns two round trips into one without assuming anything: when the check fails the
/// caller asks, exactly as it does today.
#[must_use]
pub fn prefetch_candidates(
    dex: Dex,
    pool: &Pubkey,
    program: &Pubkey,
    last_known_tick: i32,
    tick_spacing: u16,
    price_falling: bool,
) -> Vec<(i32, Pubkey)> {
    let span = i32::from(tick_spacing).saturating_mul(per_array(dex));
    // One array *against* the direction of travel: the sweep only ever walks forward,
    // so the margin for a tick that slipped backwards has to come from the anchor.
    let behind = span.saturating_mul(PREFETCH_BEHIND);
    let anchor = if price_falling {
        last_known_tick.saturating_add(behind)
    } else {
        last_known_tick.saturating_sub(behind)
    };
    sweep(dex, pool, program, anchor, tick_spacing, price_falling, PREFETCH_WIDTH)
}

/// Turn a candidate list and the subset known to be live into the three arrays the
/// instruction will name.
///
/// Public so a caller that fetched the candidate accounts itself — see
/// [`prefetch_candidates`] — can answer the same question without a second round trip.
/// `None` when there were no candidates at all, which is [`resolve`]'s error case.
#[must_use]
pub fn choose(candidates: &[(i32, Pubkey)], live: &[(i32, Pubkey)]) -> Option<Chosen> {
    if candidates.is_empty() {
        return None;
    }
    Some(pick(candidates, live))
}

/// Ask the chain which candidates exist and take the first three, nearest first.
///
/// # Errors
/// If the RPC call fails, or if the pool has no initialised arrays ahead of it at all —
/// which means there is nothing to swap into and the trade would fail on chain.
pub async fn resolve(
    rpc: &Rpc,
    dex: Dex,
    pool: &Pubkey,
    program: &Pubkey,
    tick_current: i32,
    tick_spacing: u16,
    price_falling: bool,
) -> Result<Chosen> {
    let cands = candidates(dex, pool, program, tick_current, tick_spacing, price_falling);
    if cands.is_empty() {
        bail!("no candidate tick arrays for tick {tick_current} at spacing {tick_spacing}");
    }
    let keys: Vec<Pubkey> = cands.iter().map(|(_, k)| *k).collect();
    let fetched = rpc.accounts_full(&keys).await?;

    let live: Vec<(i32, Pubkey)> = cands
        .iter()
        .zip(fetched.iter())
        .filter(|(_, acc)| acc.as_ref().is_some_and(|a| a.owner == *program))
        .map(|((s, k), _)| (*s, *k))
        .collect();

    Ok(pick(&cands, &live))
}

/// Turn the live candidates into the three the instruction will name.
///
/// Split out from [`resolve`] so the selection rule is testable without a network.
///
/// # Panics
/// Never — `bail` in `resolve` covers the empty case, and this is only reached with a
/// non-empty candidate list.
fn pick(candidates: &[(i32, Pubkey)], live: &[(i32, Pubkey)]) -> Chosen {
    let current_exists = live.first().is_some_and(|(s, _)| Some(*s) == candidates.first().map(|c| c.0));

    // Fewer than three initialised arrays is not an error: the program stops when it
    // runs out of liquidity, and repeating the last real array is what the reference
    // clients do. Repeating a *fake* one would be worse than useless, so the fallback
    // when nothing is live is the nearest candidate — the program will treat it as
    // empty and the swap will fail cleanly rather than doing something unintended.
    let fallback = live.last().or_else(|| candidates.first()).expect("candidates is non-empty");

    let mut arrays = [fallback.1; TICK_ARRAYS_PER_SWAP];
    let mut starts = Vec::with_capacity(TICK_ARRAYS_PER_SWAP);
    for (i, slot) in arrays.iter_mut().enumerate() {
        let (start, key) = live.get(i).copied().unwrap_or(*fallback);
        *slot = key;
        starts.push(start);
    }

    Chosen { arrays, starts, found: live.len(), current_exists }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(starts: &[i32]) -> Vec<(i32, Pubkey)> {
        starts.iter().map(|s| (*s, Pubkey::new_unique())).collect()
    }

    /// The property the whole prefetch rests on: whatever the tick does between the
    /// sweep that recorded it and the attempt that uses it, the window the *fresh*
    /// tick asks for is inside the window the *stale* one prefetched.
    ///
    /// If this is ever false the caller does not guess — it falls back to asking — so
    /// the cost of the property failing is a round trip, not a wrong array. But the
    /// point of the exercise is that it should almost never fail, so the range swept
    /// here is deliberately violent: a full array in each direction, on both venues,
    /// at every spacing the registry uses.
    #[test]
    fn a_tick_that_moved_is_still_inside_the_window_that_was_prefetched() {
        let pool = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        for dex in [Dex::OrcaWhirlpool, Dex::RaydiumClmm] {
            for spacing in [1u16, 2, 4, 8, 16, 60, 64, 120] {
                let span = i32::from(spacing) * per_array(dex);
                for falling in [true, false] {
                    let stale = 0i32;
                    let wide =
                        prefetch_candidates(dex, &pool, &program, stale, spacing, falling);
                    let have: std::collections::HashSet<Pubkey> =
                        wide.iter().map(|(_, k)| *k).collect();
                    // A whole array of movement in either direction, sampled at the
                    // boundaries and inside.
                    for moved in [-span, -span / 2, -1, 0, 1, span / 2, span] {
                        let real =
                            candidates(dex, &pool, &program, stale + moved, spacing, falling);
                        assert!(
                            real.iter().all(|(_, k)| have.contains(k)),
                            "{dex:?} spacing {spacing} falling {falling}: a tick that moved \
                             {moved} asks for an array the prefetch did not cover"
                        );
                    }
                }
            }
        }
    }

    /// And the prefetch must not be so wide that it stops being one round trip's worth
    /// of question. Twelve was already chosen to fit `getMultipleAccounts` alongside
    /// the pools; this stays in the same order.
    #[test]
    fn the_prefetch_window_stays_small_enough_to_ask_in_one_call() {
        let pool = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let wide = prefetch_candidates(Dex::RaydiumClmm, &pool, &program, 0, 60, false);
        assert_eq!(wide.len(), PREFETCH_WIDTH);
        // Three hops of prefetch plus their pools and the profit account still fits the
        // 100-account ceiling on getMultipleAccounts with room to spare.
        const { assert!(PREFETCH_WIDTH * 3 + 4 < 100) }
    }

    /// `choose` answers exactly what `resolve` would have, given the same knowledge —
    /// otherwise the fast path and the fallback would disagree about which arrays a
    /// swap names, which is the kind of difference that only shows up on chain.
    #[test]
    fn choosing_from_a_prefetch_matches_asking_directly() {
        let c = cands(&[0, -60, -120, -180, -240]);
        let live = vec![c[1], c[3]];
        assert_eq!(choose(&c, &live).expect("candidates exist").arrays, pick(&c, &live).arrays);
        assert!(choose(&[], &[]).is_none(), "no candidates is not a choice");
    }

    #[test]
    fn the_nearest_three_live_arrays_are_chosen_in_order() {
        let c = cands(&[0, -60, -120, -180, -240]);
        let live = vec![c[0], c[2], c[3]];
        let got = pick(&c, &live);
        assert_eq!(got.arrays.to_vec(), vec![c[0].1, c[2].1, c[3].1]);
        assert_eq!(got.starts, vec![0, -120, -180]);
        assert_eq!(got.found, 3);
        assert!(got.current_exists);
    }

    /// The case the whole module exists for: the array containing the current tick was
    /// never created, and the swap must use the ones that were.
    #[test]
    fn a_missing_current_array_is_skipped_rather_than_named() {
        let c = cands(&[61320, 61380, 61440, 61500]);
        let live = vec![c[1], c[2], c[3]];
        let got = pick(&c, &live);
        assert!(!got.current_exists, "the current array is absent and must be reported so");
        assert_eq!(got.starts, vec![61380, 61440, 61500]);
        assert!(!got.arrays.contains(&c[0].1), "the uninitialised array must not be named");
    }

    /// Fewer than three live arrays repeats the last real one, which is harmless: the
    /// program stops at the end of liquidity regardless.
    #[test]
    fn too_few_live_arrays_repeat_the_last_real_one() {
        let c = cands(&[0, -60, -120]);
        let live = vec![c[0], c[1]];
        let got = pick(&c, &live);
        assert_eq!(got.arrays.to_vec(), vec![c[0].1, c[1].1, c[1].1]);
        assert_eq!(got.found, 2);
    }

    #[test]
    fn one_live_array_fills_all_three_slots() {
        let c = cands(&[100, 160, 220]);
        let live = vec![c[1]];
        let got = pick(&c, &live);
        assert_eq!(got.arrays.to_vec(), vec![c[1].1; 3]);
        assert!(!got.current_exists);
    }

    /// No live arrays at all must still produce a well-formed instruction that fails on
    /// chain, rather than a panic or an address from somewhere else.
    #[test]
    fn no_live_arrays_falls_back_to_the_nearest_candidate() {
        let c = cands(&[0, -60, -120]);
        let got = pick(&c, &[]);
        assert_eq!(got.arrays.to_vec(), vec![c[0].1; 3]);
        assert_eq!(got.found, 0);
        assert!(!got.current_exists);
    }

    /// The sweep must walk away from the tick in the direction of the trade, and every
    /// candidate must be a distinct address.
    #[test]
    fn candidates_walk_in_the_traversal_direction_and_do_not_repeat() {
        let pool = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        for (dex, falling) in
            [(Dex::OrcaWhirlpool, true), (Dex::OrcaWhirlpool, false), (Dex::RaydiumClmm, true)]
        {
            let c = candidates(dex, &pool, &program, 0, 64, falling);
            assert_eq!(c.len(), SWEEP_WIDTH, "{dex:?} swept the wrong width");
            for w in c.windows(2) {
                if falling {
                    assert!(w[1].0 < w[0].0, "{dex:?} did not descend");
                } else {
                    assert!(w[1].0 > w[0].0, "{dex:?} did not ascend");
                }
                assert_ne!(w[0].1, w[1].1, "two candidates share an address");
            }
        }
    }

    /// Orca and Raydium must not derive the same address for the same start index, or
    /// one venue is being handed the other's arrays.
    #[test]
    fn the_two_venues_sweep_to_different_addresses() {
        let pool = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let orca = candidates(Dex::OrcaWhirlpool, &pool, &program, 0, 1, true);
        let ray = candidates(Dex::RaydiumClmm, &pool, &program, 0, 1, true);
        assert_ne!(orca[1].1, ray[1].1);
    }
}
