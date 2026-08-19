//! Two-pool cycle detection.
//!
//! Triggered by an account update: when pool P changes, only cycles involving P can
//! have changed. That keeps work proportional to the update rate rather than to the
//! square of the pool count, which is what makes broad coverage affordable.

use crate::store::PoolStore;
use cb_core::amm::{cycle_profit, optimal_input, CycleReserves};
use cb_core::types::{Opportunity, PoolState, Pubkey32};

/// Find profitable `base -> quote -> base` cycles that route through `updated`.
///
/// `min_profit` is in base-mint base units and is compared against **gross** profit;
/// fee and tip deduction happens in the evaluator, not here.
#[must_use]
pub fn find_two_pool_cycles(
    store: &PoolStore,
    base_mint: &Pubkey32,
    updated: &PoolState,
    min_profit: u128,
) -> Vec<Opportunity> {
    let mut out = Vec::new();

    // The updated pool must trade the base mint for it to start a cycle.
    let Some(quote_mint) = updated.other_mint(base_mint) else {
        return out;
    };

    for other in store.pools_trading(&quote_mint) {
        // A pool cannot arbitrage against itself.
        if other.id == updated.id {
            continue;
        }
        // The counterparty pool must return us to the base mint.
        if other.other_mint(&quote_mint) != Some(*base_mint) {
            continue;
        }

        // Leg 1: spend base in `updated`, receive quote.
        let Some(leg1) = updated.reserves_for_input(base_mint) else {
            continue;
        };
        // Leg 2: spend quote in `other`, receive base.
        let Some(leg2) = other.reserves_for_input(&quote_mint) else {
            continue;
        };

        let reserves = CycleReserves {
            a_in: leg1.r_in,
            a_out: leg1.r_out,
            b_in: leg2.r_in,
            b_out: leg2.r_out,
            fee_a_bps: updated.fee_bps,
            fee_b_bps: other.fee_bps,
        };

        let Some(amount_in) = optimal_input(&reserves) else {
            continue;
        };
        let Some(gross_profit) = cycle_profit(&reserves, amount_in) else {
            continue;
        };

        if gross_profit == 0 || gross_profit < min_profit {
            continue;
        }

        out.push(Opportunity {
            pool_buy: updated.id,
            pool_sell: other.id,
            base_mint: *base_mint,
            quote_mint,
            amount_in,
            gross_profit,
            // The cycle is only as fresh as its staler leg.
            slot: updated.slot.min(other.slot),
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PoolStore;
    use cb_core::types::{Dex, PoolId, PoolState};

    const BASE: [u8; 32] = [10; 32]; // e.g. WSOL
    const QUOTE: [u8; 32] = [20; 32]; // the intermediate token

    fn pool(id: u8, ra: u128, rb: u128) -> PoolState {
        PoolState {
            id: PoolId([id; 32]),
            dex: Dex::PumpSwap,
            mint_a: BASE,
            mint_b: QUOTE,
            reserve_a: ra,
            reserve_b: rb,
            fee_bps: 25,
            slot: 1,
        }
    }

    #[test]
    fn finds_a_cycle_between_two_dislocated_pools() {
        let s = PoolStore::new();
        // Pool 1 prices QUOTE cheap; pool 2 prices it dear. The round trip profits.
        let p1 = pool(1, 1_000_000, 1_000_000);
        let p2 = pool(2, 1_300_000, 1_000_000);
        s.upsert(p1);
        s.upsert(p2);

        let found = find_two_pool_cycles(&s, &BASE, &p1, 0);
        assert!(!found.is_empty(), "a 30% dislocation must yield an opportunity");
        let o = &found[0];
        assert_eq!(o.base_mint, BASE);
        assert_eq!(o.quote_mint, QUOTE);
        assert!(o.amount_in > 0);
        assert!(o.gross_profit > 0);
    }

    #[test]
    fn identical_pools_yield_nothing() {
        let s = PoolStore::new();
        let p1 = pool(1, 1_000_000, 1_000_000);
        s.upsert(p1);
        s.upsert(pool(2, 1_000_000, 1_000_000));
        assert!(find_two_pool_cycles(&s, &BASE, &p1, 0).is_empty());
    }

    #[test]
    fn min_profit_threshold_filters_marginal_cycles() {
        let s = PoolStore::new();
        let p1 = pool(1, 1_000_000, 1_000_000);
        let p2 = pool(2, 1_300_000, 1_000_000);
        s.upsert(p1);
        s.upsert(p2);
        let permissive = find_two_pool_cycles(&s, &BASE, &p1, 0);
        let strict = find_two_pool_cycles(&s, &BASE, &p1, u128::MAX);
        assert!(!permissive.is_empty());
        assert!(strict.is_empty(), "an impossible threshold must filter everything");
    }

    #[test]
    fn a_pool_is_never_arbitraged_against_itself() {
        let s = PoolStore::new();
        let p1 = pool(1, 1_000_000, 1_300_000);
        s.upsert(p1);
        assert!(
            find_two_pool_cycles(&s, &BASE, &p1, 0).is_empty(),
            "self-cycle is not an arb"
        );
    }

    #[test]
    fn ignores_pools_that_do_not_close_the_cycle() {
        // A pool trading QUOTE against something other than BASE cannot return us home.
        let s = PoolStore::new();
        let p1 = pool(1, 1_000_000, 1_000_000);
        s.upsert(p1);
        s.upsert(PoolState {
            id: PoolId([2; 32]),
            dex: Dex::PumpSwap,
            mint_a: QUOTE,
            mint_b: [77; 32], // not BASE
            reserve_a: 1_000_000,
            reserve_b: 5_000_000,
            fee_bps: 25,
            slot: 1,
        });
        assert!(find_two_pool_cycles(&s, &BASE, &p1, 0).is_empty());
    }

    #[test]
    fn opportunity_slot_is_the_staler_of_the_two_legs() {
        // Freshness of a cycle is bounded by its oldest input, not its newest.
        let s = PoolStore::new();
        let mut p1 = pool(1, 1_000_000, 1_000_000);
        p1.slot = 500;
        let mut p2 = pool(2, 1_300_000, 1_000_000);
        p2.slot = 300;
        s.upsert(p1);
        s.upsert(p2);
        let found = find_two_pool_cycles(&s, &BASE, &p1, 0);
        assert_eq!(found[0].slot, 300);
    }
}
