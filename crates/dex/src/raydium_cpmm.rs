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
//! the protocol has accrued but not withdrawn — here in *two* buckets per side,
//! `protocol_fees` and `fund_fees`. Forgetting either overstates the reserve, which
//! overstates the output, which turns a losing trade into one that looked profitable.
//!
//! **Token-2022.** Unlike v4, this program accepts Token-2022 mints, which can carry
//! transfer fees and transfer hooks that skim a swap invisibly to constant-product
//! arithmetic. The pool account records each mint's token program, so we check it and
//! decline rather than trusting the registry to have filtered correctly.
//!
//! **The status byte.** A pool can have swaps disabled while still holding liquidity
//! and looking perfectly quotable. Routing through one produces a cycle that cannot
//! execute.
//!
//! Offsets verified against the live mainnet pool
//! `Q2sPHPdUWFMg7M7wwrQKLrn619cAucfRsmhVJffodSp` on 2026-08-21.

use anyhow::{ensure, Result};
use cb_core::types::{Dex, PoolId, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// Exact serialised length of the CP-Swap `PoolState` account.
pub const POOL_LEN: usize = 637;
/// Minimum length of an `AmmConfig`. The account has grown by padding over time, so
/// this is a floor rather than an equality.
pub const CONFIG_MIN_LEN: usize = 44;

/// The classic SPL Token program. Anything else may skim transfers.
pub const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

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

/// Bit in `status` that disables swapping. Set means the pool will reject a trade.
const STATUS_SWAP_DISABLED: u8 = 1 << 2;

// Verified byte offsets into AmmConfig.
const OFF_CFG_TRADE_FEE_RATE: usize = 12;

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
}

impl CpmmPool {
    /// Everything owed out of vault 0 that is not tradable reserve.
    #[must_use]
    pub fn owed_0(&self) -> u64 {
        self.protocol_fees_0.saturating_add(self.fund_fees_0)
    }

    #[must_use]
    pub fn owed_1(&self) -> u64 {
        self.protocol_fees_1.saturating_add(self.fund_fees_1)
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

    let spl = bs58_expect(SPL_TOKEN_PROGRAM);
    let prog_0 = pubkey_at(data, OFF_PROGRAM_0);
    let prog_1 = pubkey_at(data, OFF_PROGRAM_1);
    ensure!(
        prog_0 == spl && prog_1 == spl,
        "cpmm pool holds a non-classic token mint — a transfer fee or hook could skim \
         the swap in a way constant-product arithmetic cannot see"
    );

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
    })
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

/// Combine the pool account, its two vault balances, and its fee into a [`PoolState`].
///
/// Errors if a vault holds less than the fees recorded against it, which means the
/// three accounts were read at inconsistent slots and must not be quoted on.
pub fn to_pool_state(
    address: Pubkey32,
    pool: &CpmmPool,
    vault_0_amount: u64,
    vault_1_amount: u64,
    trade_fee_ppm: u32,
    slot: u64,
) -> Result<PoolState> {
    ensure!(trade_fee_ppm < 1_000_000, "cpmm trade fee {trade_fee_ppm}ppm is not a fee");

    let r0 = vault_0_amount
        .checked_sub(pool.owed_0())
        .ok_or_else(|| anyhow::anyhow!("vault 0 below fees owed — torn read across slots"))?;
    let r1 = vault_1_amount
        .checked_sub(pool.owed_1())
        .ok_or_else(|| anyhow::anyhow!("vault 1 below fees owed — torn read across slots"))?;
    ensure!(r0 > 0 && r1 > 0, "cpmm pool has an empty side after fees");

    Ok(PoolState::constant_product(
        PoolId(address),
        Dex::RaydiumCpmm,
        pool.mint_0,
        pool.mint_1,
        u128::from(r0),
        u128::from(r1),
        trade_fee_ppm,
        slot,
    ))
}

/// Decode a base58 constant known to be valid. Panics only on a typo in this file.
fn bs58_expect(s: &str) -> Pubkey32 {
    let v = bs58::decode(s).into_vec().expect("constant is valid base58");
    v.as_slice().try_into().expect("constant is 32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(status: u8, prog: Pubkey32, decimals: (u8, u8), fees: [u64; 4]) -> Vec<u8> {
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
        d
    }

    /// The real mainnet pool's field values, captured 2026-08-21.
    fn live() -> Vec<u8> {
        account(0, bs58_expect(SPL_TOKEN_PROGRAM), (9, 6), [2_595_021, 34_437_398, 55_276_153, 66_403_360])
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
        assert_eq!(p.protocol_fees_0, 2_595_021);
        assert_eq!(p.fund_fees_1, 66_403_360);
    }

    #[test]
    fn config_yields_the_fee_raydiums_api_reports() {
        assert_eq!(decode_trade_fee_ppm(&config(2500)).unwrap(), 2500, "25 bp, the default tier");
        assert_eq!(decode_trade_fee_ppm(&config(100)).unwrap(), 100);
    }

    /// The subtraction that makes this decoder worth writing. Both fee buckets come
    /// out, not just the one that shares a name with Raydium v4's field.
    #[test]
    fn reserves_exclude_both_fee_buckets() {
        let p = decode(&live()).unwrap();
        let (v0, v1) = (10_000_000_000u64, 20_000_000_000u64);
        let ps = to_pool_state([9u8; 32], &p, v0, v1, 2500, 42).unwrap();

        assert_eq!(ps.reserve_a(), u128::from(v0 - 2_595_021 - 55_276_153));
        assert_eq!(ps.reserve_b(), u128::from(v1 - 34_437_398 - 66_403_360));
        assert!(ps.reserve_a() < u128::from(v0), "must be strictly less than the raw vault");
        assert_eq!(ps.dex, Dex::RaydiumCpmm);
        assert_eq!(ps.fee_ppm, 2500);
        assert_eq!(ps.slot, 42);
    }

    #[test]
    fn a_vault_below_its_own_fees_is_a_torn_read_not_a_pool() {
        let p = decode(&live()).unwrap();
        let err = to_pool_state([9u8; 32], &p, 1, 20_000_000_000, 2500, 1).unwrap_err().to_string();
        assert!(err.contains("torn read"), "unexpected error: {err}");
    }

    /// A pool with swaps switched off still holds liquidity and still looks quotable.
    /// Routing through one produces a cycle that reverts.
    #[test]
    fn pools_with_swaps_disabled_are_refused() {
        let d = account(STATUS_SWAP_DISABLED, bs58_expect(SPL_TOKEN_PROGRAM), (9, 6), [0; 4]);
        let err = decode(&d).unwrap_err().to_string();
        assert!(err.contains("swaps disabled"), "unexpected error: {err}");

        // Other status bits are not our problem — only the swap one.
        assert!(decode(&account(0b11, bs58_expect(SPL_TOKEN_PROGRAM), (9, 6), [0; 4])).is_ok());
    }

    /// This program, unlike Raydium v4, accepts Token-2022 mints. Those can carry a
    /// transfer fee that constant-product arithmetic cannot see.
    #[test]
    fn token_2022_mints_are_refused() {
        let t22 = bs58_expect("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
        let err = decode(&account(0, t22, (9, 6), [0; 4])).unwrap_err().to_string();
        assert!(err.contains("non-classic token mint"), "unexpected error: {err}");
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
        assert!(to_pool_state([9u8; 32], &p, 10_000_000_000, 20_000_000_000, 1_000_000, 1).is_err());
    }
}
