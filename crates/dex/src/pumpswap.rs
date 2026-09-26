//! PumpSwap (Pump AMM) decoding and pricing.
//!
//! Program: `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`
//!
//! # Why this venue
//!
//! An on-chain census of 360 blocks on 2026-09-26 found PumpSwap in more closed-loop
//! arbitrages than any other venue, and in most of the ones whose state had stood for a
//! slot or more before they were taken — the only kind a sender a slot behind can win.
//!
//! # The curve and where the fee comes from
//!
//! Constant product over two SPL vaults. The fee is taken on the **quote** side in
//! both directions: out of what a sale of the base token receives, and out of what a
//! purchase spends. See [`PoolMath::QuoteSideFee`].
//!
//! # The fee is not a constant
//!
//! It used to be 25 bp. It is now computed by a separate fee program from tiers held in
//! its `FeeConfig` account: for a pool created by pump.fun's migration, by the coin's
//! market cap in quote units (30 to 125 bp as read on 2026-09-26), and a flat rate for
//! any other pool. Each tier splits into an LP, a protocol and a creator share; the
//! swapper pays their sum. Everything here is read from those accounts, never assumed.
//!
//! # The quote reserve has a virtual part
//!
//! A pump.fun pool prices against its quote vault **plus** a virtual quote reserve held
//! in the pool account (a `u64` at byte 245, newer than the published IDL): about
//! 17.58 SOL on every pump pool read on 2026-09-26, zero on other pools. Purchases get
//! fewer tokens and sales more SOL than the vault alone would give. Without it the
//! quote was 6-9% off on a live pool; with it, a simulated sale matched the program's
//! own event to the lamport and a purchase to a thousandth of a basis point. The market
//! cap that picks the fee tier is measured on the same effective reserve — the one
//! data point the vault-only market cap put in the wrong tier moved into the right one.
//!
//! Layouts are the on-chain Anchor IDLs of both programs, read from chain on 2026-09-26.

use anyhow::{ensure, Result};
use cb_core::types::{Dex, PoolId, PoolMath, PoolState, Pubkey32};

pub const PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
/// The fee program PumpSwap asks for its fees.
pub const FEE_PROGRAM_ID: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
/// pump.fun's bonding-curve program. A PumpSwap pool it migrated a coin into is created
/// by its `["pool-authority", base_mint]` PDA, which is how a "pump pool" is told apart.
pub const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
/// PumpSwap's one `GlobalConfig`.
pub const GLOBAL_CONFIG: &str = "ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw";
/// The `FeeConfig` PumpSwap's swaps name (a PDA of the fee program, seeded by PumpSwap).
pub const FEE_CONFIG: &str = "5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx";

// Pool, after the 8-byte discriminator.
const OFF_INDEX: usize = 9;
const OFF_CREATOR: usize = 11;
const OFF_BASE_MINT: usize = 43;
const OFF_QUOTE_MINT: usize = 75;
const OFF_BASE_VAULT: usize = 139;
const OFF_QUOTE_VAULT: usize = 171;
const OFF_COIN_CREATOR: usize = 211;
const OFF_MAYHEM: usize = 243;
const OFF_CASHBACK: usize = 244;
const OFF_VIRTUAL_QUOTE: usize = 245;
/// The pool account ends here or later.
pub const POOL_MIN_LEN: usize = 245;

// GlobalConfig.
const OFF_G_LP_FEE: usize = 40;
const OFF_G_PROTOCOL_FEE: usize = 48;
const OFF_G_DISABLE: usize = 56;
const OFF_G_PROTOCOL_RECIPIENTS: usize = 57;
const OFF_G_CREATOR_FEE: usize = 313;
const OFF_G_BUYBACK_RECIPIENTS: usize = 643;
const OFF_G_BUYBACK_BPS: usize = 899;
const GLOBAL_MIN_LEN: usize = 907;

// FeeConfig: discriminator, bump, admin, then the flat fees and two tier vectors.
const OFF_F_FLAT: usize = 41;
const FEES_LEN: usize = 24;
const TIER_LEN: usize = 16 + FEES_LEN;

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().expect("8 bytes"))
}

fn pubkey_at(d: &[u8], o: usize) -> Pubkey32 {
    d[o..o + 32].try_into().expect("32 bytes")
}

/// A decoded PumpSwap pool account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpPool {
    pub index: u16,
    /// Who created the pool: pump.fun's migration authority for a canonical pool.
    pub creator: Pubkey32,
    pub base_mint: Pubkey32,
    pub quote_mint: Pubkey32,
    pub base_vault: Pubkey32,
    pub quote_vault: Pubkey32,
    /// Receives the creator share of the fee. All zeros when there is none.
    pub coin_creator: Pubkey32,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    /// Added to the quote vault's balance wherever the program prices. Zero when the
    /// account predates the field.
    pub virtual_quote: u64,
}

impl PumpPool {
    /// The quote reserve the program prices against: the vault's balance plus the
    /// pool's virtual quote.
    #[must_use]
    pub fn effective_quote(&self, quote_vault_balance: u64) -> u64 {
        quote_vault_balance.saturating_add(self.virtual_quote)
    }
}

/// Decode a PumpSwap `Pool` account.
///
/// # Errors
/// If the account is too short to be one.
pub fn decode_pool(data: &[u8]) -> Result<PumpPool> {
    ensure!(
        data.len() >= POOL_MIN_LEN,
        "pumpswap pool account too short: {} bytes, need at least {POOL_MIN_LEN}",
        data.len()
    );
    Ok(PumpPool {
        index: u16::from_le_bytes([data[OFF_INDEX], data[OFF_INDEX + 1]]),
        creator: pubkey_at(data, OFF_CREATOR),
        base_mint: pubkey_at(data, OFF_BASE_MINT),
        quote_mint: pubkey_at(data, OFF_QUOTE_MINT),
        base_vault: pubkey_at(data, OFF_BASE_VAULT),
        quote_vault: pubkey_at(data, OFF_QUOTE_VAULT),
        coin_creator: pubkey_at(data, OFF_COIN_CREATOR),
        is_mayhem_mode: data[OFF_MAYHEM] != 0,
        is_cashback_coin: data[OFF_CASHBACK] != 0,
        virtual_quote: if data.len() >= OFF_VIRTUAL_QUOTE + 8 { u64_at(data, OFF_VIRTUAL_QUOTE) } else { 0 },
    })
}

/// The parts of PumpSwap's `GlobalConfig` a swap needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalConfig {
    pub lp_fee_bps: u64,
    pub protocol_fee_bps: u64,
    pub coin_creator_fee_bps: u64,
    /// Non-zero bits switch operations off.
    pub disable_flags: u8,
    /// Any one of these may be named as the protocol fee recipient.
    pub protocol_fee_recipients: [Pubkey32; 8],
    /// Any one of these may be named as the buyback fee recipient, which a swap now
    /// passes as a remaining account.
    pub buyback_fee_recipients: [Pubkey32; 8],
    pub buyback_bps: u64,
}

/// Decode PumpSwap's `GlobalConfig`.
///
/// # Errors
/// If the account is too short to be one.
pub fn decode_global_config(data: &[u8]) -> Result<GlobalConfig> {
    ensure!(
        data.len() >= GLOBAL_MIN_LEN,
        "pumpswap global config too short: {} bytes, need {GLOBAL_MIN_LEN}",
        data.len()
    );
    let keys = |start: usize| -> [Pubkey32; 8] {
        let mut out = [[0u8; 32]; 8];
        for (i, k) in out.iter_mut().enumerate() {
            *k = pubkey_at(data, start + 32 * i);
        }
        out
    };
    Ok(GlobalConfig {
        lp_fee_bps: u64_at(data, OFF_G_LP_FEE),
        protocol_fee_bps: u64_at(data, OFF_G_PROTOCOL_FEE),
        coin_creator_fee_bps: u64_at(data, OFF_G_CREATOR_FEE),
        disable_flags: data[OFF_G_DISABLE],
        protocol_fee_recipients: keys(OFF_G_PROTOCOL_RECIPIENTS),
        buyback_fee_recipients: keys(OFF_G_BUYBACK_RECIPIENTS),
        buyback_bps: u64_at(data, OFF_G_BUYBACK_BPS),
    })
}

/// One fee split, in basis points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fees {
    pub lp_bps: u64,
    pub protocol_bps: u64,
    pub creator_bps: u64,
}

impl Fees {
    /// What the swapper pays, in basis points.
    #[must_use]
    pub fn total_bps(&self) -> u64 {
        self.lp_bps + self.protocol_bps + self.creator_bps
    }
}

/// The fee program's `FeeConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeConfig {
    /// For a pool that did not come from pump.fun's migration.
    pub flat: Fees,
    /// `(market cap threshold in quote base units, fees)`, ascending.
    pub tiers: Vec<(u128, Fees)>,
    /// The same for a coin quoted in a stablecoin.
    pub stable_tiers: Vec<(u128, Fees)>,
}

fn fees_at(d: &[u8], o: usize) -> Fees {
    Fees { lp_bps: u64_at(d, o), protocol_bps: u64_at(d, o + 8), creator_bps: u64_at(d, o + 16) }
}

/// Decode the fee program's `FeeConfig`.
///
/// # Errors
/// If a vector runs past the end of the account.
pub fn decode_fee_config(data: &[u8]) -> Result<FeeConfig> {
    ensure!(data.len() >= OFF_F_FLAT + FEES_LEN + 4, "fee config too short: {} bytes", data.len());
    let flat = fees_at(data, OFF_F_FLAT);
    let mut o = OFF_F_FLAT + FEES_LEN;
    let read_tiers = |o: &mut usize| -> Result<Vec<(u128, Fees)>> {
        ensure!(data.len() >= *o + 4, "fee config ends inside a tier count");
        let n = u32::from_le_bytes(data[*o..*o + 4].try_into().expect("4 bytes")) as usize;
        *o += 4;
        ensure!(data.len() >= *o + n * TIER_LEN, "fee config ends inside its {n} tiers");
        let mut tiers = Vec::with_capacity(n);
        for _ in 0..n {
            let threshold = u128::from_le_bytes(data[*o..*o + 16].try_into().expect("16 bytes"));
            tiers.push((threshold, fees_at(data, *o + 16)));
            *o += TIER_LEN;
        }
        Ok(tiers)
    };
    let tiers = read_tiers(&mut o)?;
    let stable_tiers = read_tiers(&mut o)?;
    Ok(FeeConfig { flat, tiers, stable_tiers })
}

impl FeeConfig {
    /// The fees a swap on this pool pays.
    ///
    /// A pump.fun pool pays the tier its market cap has reached; any other pool pays the
    /// flat rate. Market cap below every threshold falls into the first tier.
    #[must_use]
    pub fn fees(&self, is_pump_pool: bool, market_cap: u128, stable_quote: bool) -> Fees {
        if !is_pump_pool {
            return self.flat;
        }
        let tiers = if stable_quote { &self.stable_tiers } else { &self.tiers };
        tiers
            .iter()
            .take_while(|(threshold, _)| *threshold <= market_cap)
            .last()
            .or_else(|| tiers.first())
            .map_or(self.flat, |(_, f)| *f)
    }
}

/// A coin's market cap in quote base units: its supply at the pool's price.
#[must_use]
pub fn market_cap(base_supply: u64, base_reserve: u64, quote_reserve: u64) -> u128 {
    if base_reserve == 0 {
        return 0;
    }
    u128::from(base_supply) * u128::from(quote_reserve) / u128::from(base_reserve)
}

/// A PumpSwap pool as a [`PoolState`], charging `fee_bps` on the quote side.
///
/// `quote_reserve` is the quote **vault's** balance; the pool's virtual quote is added
/// here, so a caller cannot forget it.
///
/// # Errors
/// If the fee is not a fee.
pub fn to_pool_state(
    address: Pubkey32,
    pool: &PumpPool,
    base_reserve: u64,
    quote_reserve: u64,
    fee_bps: u64,
    slot: u64,
) -> Result<PoolState> {
    ensure!(fee_bps < 10_000, "pumpswap fee {fee_bps} bps is not a fee");
    // Mint A is the base token; the quote side is B.
    Ok(PoolState {
        id: PoolId(address),
        dex: Dex::PumpSwap,
        mint_a: pool.base_mint,
        mint_b: pool.quote_mint,
        math: PoolMath::QuoteSideFee {
            reserve_a: u128::from(base_reserve),
            reserve_b: u128::from(pool.effective_quote(quote_reserve)),
            quote_is_b: true,
        },
        fee_ppm: u32::try_from(fee_bps * 100).expect("under a million"),
        slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_account() -> Vec<u8> {
        let mut v = vec![0u8; POOL_MIN_LEN + 11];
        v[OFF_INDEX..OFF_INDEX + 2].copy_from_slice(&7u16.to_le_bytes());
        v[OFF_CREATOR..OFF_CREATOR + 32].copy_from_slice(&[9u8; 32]);
        v[OFF_BASE_MINT..OFF_BASE_MINT + 32].copy_from_slice(&[11u8; 32]);
        v[OFF_QUOTE_MINT..OFF_QUOTE_MINT + 32].copy_from_slice(&[22u8; 32]);
        v[OFF_BASE_VAULT..OFF_BASE_VAULT + 32].copy_from_slice(&[33u8; 32]);
        v[OFF_QUOTE_VAULT..OFF_QUOTE_VAULT + 32].copy_from_slice(&[44u8; 32]);
        v[OFF_COIN_CREATOR..OFF_COIN_CREATOR + 32].copy_from_slice(&[55u8; 32]);
        v[OFF_CASHBACK] = 1;
        v
    }

    #[test]
    fn decodes_the_pool_fields_a_swap_needs() {
        let p = decode_pool(&pool_account()).unwrap();
        assert_eq!(p.index, 7);
        assert_eq!((p.base_mint, p.quote_mint), ([11u8; 32], [22u8; 32]));
        assert_eq!((p.base_vault, p.quote_vault), ([33u8; 32], [44u8; 32]));
        assert_eq!(p.coin_creator, [55u8; 32]);
        assert!(p.is_cashback_coin && !p.is_mayhem_mode);
        assert!(decode_pool(&pool_account()[..POOL_MIN_LEN - 1]).is_err());
    }

    fn fee_config() -> Vec<u8> {
        let mut v = vec![0u8; OFF_F_FLAT];
        for x in [25u64, 5, 0] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        let tiers: [(u128, [u64; 3]); 3] =
            [(0, [2, 93, 30]), (420_000_000_000, [20, 5, 95]), (1_470_000_000_000, [20, 5, 90])];
        for _ in 0..2 {
            v.extend_from_slice(&(tiers.len() as u32).to_le_bytes());
            for (t, f) in tiers {
                v.extend_from_slice(&t.to_le_bytes());
                for x in f {
                    v.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        v
    }

    #[test]
    fn a_pump_pool_pays_the_tier_its_market_cap_reached() {
        let c = decode_fee_config(&fee_config()).unwrap();
        assert_eq!(c.flat.total_bps(), 30);
        assert_eq!(c.tiers.len(), 3);
        assert_eq!(c.fees(true, 0, false).total_bps(), 125, "the first tier starts at zero");
        assert_eq!(c.fees(true, 419_999_999_999, false).total_bps(), 125);
        assert_eq!(c.fees(true, 420_000_000_000, false).total_bps(), 120);
        assert_eq!(c.fees(true, 10u128.pow(15), false).total_bps(), 115);
        assert_eq!(c.fees(false, 10u128.pow(15), false).total_bps(), 30, "any other pool is flat");
    }

    #[test]
    fn market_cap_is_supply_at_the_pool_price() {
        // A billion tokens (6 decimals), 800M in the pool against 80 SOL: 100 SOL cap.
        assert_eq!(market_cap(1_000_000_000_000_000, 800_000_000_000_000, 80_000_000_000), 100_000_000_000);
        assert_eq!(market_cap(1, 0, 1), 0);
    }

    #[test]
    fn a_pump_pool_prices_against_its_vault_plus_its_virtual_quote() {
        // A sale on pool 6VjSkP... simulated on 2026-09-26: the program's event gave
        // 17,749,442 lamports gross for 6,809,878,393 base units, which is the curve
        // over the vault plus the pool's 17,584,505,557-lamport virtual quote exactly.
        let mut acct = pool_account();
        acct[OFF_VIRTUAL_QUOTE..OFF_VIRTUAL_QUOTE + 8].copy_from_slice(&17_584_505_557u64.to_le_bytes());
        let p = decode_pool(&acct).unwrap();
        assert_eq!(p.effective_quote(223_151_404_199), 240_735_909_756);
        let s = to_pool_state([1u8; 32], &p, 92_355_657_266_285, 223_151_404_199, 0, 0).unwrap();
        // With no fee the leg is the plain curve over the effective reserves, which the
        // program's gross output matched to the lamport; the margin takes a hair off.
        let gross = s.leg_for_input(&p.base_mint).unwrap().quote(6_809_878_393).unwrap();
        assert!((17_749_442 - 40..=17_749_442).contains(&gross), "got {gross}");
    }

    #[test]
    fn the_pool_state_charges_the_whole_fee_on_the_quote_side() {
        let p = decode_pool(&pool_account()).unwrap();
        let s = to_pool_state([1u8; 32], &p, 1_000_000, 2_000_000, 125, 9).unwrap();
        assert_eq!(s.fee_ppm, 12_500);
        assert!(matches!(s.math, PoolMath::QuoteSideFee { quote_is_b: true, .. }));
        assert!(to_pool_state([1u8; 32], &p, 1, 1, 10_000, 9).is_err());
    }
}
