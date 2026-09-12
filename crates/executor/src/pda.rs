//! Program-derived addresses: the accounts a swap needs that are not stored anywhere,
//! only computed.
//!
//! # The one piece of arithmetic worth reading twice
//!
//! A concentrated-liquidity pool stores its ticks in fixed-size arrays, and the address
//! of an array is derived from the tick index it *starts* at. Finding that start is a
//! floor division — and the pools that matter most sit at negative ticks, because a
//! tick is a logarithm of price and every pool priced below 1.0 in its own units is
//! negative. `-5 / 3` is `-1` in Rust and the floor is `-2`, so truncating division
//! picks the array one span too high for exactly the pools we care about, and does it
//! silently: the address is a real PDA, it just holds the wrong ticks.
//!
//! [`start_tick_index`] uses `div_euclid`, and the tests below pin the negative cases
//! specifically, because a positive-only test suite passes either way.

use crate::encode::{pk, programs};
use solana_sdk::pubkey::Pubkey;

/// Ticks per array in an Orca Whirlpool.
pub const ORCA_TICKS_PER_ARRAY: i32 = 88;
/// Ticks per array in a Raydium CLMM pool.
pub const RAYDIUM_TICKS_PER_ARRAY: i32 = 60;

/// How many tick arrays a swap instruction is given to traverse.
pub const TICK_ARRAYS_PER_SWAP: usize = 3;

/// The first tick index of the array containing `tick`.
///
/// Floor division, so this is correct on both sides of zero. See the module docs for
/// why that is not a pedantic distinction.
#[must_use]
pub fn start_tick_index(tick: i32, tick_spacing: u16, ticks_per_array: i32) -> i32 {
    let span = i32::from(tick_spacing) * ticks_per_array;
    tick.div_euclid(span) * span
}

/// The sequence of tick arrays a swap will traverse, in the order the program expects.
///
/// A swap that spends token A moves the price **down**, so it walks into arrays with
/// lower start indices; spending token B walks up. Handing the program the sequence in
/// the wrong direction gives it arrays it will never reach, and the swap fails once it
/// exhausts the one array that was right.
///
/// Indices that would run past the last real array are clamped to repeat it, which is
/// what the reference clients do — a repeated array is harmless because the program
/// stops when it runs out of liquidity anyway.
///
/// # The bound is on the array, not on the tick
///
/// A start index is allowed to sit *outside* `[MIN_TICK, MAX_TICK]`, and for pools near
/// either extreme it must: the array holding `MIN_TICK` begins at the multiple of the
/// span below it, which is below `MIN_TICK` by construction. Clamping the start index
/// to the tick range instead of to the array range therefore rejects the one array that
/// is actually correct at the edges. The valid range is derived here rather than
/// assumed.
#[must_use]
pub fn tick_array_starts(
    tick_current: i32,
    tick_spacing: u16,
    ticks_per_array: i32,
    price_falling: bool,
) -> [i32; TICK_ARRAYS_PER_SWAP] {
    let span = i32::from(tick_spacing) * ticks_per_array;
    let first = start_tick_index(tick_current, tick_spacing, ticks_per_array);
    let step = if price_falling { -span } else { span };

    // The first and last arrays that contain any representable tick.
    let lowest = start_tick_index(cb_core::clmm::MIN_TICK, tick_spacing, ticks_per_array);
    let highest = start_tick_index(cb_core::clmm::MAX_TICK, tick_spacing, ticks_per_array);

    let mut out = [first; TICK_ARRAYS_PER_SWAP];
    let mut cursor = first;
    for slot in out.iter_mut().skip(1) {
        let next = cursor.saturating_add(step);
        if (lowest..=highest).contains(&next) {
            cursor = next;
        }
        *slot = cursor;
    }
    out
}

/// Candidate array start indices walking away from the current tick, nearest first.
///
/// # Why a sweep and not just the next three
///
/// A tick array stores the *boundaries* of positions, not the liquidity between them. A
/// position spanning ticks 61000 to 62000 writes into the arrays holding 61000 and
/// 62000 and touches nothing in between — so the array containing the current price can
/// legitimately have never been created, while the pool is deep and trading normally.
///
/// This was measured rather than assumed. For a live Raydium pool at tick 61364 with
/// spacing 1, the arrays at 61260, 61380 and 61440 all exist and the one containing the
/// tick, 61320, does not. Naming three consecutive arrays from the current tick would
/// hand the program two real arrays and one that does not exist.
///
/// So the caller sweeps, asks the chain which of these exist, and passes the ones that
/// do. That is a round trip the encoder cannot make on its own, which is why the
/// addresses are an input to [`crate::venue::SwapContext`] rather than derived inside
/// it — making the choice visible at the call site instead of buried in an encoder.
#[must_use]
pub fn tick_array_sweep(
    tick_current: i32,
    tick_spacing: u16,
    ticks_per_array: i32,
    price_falling: bool,
    how_many: usize,
) -> Vec<i32> {
    let span = i32::from(tick_spacing) * ticks_per_array;
    if span == 0 || how_many == 0 {
        return Vec::new();
    }
    let step = if price_falling { -span } else { span };
    let lowest = start_tick_index(cb_core::clmm::MIN_TICK, tick_spacing, ticks_per_array);
    let highest = start_tick_index(cb_core::clmm::MAX_TICK, tick_spacing, ticks_per_array);

    let mut out = Vec::with_capacity(how_many);
    let mut cursor = start_tick_index(tick_current, tick_spacing, ticks_per_array);
    for _ in 0..how_many {
        if !(lowest..=highest).contains(&cursor) {
            break;
        }
        out.push(cursor);
        cursor = cursor.saturating_add(step);
    }
    out
}

/// The associated token account for `owner` and `mint` under `token_program`.
///
/// The token program is a seed, so a Token-2022 mint has a *different* ATA from the
/// one the classic program would derive. Passing the wrong one produces an address
/// that exists, belongs to nobody, and cannot be created.
#[must_use]
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &pk(programs::ASSOCIATED_TOKEN),
    )
    .0
}

/// Orca's tick array PDA. The start index is seeded as its **decimal string**, not its
/// bytes — Orca and Raydium differ here, and the two are not interchangeable.
#[must_use]
pub fn orca_tick_array(whirlpool: &Pubkey, start_tick: i32, program: &Pubkey) -> Pubkey {
    let s = start_tick.to_string();
    Pubkey::find_program_address(&[b"tick_array", whirlpool.as_ref(), s.as_bytes()], program).0
}

/// Orca's per-pool oracle account.
#[must_use]
pub fn orca_oracle(whirlpool: &Pubkey, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", whirlpool.as_ref()], program).0
}

/// Raydium CLMM's tick array PDA. The start index is seeded as four **big-endian**
/// bytes, which is the opposite of how the same number is stored inside the account.
#[must_use]
pub fn raydium_tick_array(pool: &Pubkey, start_index: i32, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), &start_index.to_be_bytes()],
        program,
    )
    .0
}

/// Raydium CLMM's bitmap extension, which covers ticks outside the pool account's own
/// inline bitmap.
#[must_use]
pub fn raydium_bitmap_extension(pool: &Pubkey, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool_tick_array_bitmap_extension", pool.as_ref()], program).0
}

/// Meteora DLMM's bin array PDA.
///
/// The index is seeded as eight **little-endian** bytes of an `i64`, which is also how it
/// is stored inside the account — unlike Raydium's tick arrays, whose seed is big-endian
/// while the stored field is not. Two venues, two conventions, and the only way to be
/// sure is a test against an address the chain already has, which
/// `the_bin_array_address_is_the_one_the_chain_holds` is.
#[must_use]
pub fn meteora_bin_array(lb_pair: &Pubkey, index: i64, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bin_array", lb_pair.as_ref(), &index.to_le_bytes()], program).0
}

/// The `event_authority` PDA an Anchor program with events requires on every
/// instruction that emits one.
///
/// Constant per program, so it could be pasted — and is derived instead for the same
/// reason discriminators are: the seed is checkable by eye and a base58 string is not.
#[must_use]
pub fn anchor_event_authority(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program).0
}

/// How many bin arrays a DLMM swap instruction is given to traverse.
///
/// Three, matching the tick-array count on the concentrated venues, which is what lets
/// one `[Pubkey; 3]` on [`crate::venue::SwapContext`] serve both shapes. A DLMM array
/// covers seventy bins, so three of them span 210 bin steps — 2.1% of price on a
/// one-basis-point pool, and proportionally more on a coarser one. No trade this
/// instrument can fund comes close to needing the third.
pub const BIN_ARRAYS_PER_SWAP: usize = TICK_ARRAYS_PER_SWAP;

/// The bin arrays a DLMM swap will walk, in the order the program expects.
///
/// Spending token X moves the price **down** into lower bin ids and therefore lower array
/// indices; spending token Y walks up. Indices that would leave the pool's own bin range
/// are clamped to repeat the last real one, which is harmless because the program stops
/// when it runs out of liquidity anyway.
#[must_use]
pub fn meteora_bin_array_indices(active_id: i32, price_falling: bool) -> [i64; BIN_ARRAYS_PER_SWAP] {
    let first = cb_dex::meteora_dlmm::bin_array_index(active_id);
    let step: i64 = if price_falling { -1 } else { 1 };
    let mut out = [first; BIN_ARRAYS_PER_SWAP];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = first + step * i64::try_from(i).unwrap_or(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case the module docs are about. Truncating division would give 0 here.
    #[test]
    fn a_negative_tick_floors_rather_than_truncating() {
        // spacing 1, 88 per array -> span 88. Tick -5 lives in the array starting -88.
        assert_eq!(start_tick_index(-5, 1, 88), -88);
        assert_eq!(start_tick_index(-88, 1, 88), -88);
        assert_eq!(start_tick_index(-89, 1, 88), -176);
        // And the positive side, which truncation would also get right.
        assert_eq!(start_tick_index(5, 1, 88), 0);
        assert_eq!(start_tick_index(88, 1, 88), 88);
    }

    #[test]
    fn the_start_index_is_always_a_multiple_of_the_span_and_never_above_the_tick() {
        for spacing in [1u16, 2, 4, 8, 16, 64, 128] {
            let span = i32::from(spacing) * ORCA_TICKS_PER_ARRAY;
            for tick in [-443_000, -100_000, -5000, -1, 0, 1, 5000, 100_000, 443_000] {
                let s = start_tick_index(tick, spacing, ORCA_TICKS_PER_ARRAY);
                assert_eq!(s % span, 0, "start {s} is not a multiple of {span}");
                assert!(s <= tick, "start {s} is above tick {tick}");
                assert!(tick - s < span, "tick {tick} is more than one span past {s}");
            }
        }
    }

    #[test]
    fn arrays_descend_when_the_price_falls_and_ascend_when_it_rises() {
        let down = tick_array_starts(0, 8, ORCA_TICKS_PER_ARRAY, true);
        let span = 8 * ORCA_TICKS_PER_ARRAY;
        assert_eq!(down, [0, -span, -2 * span]);

        let up = tick_array_starts(0, 8, ORCA_TICKS_PER_ARRAY, false);
        assert_eq!(up, [0, span, 2 * span]);
    }

    /// Walking off the end must repeat the last real array, not wrap and not step past
    /// it. The bound is the array range, which is wider than the tick range — see the
    /// doc comment on `tick_array_starts` for why conflating the two is the bug here.
    #[test]
    fn arrays_clamp_at_the_last_real_array_not_at_the_last_tick() {
        for spacing in [1u16, 8, 64, 128] {
            let lowest = start_tick_index(cb_core::clmm::MIN_TICK, spacing, ORCA_TICKS_PER_ARRAY);
            let highest = start_tick_index(cb_core::clmm::MAX_TICK, spacing, ORCA_TICKS_PER_ARRAY);

            let up = tick_array_starts(cb_core::clmm::MAX_TICK - 1, spacing, ORCA_TICKS_PER_ARRAY, false);
            for w in up.windows(2) {
                assert!(w[1] >= w[0], "ascending sequence went backwards: {up:?}");
            }

            let down = tick_array_starts(cb_core::clmm::MIN_TICK + 1, spacing, ORCA_TICKS_PER_ARRAY, true);
            for w in down.windows(2) {
                assert!(w[1] <= w[0], "descending sequence went forwards: {down:?}");
            }

            for t in up.iter().chain(down.iter()) {
                assert!(
                    (lowest..=highest).contains(t),
                    "start {t} is outside the array range [{lowest}, {highest}] at spacing {spacing}"
                );
            }
        }
    }

    /// The specific case that a tick-range clamp gets wrong: the array holding the very
    /// lowest tick begins below it, and that array is the correct one.
    #[test]
    fn the_array_holding_the_lowest_tick_starts_below_it() {
        let lowest = start_tick_index(cb_core::clmm::MIN_TICK, 128, ORCA_TICKS_PER_ARRAY);
        assert!(
            lowest < cb_core::clmm::MIN_TICK,
            "expected the containing array to start below MIN_TICK, got {lowest}"
        );
        let down = tick_array_starts(cb_core::clmm::MIN_TICK + 1, 128, ORCA_TICKS_PER_ARRAY, true);
        assert_eq!(down[0], lowest, "the pool's own array must be the first one named");
    }

    /// The two venues seed the same conceptual number differently. If these ever agree
    /// for a non-zero index, one of the two derivations has been changed to match the
    /// other and both pools will be handed the wrong arrays.
    #[test]
    fn orca_and_raydium_tick_arrays_are_not_the_same_address() {
        let pool = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        assert_ne!(
            orca_tick_array(&pool, 1408, &prog),
            raydium_tick_array(&pool, 1408, &prog)
        );
    }

    /// A Token-2022 mint's ATA differs from the classic one. Getting this wrong sends
    /// funds to an address that cannot be created.
    #[test]
    fn the_token_program_changes_the_associated_address() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let classic = associated_token_address(&owner, &mint, &pk(programs::SPL_TOKEN));
        let t22 = associated_token_address(&owner, &mint, &pk(programs::SPL_TOKEN_2022));
        assert_ne!(classic, t22);
    }

    #[test]
    fn derivations_are_deterministic_and_distinct_per_seed() {
        let pool = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        assert_eq!(orca_oracle(&pool, &prog), orca_oracle(&pool, &prog));
        assert_ne!(orca_oracle(&pool, &prog), orca_tick_array(&pool, 0, &prog));
        assert_ne!(
            raydium_tick_array(&pool, 0, &prog),
            raydium_tick_array(&pool, 60, &prog)
        );
        assert_ne!(raydium_bitmap_extension(&pool, &prog), orca_oracle(&pool, &prog));
    }

    /// A PDA is off-curve by construction. If one of these were on-curve it would be a
    /// plain address that somebody could hold the key to.
    #[test]
    fn derived_addresses_are_off_curve() {
        let pool = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        assert!(!orca_oracle(&pool, &prog).is_on_curve());
        assert!(!orca_tick_array(&pool, -176, &prog).is_on_curve());
        assert!(!raydium_tick_array(&pool, -180, &prog).is_on_curve());
        assert!(!associated_token_address(&pool, &pool, &pk(programs::SPL_TOKEN)).is_on_curve());
    }
    /// The derivation checked against an address the chain already holds.
    ///
    /// `DxyNRLdPkPsaV73w6qe9Ytau8siAnzBXfTFaaJD8UegD` is the live bin array at index
    /// −327 of the Meteora DLMM SOL/USDC pool, found by asking the program for every
    /// account whose stored `lb_pair` is that pool and reading the index out of each.
    /// Deriving the same address from the seeds is what proves the seed layout — the
    /// endianness in particular, which differs from Raydium's tick arrays and cannot be
    /// checked any other way.
    #[test]
    fn the_bin_array_address_is_the_one_the_chain_holds() {
        let pool = pk("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR");
        let program = pk(cb_dex::meteora_dlmm::PROGRAM_ID);
        assert_eq!(
            meteora_bin_array(&pool, -327, &program),
            pk("DxyNRLdPkPsaV73w6qe9Ytau8siAnzBXfTFaaJD8UegD")
        );
        // And a neighbour must not collide with it, which a big-endian seed would make
        // look plausible while being a different account entirely.
        assert_ne!(
            meteora_bin_array(&pool, -326, &program),
            meteora_bin_array(&pool, -327, &program)
        );
    }

    /// Every `swap2` call on the program passes the same event authority, so it is a
    /// constant — and this is the check that the derivation reproduces it rather than
    /// that somebody transcribed it correctly.
    #[test]
    fn the_event_authority_is_the_one_every_real_call_passes() {
        assert_eq!(
            anchor_event_authority(&pk(cb_dex::meteora_dlmm::PROGRAM_ID)),
            pk("D1ZN9Wj1fRSUQfCjhvnu1hqDMT7hzjzBBpi12nVniYD6")
        );
    }

    /// The walk direction, which is the thing that reverses a swap if it is backwards.
    /// Spending token X moves the price down, so into lower array indices.
    #[test]
    fn bin_arrays_walk_the_way_the_price_moves() {
        let falling = meteora_bin_array_indices(-22849, true);
        let rising = meteora_bin_array_indices(-22849, false);
        assert_eq!(falling, [-327, -328, -329]);
        assert_eq!(rising, [-327, -326, -325]);
        // The first is always the array holding the active bin, whichever way it goes.
        assert_eq!(falling[0], rising[0]);
        assert_eq!(falling[0], cb_dex::meteora_dlmm::bin_array_index(-22849));
    }
}
