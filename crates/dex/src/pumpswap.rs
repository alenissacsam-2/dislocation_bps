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
//! in the pool account (a **signed** `i64` at byte 245, newer than the published IDL):
//! about 17.58 SOL on every pump pool read on 2026-09-26. Purchases get fewer tokens and
//! sales more SOL than the vault alone would give. Without it the quote was 6-9% off on
//! a live pool; with it, a simulated sale matched the program's own event to the
//! lamport and a purchase to a thousandth of a basis point. The market cap that picks
//! the fee tier is measured on the same effective reserve — the one data point the
//! vault-only market cap put in the wrong tier moved into the right one.
//!
//! On a pool pump.fun did not create the field is a small **negative** number (the
//! eight bytes after it are its sign extension): the vault holds that much more than
//! the curve prices against. Read unsigned it wrapped to about 1.8e19, which priced the
//! $10.8M PUMP/USDC pool `2uF4Xh61…` 1.7 million times too high and put a
//! 17,000,086,981 bp "edge" on the desk on 2026-11-07. Two sales on that pool matched
//! vault + the signed value to the unit; vault alone was off by 1 and 7.
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

/// `disable_flags` bits 3 (buy) and 4 (sell).
const DISABLE_SWAP_MASK: u8 = 0b1_1000;

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
    /// Added to the quote vault's balance wherever the program prices. Signed: negative
    /// on pools pump.fun did not create. Zero when the account predates the field.
    pub virtual_quote: i64,
}

impl PumpPool {
    /// The quote reserve the program prices against: the vault's balance plus the
    /// pool's virtual quote.
    #[must_use]
    pub fn effective_quote(&self, quote_vault_balance: u64) -> u64 {
        quote_vault_balance.saturating_add_signed(self.virtual_quote)
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
        virtual_quote: if data.len() >= OFF_VIRTUAL_QUOTE + 8 {
            i64::from_le_bytes(data[OFF_VIRTUAL_QUOTE..OFF_VIRTUAL_QUOTE + 8].try_into().expect("8 bytes"))
        } else {
            0
        },
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
    /// Decoded but deliberately not charged: nothing here confirms the program takes it
    /// from swappers (the recipients are only a remaining account), so it is not applied.
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
    // IDL `disable_flags`: bit 3 disables buy, bit 4 disables sell. This quotes both
    // directions, so either one set means the pool cannot be quoted.
    let flags = data[OFF_G_DISABLE];
    ensure!(flags & DISABLE_SWAP_MASK == 0, "pumpswap global config disables swaps (flags {flags:#010b})");
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
    fn a_global_config_that_disables_buy_or_sell_is_refused() {
        let mut g = vec![0u8; GLOBAL_MIN_LEN];
        assert!(decode_global_config(&g).is_ok());
        g[OFF_G_DISABLE] = 0b0_0111; // create, deposit, withdraw off: swaps still allowed
        assert!(decode_global_config(&g).is_ok());
        for bit in [0b0_1000u8, 0b1_0000] {
            g[OFF_G_DISABLE] = bit;
            assert!(decode_global_config(&g).is_err(), "flag {bit:#b}");
        }
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

    /// The PUMP/USDC pool `2uF4Xh61rDwxnG9woyxsVQP7zuA6kLFpb3NvnRQeoiSd` as read from
    /// chain on 2026-11-08: not a pump pool, virtual quote -1,633,261.
    fn pump_usdc_account() -> Vec<u8> {
        let hex = concat!(
            "f19a6d0411b16dbcfe000018297a191d2a929540d322ce36d3c31954c8737306e4e9abaf5065ee1785e8a30c45f7df8d",
            "9e72956284933f6d98b757032e83df84604fb5e117fff61d5b12f9c6fa7af3bedbad3a3d65f36aabc97431b1bbe4c2d2",
            "f6e0e47ca60203452f5d611ab0f37131762c07711409422e7c3a9f0246d77a1af89eb3d198caff0a610fc36ee1890bfd",
            "9f6e2d44da178cec6ff13e79b2c89a4e23ac2ad70bb756f1a8c3534c35e06b861e6923b1038ab10987085c4215eb9e49",
            "c917bfd2b9c5e639ed7ded6d7c8a21ae7900000000000000000000000000000000000000000000000000000000000000",
            "00000000001314e7ffffffffffffffffffffffffff00000000000000000000edeb180000000000000000000000000000",
            "000000000000000000000000",
        );
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    /// Read unsigned, this pool's virtual quote wrapped to 1.8e19, its quote reserve
    /// saturated, and PUMP priced 1.7 million times too high: the 17,000,086,981 bp edge
    /// on the desk on 2026-11-07. Signed, two real sales price to the program's own events.
    #[test]
    fn a_negative_virtual_quote_prices_real_sales_to_the_unit() {
        let acct = pump_usdc_account();
        let p = decode_pool(&acct).unwrap();
        assert_eq!(p.virtual_quote, -1_633_261);
        let unsigned = u64::from_le_bytes(acct[OFF_VIRTUAL_QUOTE..OFF_VIRTUAL_QUOTE + 8].try_into().unwrap());
        assert_eq!(10_827_267_412_646u64.saturating_add(unsigned), u64::MAX, "the old reading");

        // SellEvents at slots 466629380 and 466629415: base in, the pool's reserves as the
        // event reports them (the vault balances), and the gross quote the program paid.
        for (base_in, base_reserve, vault, paid) in [
            (1_008_810_033u64, 2_920_567_507_563_040u64, 10_827_267_412_646u64, 3_739_906u128),
            (12_846_832_429, 2_920_568_516_373_073, 10_827_263_682_090, 47_626_141),
        ] {
            assert_eq!(p.effective_quote(vault), vault - 1_633_261);
            let s = to_pool_state([1u8; 32], &p, base_reserve, vault, 0, 0).unwrap();
            let out = s.leg_for_input(&p.base_mint).unwrap().quote(u128::from(base_in)).unwrap();
            // A floor: never over what the program paid, and under it only by the margin.
            assert!(out <= paid && paid - out <= paid / 300_000, "quoted {out}, the program paid {paid}");
        }
        let floor = PumpPool { virtual_quote: -5, ..p };
        assert_eq!(floor.effective_quote(3), 0, "a deficit past the vault prices nothing");
    }

    /// The SOL → ZACK → SOL "edge" of 54,361,175 bps refused at start-up on 2026-11-08 is
    /// the market, not a decode: DAMM v2 `DXPwyj…` and PumpSwap `EQmXSx…` read at slot
    /// 466642187 reproduce it to the basis point. The PumpSwap pool's creator withdrew
    /// all of its liquidity on 2026-09-26 (138.49 SOL out, LP burned), leaving 4 lamports
    /// and 3,354 ZACK units at that day's price, while ZACK fell ~5,000x on the DAMM pool.
    /// No size of input gets more than 3 lamports back.
    #[test]
    fn a_drained_pool_shows_a_real_edge_with_three_lamports_behind_it() {
        let hex = |s: &str| -> Vec<u8> {
            let s = s.trim();
            (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
        };
        let damm_acct = hex(include_str!("../tests/fixtures/meteora_damm_v2_zack_sol_466642187.hex"));
        let pump = decode_pool(&hex(include_str!("../tests/fixtures/pumpswap_sol_zack_466642187.hex"))).unwrap();
        assert_eq!(pump.virtual_quote, 0);
        // Vault balances in the same read: base (WSOL) 4, quote (ZACK) 3,354.
        let pump = to_pool_state([1; 32], &pump, 4, 3_354, 30, 0).unwrap();
        let damm = crate::meteora_damm_v2::to_pool_state([2; 32], &damm_acct, 466_642_187, 1_794_100_000).unwrap();
        let (sol, zack) = (pump.mint_a, pump.mint_b);
        assert_eq!((damm.mint_a, damm.mint_b), (zack, sol));

        let legs = [damm.leg_for_input(&sol).unwrap(), pump.leg_for_input(&zack).unwrap()];
        let edge = cb_core::path::marginal_edge_bps(&legs).unwrap();
        assert!((edge - 54_361_175.0).abs() < 1.0, "the logged edge: {edge}");
        for x in [1u128, 1_000, 100_000, 10_000_000] {
            let back = legs[0].quote(x).and_then(|z| legs[1].quote(z)).unwrap();
            assert!(back <= 3, "{x} lamports came back as {back}");
        }

        // The DAMM side against the program: its last swap (2026-11-04, 5MAAq9…) sold
        // 98,618,982,062 ZACK for 135,746 lamports. Undoing it on the decoded curve:
        let p = crate::meteora_damm_v2::decode_layout(&damm_acct).unwrap();
        let (l, sold) = (p.liquidity, 98_618_982_062u128);
        let a_after = (l << 64) / p.sqrt_price_x64;
        let gross = l * l / (a_after - sold) - l * l / a_after;
        let net = gross - gross.div_ceil(1_000); // 10 bp from the output
        assert!(net.abs_diff(135_746) <= 2, "the program paid 135,746; the curve gives {net}");
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
