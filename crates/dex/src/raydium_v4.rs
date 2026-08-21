//! Raydium AMM v4 pool decoding.
//!
//! Program: `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`
//!
//! # The trap
//!
//! `AmmInfo` **does not contain the reserves.** The true reserves are the balances of
//! two separate SPL token vault accounts, *minus* the protocol's uncollected fees
//! (`base_need_take_pnl` / `quote_need_take_pnl`). Reading reserves from the pool
//! account, or forgetting the PnL subtraction, produces quotes that look plausible and
//! are wrong — the worst kind of bug in a trading system, because it loses money
//! quietly rather than failing loudly.
//!
//! So decoding a Raydium pool requires **three** accounts, and a live feed must
//! subscribe to all three.
//!
//! Layout is `#[repr(C)]`, `Pack`-serialised — no Anchor discriminator. Offsets below
//! were verified against the live SOL/USDC pool
//! (`58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2`) on 2026-08-20.

use anyhow::{ensure, Result};
use cb_core::types::{Dex, PoolId, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

/// Exact serialised length of `AmmInfo`.
pub const AMM_INFO_LEN: usize = 752;

// Verified byte offsets into AmmInfo.
const OFF_BASE_DECIMALS: usize = 32;
const OFF_QUOTE_DECIMALS: usize = 40;
const OFF_SWAP_FEE_NUM: usize = 176;
const OFF_SWAP_FEE_DEN: usize = 184;
const OFF_BASE_NEED_TAKE_PNL: usize = 192;
const OFF_QUOTE_NEED_TAKE_PNL: usize = 200;
const OFF_BASE_VAULT: usize = 336;
const OFF_QUOTE_VAULT: usize = 368;
const OFF_BASE_MINT: usize = 400;
const OFF_QUOTE_MINT: usize = 432;

/// SPL token account: `mint(32) | owner(32) | amount(u64)`. Amount starts at 64.
const SPL_AMOUNT_OFFSET: usize = 64;
/// SPL token accounts are a fixed 165 bytes.
pub const SPL_TOKEN_ACCOUNT_LEN: usize = 165;

fn u64_at(data: &[u8], offset: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[offset..offset + 8]);
    u64::from_le_bytes(b)
}

fn pubkey_at(data: &[u8], offset: usize) -> Pubkey32 {
    let mut k = [0u8; 32];
    k.copy_from_slice(&data[offset..offset + 32]);
    k
}

/// The parts of `AmmInfo` we need, decoded from the pool account alone.
///
/// This is deliberately separate from [`PoolState`]: it tells the feed *which vault
/// accounts to go and watch*, which it cannot know until the pool is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmmInfo {
    pub base_mint: Pubkey32,
    pub quote_mint: Pubkey32,
    pub base_vault: Pubkey32,
    pub quote_vault: Pubkey32,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    /// Protocol fees accrued in the base vault but not yet withdrawn. **Not** part of
    /// the tradable reserve.
    pub base_need_take_pnl: u64,
    pub quote_need_take_pnl: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
}

impl AmmInfo {
    /// Swap fee in parts per million, rounded up so we never *under*-estimate cost.
    ///
    /// Rounding down here would make marginal cycles look profitable when they are
    /// not; rounding up only makes us skip trades that were barely worth taking.
    /// Parts per million rather than basis points because the concentrated-liquidity
    /// venues quote at 1 bp, and a unit that cannot tell 1 bp from 2 bp cannot tell a
    /// profitable route from a losing one.
    #[must_use]
    pub fn fee_ppm(&self) -> u32 {
        const DEFAULT: u32 = 2500; // Raydium's standard 25 bp.
        if self.swap_fee_denominator == 0 {
            return DEFAULT; // A zero denominator is corrupt data, not a free pool.
        }
        let num = u128::from(self.swap_fee_numerator) * 1_000_000;
        let den = u128::from(self.swap_fee_denominator);
        u32::try_from(num.div_ceil(den)).unwrap_or(DEFAULT)
    }
}

/// Decode the pool account.
pub fn decode_amm_info(data: &[u8]) -> Result<AmmInfo> {
    ensure!(
        data.len() >= AMM_INFO_LEN,
        "raydium v4 pool account too short: {} bytes, need {AMM_INFO_LEN}",
        data.len()
    );
    let base_decimals = u64_at(data, OFF_BASE_DECIMALS);
    let quote_decimals = u64_at(data, OFF_QUOTE_DECIMALS);
    ensure!(
        base_decimals <= 18 && quote_decimals <= 18,
        "implausible decimals ({base_decimals}, {quote_decimals}) — wrong layout or wrong account"
    );

    Ok(AmmInfo {
        base_mint: pubkey_at(data, OFF_BASE_MINT),
        quote_mint: pubkey_at(data, OFF_QUOTE_MINT),
        base_vault: pubkey_at(data, OFF_BASE_VAULT),
        quote_vault: pubkey_at(data, OFF_QUOTE_VAULT),
        base_decimals: base_decimals as u8,
        quote_decimals: quote_decimals as u8,
        base_need_take_pnl: u64_at(data, OFF_BASE_NEED_TAKE_PNL),
        quote_need_take_pnl: u64_at(data, OFF_QUOTE_NEED_TAKE_PNL),
        swap_fee_numerator: u64_at(data, OFF_SWAP_FEE_NUM),
        swap_fee_denominator: u64_at(data, OFF_SWAP_FEE_DEN),
    })
}

/// Read the token balance out of an SPL token account.
pub fn decode_token_amount(data: &[u8]) -> Result<u64> {
    ensure!(
        data.len() >= SPL_AMOUNT_OFFSET + 8,
        "spl token account too short: {} bytes",
        data.len()
    );
    Ok(u64_at(data, SPL_AMOUNT_OFFSET))
}

/// Combine the pool account and its two vault balances into a [`PoolState`].
///
/// `base_vault_amount` / `quote_vault_amount` are the raw SPL balances; the
/// uncollected-fee subtraction happens here so callers cannot forget it.
///
/// Returns an error if the vault holds less than the recorded uncollected fees, which
/// means the three accounts were read at inconsistent slots and must not be quoted on.
pub fn to_pool_state(
    address: Pubkey32,
    info: &AmmInfo,
    base_vault_amount: u64,
    quote_vault_amount: u64,
    slot: u64,
) -> Result<PoolState> {
    let base = base_vault_amount
        .checked_sub(info.base_need_take_pnl)
        .ok_or_else(|| anyhow::anyhow!("base vault below uncollected fees — torn read across slots"))?;
    let quote = quote_vault_amount
        .checked_sub(info.quote_need_take_pnl)
        .ok_or_else(|| anyhow::anyhow!("quote vault below uncollected fees — torn read across slots"))?;

    Ok(PoolState::constant_product(
        PoolId(address),
        Dex::RaydiumAmmV4,
        info.base_mint,
        info.quote_mint,
        u128::from(base),
        u128::from(quote),
        info.fee_ppm(),
        slot,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

    fn to_b58(bytes: &[u8]) -> String {
        let mut digits: Vec<u8> = vec![0];
        for &b in bytes {
            let mut carry = u32::from(b);
            for d in digits.iter_mut() {
                carry += u32::from(*d) << 8;
                *d = (carry % 58) as u8;
                carry /= 58;
            }
            while carry > 0 {
                digits.push((carry % 58) as u8);
                carry /= 58;
            }
        }
        let leading = bytes.iter().take_while(|&&b| b == 0).count();
        let mut s = String::new();
        for _ in 0..leading {
            s.push('1');
        }
        for &d in digits.iter().rev() {
            s.push(B58[d as usize] as char);
        }
        s
    }

    /// Real mainnet bytes for the Raydium v4 SOL/USDC pool, captured 2026-08-20.
    fn real_pool_account() -> Vec<u8> {
        let b64 = include_str!("../tests/fixtures/raydium_v4_sol_usdc.b64").trim();
        // Minimal base64 decoder — avoids a dependency for one fixture.
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
        for &c in b64.as_bytes() {
            let v = table(c);
            if v < 0 {
                continue;
            }
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((acc >> bits) & 0xFF) as u8);
            }
        }
        out
    }

    #[test]
    fn fixture_is_the_expected_size() {
        assert_eq!(real_pool_account().len(), AMM_INFO_LEN);
    }

    #[test]
    fn decodes_the_real_sol_usdc_pool() {
        let info = decode_amm_info(&real_pool_account()).unwrap();
        assert_eq!(to_b58(&info.base_mint), "So11111111111111111111111111111111111111112");
        assert_eq!(to_b58(&info.quote_mint), "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_eq!(to_b58(&info.base_vault), "DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz");
        assert_eq!(to_b58(&info.quote_vault), "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz");
        assert_eq!(info.base_decimals, 9, "WSOL");
        assert_eq!(info.quote_decimals, 6, "USDC");
        assert_eq!(info.fee_ppm(), 2500, "25 bp, carried as parts per million");
    }

    /// The PnL subtraction is the whole point of this module.
    #[test]
    fn reserves_exclude_uncollected_protocol_fees() {
        let info = decode_amm_info(&real_pool_account()).unwrap();
        assert!(info.base_need_take_pnl > 0, "fixture should have accrued fees");

        // Raw vault balances observed at capture time.
        let (base_raw, quote_raw) = (66_641_707_408_048u64, 5_934_861_275_575u64);
        let p = to_pool_state([7u8; 32], &info, base_raw, quote_raw, 42).unwrap();

        assert_eq!(p.reserve_a(), u128::from(base_raw - info.base_need_take_pnl));
        assert_eq!(p.reserve_b(), u128::from(quote_raw - info.quote_need_take_pnl));
        assert!(p.reserve_a() < u128::from(base_raw), "must be strictly less than raw");
    }

    #[test]
    fn implied_price_matches_the_market() {
        let info = decode_amm_info(&real_pool_account()).unwrap();
        let p = to_pool_state([7u8; 32], &info, 66_641_707_408_048, 5_934_861_275_575, 1).unwrap();
        // Adjust for 9 vs 6 decimals.
        let sol = p.reserve_a() as f64 / 1e9;
        let usdc = p.reserve_b() as f64 / 1e6;
        let price = usdc / sol;
        assert!(
            (50.0..200.0).contains(&price),
            "implied SOL price {price} is outside any plausible range — layout is wrong"
        );
    }

    #[test]
    fn torn_reads_across_slots_are_rejected() {
        // A vault balance below the recorded uncollected fees means the pool account
        // and the vault were sampled at different slots. Quoting on that is unsafe.
        let info = decode_amm_info(&real_pool_account()).unwrap();
        let err = to_pool_state([7u8; 32], &info, 1, 5_934_861_275_575, 1).unwrap_err();
        assert!(err.to_string().contains("torn read"), "got: {err}");
    }

    #[test]
    fn rejects_short_and_wrong_accounts() {
        assert!(decode_amm_info(&[]).is_err());
        assert!(decode_amm_info(&vec![0u8; AMM_INFO_LEN - 1]).is_err());
        // All-zero data has plausible length but decimals of 0 pass; the mints would
        // be all-zero, which the caller filters. Confirm no panic on garbage.
        let _ = decode_amm_info(&vec![0u8; AMM_INFO_LEN]);
        // Implausible decimals must be caught.
        let mut bad = vec![0u8; AMM_INFO_LEN];
        bad[OFF_BASE_DECIMALS] = 99;
        assert!(decode_amm_info(&bad).is_err());
    }

    #[test]
    fn token_amount_decodes_at_the_right_offset() {
        let mut acct = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
        acct[SPL_AMOUNT_OFFSET..SPL_AMOUNT_OFFSET + 8].copy_from_slice(&12_345_678u64.to_le_bytes());
        assert_eq!(decode_token_amount(&acct).unwrap(), 12_345_678);
        assert!(decode_token_amount(&[0u8; 8]).is_err());
    }

    #[test]
    fn fee_ppm_rounds_up_never_down() {
        let mk = |num, den| AmmInfo {
            base_mint: [0; 32], quote_mint: [0; 32], base_vault: [0; 32], quote_vault: [0; 32],
            base_decimals: 9, quote_decimals: 6,
            base_need_take_pnl: 0, quote_need_take_pnl: 0,
            swap_fee_numerator: num, swap_fee_denominator: den,
        };
        assert_eq!(mk(25, 10_000).fee_ppm(), 2500, "25/10000 is 25 bp = 2500 ppm");
        assert_eq!(mk(3, 1_000).fee_ppm(), 3000, "30 bp");
        // 25.1 bp is exactly representable in ppm — the unit change bought this.
        assert_eq!(mk(251, 100_000).fee_ppm(), 2510);
        // A rate that genuinely does not divide must round up, never down.
        assert_eq!(mk(1, 3).fee_ppm(), 333_334);
        // Corrupt denominator falls back rather than dividing by zero.
        assert_eq!(mk(25, 0).fee_ppm(), 2500);
    }
}
