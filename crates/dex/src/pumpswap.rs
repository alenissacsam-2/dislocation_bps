//! PumpSwap (Pump AMM) pool decoding.
//!
//! Program: `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`
//!
//! Constant-product. The `Pool` account is Anchor-serialised: an 8-byte discriminator
//! followed by the struct. Reserves live in separate SPL token accounts and are passed
//! in by the caller, which keeps this a pure, network-free function.

use anyhow::{ensure, Result};
use cb_core::types::{Dex, PoolId, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

/// PumpSwap's swap fee in parts per million (25 bp).
pub const FEE_PPM: u32 = 2500;

// Byte offsets into the Pool account, after the 8-byte Anchor discriminator.
const OFF_DISCRIMINATOR: usize = 8;
const OFF_BASE_MINT: usize = OFF_DISCRIMINATOR + 1 + 2 + 32; // bump + index + creator
const OFF_QUOTE_MINT: usize = OFF_BASE_MINT + 32;
const MIN_LEN: usize = OFF_QUOTE_MINT + 32;

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey32 {
    let mut k = [0u8; 32];
    k.copy_from_slice(&data[offset..offset + 32]);
    k
}

/// Decode a PumpSwap pool account into a [`PoolState`].
///
/// `reserve_a` / `reserve_b` are the balances of the base and quote vault token
/// accounts respectively, supplied by the caller.
pub fn decode(
    address: Pubkey32,
    data: &[u8],
    reserve_a: u128,
    reserve_b: u128,
    slot: u64,
) -> Result<PoolState> {
    ensure!(
        data.len() >= MIN_LEN,
        "pumpswap pool account too short: {} bytes, need at least {MIN_LEN}",
        data.len()
    );
    Ok(PoolState::constant_product(
        PoolId(address),
        Dex::PumpSwap,
        read_pubkey(data, OFF_BASE_MINT),
        read_pubkey(data, OFF_QUOTE_MINT),
        reserve_a,
        reserve_b,
        FEE_PPM,
        slot,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal synthetic PumpSwap `Pool` account: 8-byte Anchor discriminator, then
    /// pool_bump(1) + index(2) + creator(32) + base_mint(32) + quote_mint(32).
    fn synthetic_pool_account() -> Vec<u8> {
        let mut v = vec![0u8; 8]; // discriminator
        v.push(255); // pool_bump
        v.extend_from_slice(&7u16.to_le_bytes()); // index
        v.extend_from_slice(&[9u8; 32]); // creator
        v.extend_from_slice(&[11u8; 32]); // base_mint
        v.extend_from_slice(&[22u8; 32]); // quote_mint
        v
    }

    #[test]
    fn decodes_mints_from_account_bytes() {
        let data = synthetic_pool_account();
        let p = decode([1u8; 32], &data, 5_000, 9_000, 99).unwrap();
        assert_eq!(p.mint_a, [11u8; 32]);
        assert_eq!(p.mint_b, [22u8; 32]);
        assert_eq!(p.reserve_a(), 5_000);
        assert_eq!(p.reserve_b(), 9_000);
        assert_eq!(p.dex, Dex::PumpSwap);
        assert_eq!(p.fee_ppm, FEE_PPM);
        assert_eq!(p.slot, 99);
    }

    #[test]
    fn rejects_truncated_account_data() {
        let short = vec![0u8; 20];
        assert!(
            decode([1u8; 32], &short, 1, 1, 0).is_err(),
            "must not read past the buffer"
        );
    }

    #[test]
    fn rejects_empty_data() {
        assert!(decode([1u8; 32], &[], 1, 1, 0).is_err());
    }

    #[test]
    fn accepts_data_longer_than_the_minimum() {
        // Real accounts carry trailing fields we don't parse; extra bytes are fine.
        let mut data = synthetic_pool_account();
        data.extend_from_slice(&[0u8; 256]);
        assert!(decode([1u8; 32], &data, 1, 1, 0).is_ok());
    }
}
