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
    let per_array = match dex {
        Dex::OrcaWhirlpool => ORCA_TICKS_PER_ARRAY,
        _ => RAYDIUM_TICKS_PER_ARRAY,
    };
    tick_array_sweep(tick_current, tick_spacing, per_array, price_falling, SWEEP_WIDTH)
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
