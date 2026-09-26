//! Raydium CPMM (CP-Swap) pool decoding — Raydium's current constant-product AMM.
//!
//! Program: `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`
//!
//! Not to be confused with [`crate::raydium_v4`], the older OpenBook-era AMM. Both are
//! constant product and both are listed as "Standard" by Raydium's API, but their
//! accounts are 637 and 752 bytes and share no layout. Feeding one to the other's
//! decoder is caught by the length check rather than producing plausible garbage.
//!
//! # Three things here can quietly cost money
//!
//! **Uncollected fees.** Like v4, the tradable reserve is the vault balance minus what
//! has accrued to somebody but not been withdrawn. Here that is **three** buckets per
//! side: `protocol_fees`, `fund_fees`, and `creator_fees`. Miss one and the reserve is
//! overstated, which overstates the output, which turns a losing trade into one that
//! looked profitable.
//!
//! The creator buckets are the trap. They were added after the original struct shipped,
//! in space that used to be reserved padding, so a decoder written from the older
//! layout reads them as zero — and is wrong only on pools that have actually accrued
//! them. On the first pool this module was tested against, the missing 6.888 SOL moved
//! the implied price by 65 bps, and the cycle search dutifully reported a standing
//! arbitrage that had been there for hours. It had not been. Nothing about the number
//! looked wrong; it took quoting the same swap through an independent router to expose
//! it.
//!
//! **Token-2022.** Unlike v4, this program accepts Token-2022 mints, which can carry
//! transfer fees and transfer hooks that skim a swap invisibly to constant-product
//! arithmetic. The pool account records each mint's token program but not the mint's
//! extensions, so a Token-2022 side is decoded and reported here, and the caller that
//! holds the mint account decides (`cb_dex::token2022::transfer_costs`). Any program
//! other than the two token programs is refused outright. Refusing Token-2022 wholesale
//! cost the most active CP-Swap pools on 2026-09-26: tokenised equities whose mints
//! carry metadata, a permanent delegate and a pause switch, but no fee and no hook.
//!
//! **The creator fee.** Pools launched through Raydium's launchpad charge a second fee
//! beside the trade fee, at the rate their config names (0.05% to 1.5% on every config
//! read on 2026-09-26), when `enable_creator_fee` is set. It comes off the input, or off
//! the output when the pool takes it only in the token being received. Ignoring it
//! overstates every quote through such a pool by up to 150 bps.
//!
//! **The status byte.** A pool can have swaps disabled while still holding liquidity
//! and looking perfectly quotable. Routing through one produces a cycle that cannot
//! execute.
//!
//! Offsets verified against the live mainnet pool
//! `Q2sPHPdUWFMg7M7wwrQKLrn619cAucfRsmhVJffodSp` on 2026-08-21.

use anyhow::{ensure, Result};
use cb_core::types::{Dex, PoolId, PoolMath, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// Exact serialised length of the CP-Swap `PoolState` account.
pub const POOL_LEN: usize = 637;
/// Minimum length of an `AmmConfig`. The account has grown by padding over time, so
/// this is a floor rather than an equality.
pub const CONFIG_MIN_LEN: usize = 44;

/// The classic SPL Token program.
pub const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// Token-2022, whose mints may carry extensions the caller must screen.
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

// Verified byte offsets into the pool account.
const OFF_AMM_CONFIG: usize = 8;
const OFF_VAULT_0: usize = 72;
const OFF_VAULT_1: usize = 104;
const OFF_MINT_0: usize = 168;
const OFF_MINT_1: usize = 200;
const OFF_PROGRAM_0: usize = 232;
const OFF_PROGRAM_1: usize = 264;
const OFF_STATUS: usize = 329;
const OFF_DECIMALS_0: usize = 331;
const OFF_DECIMALS_1: usize = 332;
const OFF_PROTOCOL_FEES_0: usize = 341;
const OFF_PROTOCOL_FEES_1: usize = 349;
const OFF_FUND_FEES_0: usize = 357;
const OFF_FUND_FEES_1: usize = 365;
// After `recent_epoch` at 381 come `creator_fee_on: u8`, `enable_creator_fee: bool`
// and six bytes of padding, then a **third** pair of fee buckets. These landed in what
// used to be reserved padding, so a decoder written against the older layout reads
// them as zero and silently overstates the reserve. See the module docs.
const OFF_CREATOR_FEE_ON: usize = 389;
const OFF_ENABLE_CREATOR_FEE: usize = 390;
const OFF_CREATOR_FEES_0: usize = 397;
/// The pool's observation account, which a swap writes its price history to.
const OFF_OBSERVATION: usize = 296;
const OFF_CREATOR_FEES_1: usize = 405;

/// Bit in `status` that disables swapping. Set means the pool will reject a trade.
const STATUS_SWAP_DISABLED: u8 = 1 << 2;

// Verified byte offsets into AmmConfig: bump, disable_create_pool, index, then the
// trade, protocol and fund rates, the pool-creation fee, two owners, and the creator rate.
const OFF_CFG_TRADE_FEE_RATE: usize = 12;
const OFF_CFG_CREATOR_FEE_RATE: usize = 108;

fn u64_at(d: &[u8], o: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[o..o + 8]);
    u64::from_le_bytes(b)
}

fn pubkey_at(d: &[u8], o: usize) -> Pubkey32 {
    let mut k = [0u8; 32];
    k.copy_from_slice(&d[o..o + 32]);
    k
}

/// The parts of a CP-Swap pool that are not its reserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpmmPool {
    /// Config account holding this pool's trade fee. Must be fetched separately.
    pub amm_config: Pubkey32,
    pub mint_0: Pubkey32,
    pub mint_1: Pubkey32,
    pub vault_0: Pubkey32,
    pub vault_1: Pubkey32,
    pub decimals_0: u8,
    pub decimals_1: u8,
    /// Accrued but unwithdrawn, and therefore **not** tradable.
    pub protocol_fees_0: u64,
    pub protocol_fees_1: u64,
    pub fund_fees_0: u64,
    pub fund_fees_1: u64,
    pub creator_fees_0: u64,
    pub creator_fees_1: u64,
    /// The account a swap records the price in; every swap names it.
    pub observation: Pubkey32,
    /// Whether this pool charges a creator fee on swaps, on top of the trade fee.
    pub enable_creator_fee: bool,
    /// Which side that fee comes from: 0 both, 1 only token 0, 2 only token 1.
    pub creator_fee_on: u8,
    /// Whether each mint is owned by Token-2022 rather than the classic program.
    pub token_2022_0: bool,
    pub token_2022_1: bool,
}

impl CpmmPool {
    /// Everything owed out of vault 0 that is not tradable reserve.
    #[must_use]
    pub fn owed_0(&self) -> u64 {
        self.protocol_fees_0.saturating_add(self.fund_fees_0).saturating_add(self.creator_fees_0)
    }

    #[must_use]
    pub fn owed_1(&self) -> u64 {
        self.protocol_fees_1.saturating_add(self.fund_fees_1).saturating_add(self.creator_fees_1)
    }
}

/// Decode the pool account.
pub fn decode(data: &[u8]) -> Result<CpmmPool> {
    ensure!(
        data.len() >= POOL_LEN,
        "raydium cpmm pool account too short: {} bytes, need {POOL_LEN}",
        data.len()
    );

    let status = data[OFF_STATUS];
    ensure!(
        status & STATUS_SWAP_DISABLED == 0,
        "cpmm pool has swaps disabled (status {status:#04x}) — it would quote and then revert"
    );

    let (spl, t22) = (bs58_expect(SPL_TOKEN_PROGRAM), bs58_expect(TOKEN_2022_PROGRAM));
    let prog_0 = pubkey_at(data, OFF_PROGRAM_0);
    let prog_1 = pubkey_at(data, OFF_PROGRAM_1);
    ensure!(
        [prog_0, prog_1].iter().all(|p| *p == spl || *p == t22),
        "cpmm pool names a token program that is neither SPL Token nor Token-2022"
    );
    let creator_fee_on = data[OFF_CREATOR_FEE_ON];
    ensure!(creator_fee_on <= 2, "cpmm pool has creator_fee_on {creator_fee_on}, which no version defines");

    let decimals_0 = data[OFF_DECIMALS_0];
    let decimals_1 = data[OFF_DECIMALS_1];
    ensure!(
        decimals_0 <= 18 && decimals_1 <= 18,
        "implausible decimals ({decimals_0}, {decimals_1}) — wrong layout or wrong account"
    );

    Ok(CpmmPool {
        amm_config: pubkey_at(data, OFF_AMM_CONFIG),
        mint_0: pubkey_at(data, OFF_MINT_0),
        mint_1: pubkey_at(data, OFF_MINT_1),
        vault_0: pubkey_at(data, OFF_VAULT_0),
        vault_1: pubkey_at(data, OFF_VAULT_1),
        decimals_0,
        decimals_1,
        protocol_fees_0: u64_at(data, OFF_PROTOCOL_FEES_0),
        protocol_fees_1: u64_at(data, OFF_PROTOCOL_FEES_1),
        fund_fees_0: u64_at(data, OFF_FUND_FEES_0),
        fund_fees_1: u64_at(data, OFF_FUND_FEES_1),
        creator_fees_0: u64_at(data, OFF_CREATOR_FEES_0),
        creator_fees_1: u64_at(data, OFF_CREATOR_FEES_1),
        observation: pubkey_at(data, OFF_OBSERVATION),
        enable_creator_fee: data[OFF_ENABLE_CREATOR_FEE] != 0,
        creator_fee_on,
        token_2022_0: prog_0 == t22,
        token_2022_1: prog_1 == t22,
    })
}

/// Read the creator fee rate, in parts per million, out of an `AmmConfig` account. It
/// applies only to a pool with `enable_creator_fee` set; see [`to_pool_state`].
pub fn decode_creator_fee_ppm(data: &[u8]) -> Result<u32> {
    ensure!(
        data.len() >= OFF_CFG_CREATOR_FEE_RATE + 8,
        "raydium cpmm config too short for a creator rate: {} bytes",
        data.len()
    );
    let fee = u64_at(data, OFF_CFG_CREATOR_FEE_RATE);
    ensure!(fee < 1_000_000, "cpmm creator fee {fee}ppm is not a fee");
    Ok(fee as u32)
}

/// Read the trade fee, in parts per million, out of an `AmmConfig` account.
pub fn decode_trade_fee_ppm(data: &[u8]) -> Result<u32> {
    ensure!(
        data.len() >= CONFIG_MIN_LEN,
        "raydium cpmm config too short: {} bytes, need at least {CONFIG_MIN_LEN}",
        data.len()
    );
    let fee = u64_at(data, OFF_CFG_TRADE_FEE_RATE);
    ensure!(fee < 1_000_000, "cpmm trade fee {fee}ppm is not a fee");
    Ok(fee as u32)
}

/// Combine the pool account, its two vault balances, and its config's two fee rates
/// into a [`PoolState`].
///
/// `creator_fee_ppm` is the config's creator rate, charged only when the pool has
/// `enable_creator_fee` set, and then from the side `creator_fee_on` names — see
/// [`PoolMath::ConstantProductExtraFee`].
///
/// Errors if a vault holds less than the fees recorded against it, which means the
/// three accounts were read at inconsistent slots and must not be quoted on.
pub fn to_pool_state(
    address: Pubkey32,
    pool: &CpmmPool,
    vault_0_amount: u64,
    vault_1_amount: u64,
    trade_fee_ppm: u32,
    creator_fee_ppm: u32,
    slot: u64,
) -> Result<PoolState> {
    ensure!(trade_fee_ppm < 1_000_000, "cpmm trade fee {trade_fee_ppm}ppm is not a fee");
    ensure!(creator_fee_ppm < 1_000_000, "cpmm creator fee {creator_fee_ppm}ppm is not a fee");

    let r0 = vault_0_amount
        .checked_sub(pool.owed_0())
        .ok_or_else(|| anyhow::anyhow!("vault 0 below fees owed — torn read across slots"))?;
    let r1 = vault_1_amount
        .checked_sub(pool.owed_1())
        .ok_or_else(|| anyhow::anyhow!("vault 1 below fees owed — torn read across slots"))?;
    ensure!(r0 > 0 && r1 > 0, "cpmm pool has an empty side after fees");

    let mut state = PoolState::constant_product(
        PoolId(address),
        Dex::RaydiumCpmm,
        pool.mint_0,
        pool.mint_1,
        u128::from(r0),
        u128::from(r1),
        trade_fee_ppm,
        slot,
    );
    if pool.enable_creator_fee && creator_fee_ppm > 0 {
        // Token 0 is A. Only-token-0 takes it from the input spending A and from the
        // output spending B; only-token-1 the reverse; both-tokens always the input.
        let (a_to_b_out, b_to_a_out) = match pool.creator_fee_on {
            1 => (false, true),
            2 => (true, false),
            _ => (false, false),
        };
        state.math = PoolMath::ConstantProductExtraFee {
            reserve_a: u128::from(r0),
            reserve_b: u128::from(r1),
            extra_fee_ppm: creator_fee_ppm,
            extra_on_output_a_to_b: a_to_b_out,
            extra_on_output_b_to_a: b_to_a_out,
        };
    }
    Ok(state)
}

/// Decode a base58 constant known to be valid. Panics only on a typo in this file.
fn bs58_expect(s: &str) -> Pubkey32 {
    let v = bs58::decode(s).into_vec().expect("constant is valid base58");
    v.as_slice().try_into().expect("constant is 32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(status: u8, prog: Pubkey32, decimals: (u8, u8), fees: [u64; 6]) -> Vec<u8> {
        let mut d = vec![0u8; POOL_LEN];
        d[OFF_AMM_CONFIG..OFF_AMM_CONFIG + 32].copy_from_slice(&[7u8; 32]);
        d[OFF_VAULT_0..OFF_VAULT_0 + 32].copy_from_slice(&[3u8; 32]);
        d[OFF_VAULT_1..OFF_VAULT_1 + 32].copy_from_slice(&[4u8; 32]);
        d[OFF_MINT_0..OFF_MINT_0 + 32].copy_from_slice(&[1u8; 32]);
        d[OFF_MINT_1..OFF_MINT_1 + 32].copy_from_slice(&[2u8; 32]);
        d[OFF_PROGRAM_0..OFF_PROGRAM_0 + 32].copy_from_slice(&prog);
        d[OFF_PROGRAM_1..OFF_PROGRAM_1 + 32].copy_from_slice(&prog);
        d[OFF_STATUS] = status;
        d[OFF_DECIMALS_0] = decimals.0;
        d[OFF_DECIMALS_1] = decimals.1;
        d[OFF_PROTOCOL_FEES_0..OFF_PROTOCOL_FEES_0 + 8].copy_from_slice(&fees[0].to_le_bytes());
        d[OFF_PROTOCOL_FEES_1..OFF_PROTOCOL_FEES_1 + 8].copy_from_slice(&fees[1].to_le_bytes());
        d[OFF_FUND_FEES_0..OFF_FUND_FEES_0 + 8].copy_from_slice(&fees[2].to_le_bytes());
        d[OFF_FUND_FEES_1..OFF_FUND_FEES_1 + 8].copy_from_slice(&fees[3].to_le_bytes());
        d[OFF_CREATOR_FEES_0..OFF_CREATOR_FEES_0 + 8].copy_from_slice(&fees[4].to_le_bytes());
        d[OFF_CREATOR_FEES_1..OFF_CREATOR_FEES_1 + 8].copy_from_slice(&fees[5].to_le_bytes());
        d
    }

    /// The real mainnet WSOL/ALNOOR pool, captured 2026-08-22.
    ///
    /// These exact numbers are the regression: read without the creator buckets, this
    /// pool prices 65 bps away from where every other venue and every router puts it.
    fn live() -> Vec<u8> {
        account(
            0,
            bs58_expect(SPL_TOKEN_PROGRAM),
            (9, 6),
            [72_586, 112_411_388, 32_273_478, 356_658_004, 6_888_081_160, 0],
        )
    }

    fn config(fee_ppm: u64) -> Vec<u8> {
        // Real config accounts are 236 bytes; the decoder only needs the first 20.
        let mut d = vec![0u8; 236];
        d[OFF_CFG_TRADE_FEE_RATE..OFF_CFG_TRADE_FEE_RATE + 8].copy_from_slice(&fee_ppm.to_le_bytes());
        d
    }

    #[test]
    fn decodes_the_real_pools_fields() {
        let p = decode(&live()).unwrap();
        assert_eq!(p.mint_0, [1u8; 32]);
        assert_eq!(p.mint_1, [2u8; 32]);
        assert_eq!(p.vault_0, [3u8; 32]);
        assert_eq!((p.decimals_0, p.decimals_1), (9, 6));
        assert_eq!(p.protocol_fees_0, 72_586);
        assert_eq!(p.fund_fees_1, 356_658_004);
        assert_eq!(p.creator_fees_0, 6_888_081_160, "the bucket the old layout missed");
        assert_eq!(p.creator_fees_1, 0);
    }

    #[test]
    fn config_yields_the_fee_raydiums_api_reports() {
        assert_eq!(decode_trade_fee_ppm(&config(2500)).unwrap(), 2500, "25 bp, the default tier");
        assert_eq!(decode_trade_fee_ppm(&config(100)).unwrap(), 100);
    }

    /// The subtraction that makes this decoder worth writing. All three fee buckets
    /// come out, not just the two that existed when the struct was first published.
    #[test]
    fn reserves_exclude_all_three_fee_buckets() {
        let p = decode(&live()).unwrap();
        let (v0, v1) = (10_000_000_000u64, 20_000_000_000u64);
        let ps = to_pool_state([9u8; 32], &p, v0, v1, 2500, 0, 42).unwrap();

        assert_eq!(ps.reserve_a(), u128::from(v0 - 72_586 - 32_273_478 - 6_888_081_160));
        assert_eq!(ps.reserve_b(), u128::from(v1 - 112_411_388 - 356_658_004));
        assert!(ps.reserve_a() < u128::from(v0), "must be strictly less than the raw vault");
        assert_eq!(ps.dex, Dex::RaydiumCpmm);
        assert_eq!(ps.fee_ppm, 2500);
        assert_eq!(ps.slot, 42);
    }

    /// The regression, stated as a price rather than as a field offset.
    ///
    /// Real vault balances for WSOL/ALNOOR, captured with the pool account above. An
    /// independent router quoted this pool's mid at 7.848 in both directions. Reading
    /// only the protocol and fund buckets puts it at 7.797 — a 65 bps error, in the
    /// direction that *invents* an arbitrage rather than hiding one.
    #[test]
    fn the_price_matches_what_an_independent_router_quotes() {
        let p = decode(&live()).unwrap();
        let (vault_0, vault_1) = (1_326_162_157_059u64, 10_353_978_949_932u64);
        let ps = to_pool_state([9u8; 32], &p, vault_0, vault_1, 2500, 0, 1).unwrap();

        let price = ps.spot_price().expect("pool must price");
        assert!((price - 7.848).abs() < 0.002, "priced at {price}, but it trades at 7.848");

        // And the specific wrong answer this test exists to prevent.
        let buggy =
            (vault_1 - 112_411_388 - 356_658_004) as f64 / (vault_0 - 72_586 - 32_273_478) as f64;
        assert!((buggy - 7.807).abs() < 0.002, "sanity: the old bug produced {buggy}");
        let error_bps = (price / buggy - 1.0) * 10_000.0;
        assert!(error_bps > 45.0, "the bug was worth {error_bps} bps of phantom edge");
    }

    #[test]
    fn a_vault_below_its_own_fees_is_a_torn_read_not_a_pool() {
        let p = decode(&live()).unwrap();
        let err = to_pool_state([9u8; 32], &p, 1, 20_000_000_000, 2500, 0, 1).unwrap_err().to_string();
        assert!(err.contains("torn read"), "unexpected error: {err}");
    }

    /// A pool with swaps switched off still holds liquidity and still looks quotable.
    /// Routing through one produces a cycle that reverts.
    #[test]
    fn pools_with_swaps_disabled_are_refused() {
        let d = account(STATUS_SWAP_DISABLED, bs58_expect(SPL_TOKEN_PROGRAM), (9, 6), [0; 6]);
        let err = decode(&d).unwrap_err().to_string();
        assert!(err.contains("swaps disabled"), "unexpected error: {err}");

        // Other status bits are not our problem — only the swap one.
        assert!(decode(&account(0b11, bs58_expect(SPL_TOKEN_PROGRAM), (9, 6), [0; 6])).is_ok());
    }

    /// This program, unlike Raydium v4, accepts Token-2022 mints. The pool cannot say
    /// whether one charges for transfers, so it is decoded and flagged for the caller
    /// that holds the mint; any other owning program is refused here.
    #[test]
    fn token_2022_mints_are_flagged_and_other_programs_refused() {
        let t22 = bs58_expect(TOKEN_2022_PROGRAM);
        let p = decode(&account(0, t22, (9, 6), [0; 6])).unwrap();
        assert!(p.token_2022_0 && p.token_2022_1);
        let classic = decode(&live()).unwrap();
        assert!(!classic.token_2022_0 && !classic.token_2022_1);
        let err = decode(&account(0, [5u8; 32], (9, 6), [0; 6])).unwrap_err().to_string();
        assert!(err.contains("neither SPL Token nor Token-2022"), "unexpected error: {err}");
    }

    fn with_creator_fee(on: u8) -> CpmmPool {
        let mut d = live();
        d[OFF_ENABLE_CREATOR_FEE] = 1;
        d[OFF_CREATOR_FEE_ON] = on;
        decode(&d).unwrap()
    }

    /// The creator fee comes off the input when the pool takes it in both tokens, and
    /// then it simply adds to the trade fee.
    #[test]
    fn a_creator_fee_on_both_tokens_adds_to_the_input_fee() {
        let (v0, v1) = (1_000_000_000_000u64, 2_000_000_000_000u64);
        let base = to_pool_state([9u8; 32], &decode(&live()).unwrap(), v0, v1, 2500, 10_000, 1).unwrap();
        let both = to_pool_state([9u8; 32], &with_creator_fee(0), v0, v1, 2500, 10_000, 1).unwrap();
        let x = 1_000_000_000u128;
        let plain = base.leg_for_input(&[1u8; 32]).unwrap().quote(x).unwrap();
        let charged = both.leg_for_input(&[1u8; 32]).unwrap().quote(x).unwrap();
        // 1% more fee on the input is very nearly 1% less out.
        let loss_bps = (1.0 - charged as f64 / plain as f64) * 1e4;
        assert!((100.0..101.0).contains(&loss_bps), "lost {loss_bps} bps");
        let back = both.leg_for_input(&[2u8; 32]).unwrap().quote(x).unwrap();
        let back_plain = base.leg_for_input(&[2u8; 32]).unwrap().quote(x).unwrap();
        assert!(back < back_plain, "both directions pay it");
    }

    /// Taken only in token 0, the fee comes off the input spending token 0 and off the
    /// output buying it — and the output version is the smaller quote, never the larger.
    #[test]
    fn a_creator_fee_on_one_token_follows_that_token() {
        let (v0, v1) = (1_000_000_000_000u64, 2_000_000_000_000u64);
        let only0 = to_pool_state([9u8; 32], &with_creator_fee(1), v0, v1, 2500, 10_000, 1).unwrap();
        let only1 = to_pool_state([9u8; 32], &with_creator_fee(2), v0, v1, 2500, 10_000, 1).unwrap();
        assert!(matches!(
            only0.math,
            PoolMath::ConstantProductExtraFee { extra_on_output_a_to_b: false, extra_on_output_b_to_a: true, .. }
        ));
        assert!(matches!(
            only1.math,
            PoolMath::ConstantProductExtraFee { extra_on_output_a_to_b: true, extra_on_output_b_to_a: false, .. }
        ));
        // The exact program arithmetic, spending token 1 into a token-0-only pool: the
        // trade fee off the input, then the creator fee off what the curve pays.
        let x = 5_000_000_000u128;
        let (r_in, r_out) = (u128::from(v1 - 112_411_388 - 356_658_004), u128::from(v0 - 72_586 - 32_273_478 - 6_888_081_160));
        let less = x - (x * 2500).div_ceil(1_000_000);
        let swapped = r_out * less / (r_in + less);
        let program = swapped - (swapped * 10_000).div_ceil(1_000_000);
        let quoted = only0.leg_for_input(&[2u8; 32]).unwrap().quote(x).unwrap();
        assert!(quoted <= program, "quoted {quoted} above the program's {program}");
        assert!(program - quoted < program / 50_000, "but within 0.2 bps: {quoted} vs {program}");
    }

    #[test]
    fn a_disabled_creator_fee_costs_nothing_whatever_the_config_says() {
        let p = decode(&live()).unwrap();
        assert!(!p.enable_creator_fee);
        let s = to_pool_state([9u8; 32], &p, 10_000_000_000, 20_000_000_000, 2500, 10_000, 1).unwrap();
        assert!(matches!(s.math, PoolMath::ConstantProduct { .. }));
    }

    #[test]
    fn the_config_yields_its_creator_rate() {
        let mut d = config(2500);
        d[OFF_CFG_CREATOR_FEE_RATE..OFF_CFG_CREATOR_FEE_RATE + 8].copy_from_slice(&10_000u64.to_le_bytes());
        assert_eq!(decode_creator_fee_ppm(&d).unwrap(), 10_000);
        assert!(decode_creator_fee_ppm(&d[..100]).is_err());
    }

    /// Raydium's two constant-product programs have different layouts and different
    /// account sizes. The length check is what stops one being read as the other.
    #[test]
    fn a_raydium_v4_account_is_not_mistaken_for_a_cpmm_one() {
        assert!(decode(&vec![0u8; crate::raydium_v4::AMM_INFO_LEN]).is_err(), "752 bytes is v4");
        assert!(decode(&[]).is_err());
        assert!(decode(&vec![0u8; POOL_LEN - 1]).is_err());
        assert!(decode_trade_fee_ppm(&[]).is_err());
    }

    #[test]
    fn a_hundred_percent_fee_is_rejected_rather_than_underflowing_gamma() {
        assert!(decode_trade_fee_ppm(&config(1_000_000)).is_err());
        let p = decode(&live()).unwrap();
        assert!(to_pool_state([9u8; 32], &p, 10_000_000_000, 20_000_000_000, 1_000_000, 0, 1).is_err());
        assert!(to_pool_state([9u8; 32], &p, 10_000_000_000, 20_000_000_000, 2500, 1_000_000, 1).is_err());
    }
}
