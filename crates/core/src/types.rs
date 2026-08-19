//! Core domain types. Deliberately free of Solana SDK dependencies so this crate
//! stays pure and fast to compile; pubkeys are raw 32-byte arrays and are converted
//! at the edges.

/// A raw Solana public key. Kept as bytes so `cb-core` needs no solana-sdk dependency.
pub type Pubkey32 = [u8; 32];

/// Identifier for a liquidity pool — its on-chain account address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolId(pub Pubkey32);

/// Which venue a pool belongs to. Determines the decoder and quote math used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dex {
    RaydiumAmmV4,
    PumpSwap,
}

impl Dex {
    /// Human-readable name, used in logs and on the dashboard.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Dex::RaydiumAmmV4 => "Raydium AMM v4",
            Dex::PumpSwap => "PumpSwap",
        }
    }
}

/// Reserves oriented for one swap direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserves {
    /// Reserve of the mint being spent.
    pub r_in: u128,
    /// Reserve of the mint being received.
    pub r_out: u128,
}

/// A decoded constant-product pool at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    pub id: PoolId,
    pub dex: Dex,
    pub mint_a: Pubkey32,
    pub mint_b: Pubkey32,
    pub reserve_a: u128,
    pub reserve_b: u128,
    pub fee_bps: u32,
    /// Slot this state was observed at. Used for staleness checks.
    pub slot: u64,
}

impl PoolState {
    /// Reserves oriented so `r_in` is the reserve of `input_mint`.
    ///
    /// Returns `None` if this pool does not trade `input_mint`.
    #[must_use]
    pub fn reserves_for_input(&self, input_mint: &Pubkey32) -> Option<Reserves> {
        if *input_mint == self.mint_a {
            Some(Reserves { r_in: self.reserve_a, r_out: self.reserve_b })
        } else if *input_mint == self.mint_b {
            Some(Reserves { r_in: self.reserve_b, r_out: self.reserve_a })
        } else {
            None
        }
    }

    /// The counterparty mint to `mint`, or `None` if this pool doesn't trade it.
    #[must_use]
    pub fn other_mint(&self, mint: &Pubkey32) -> Option<Pubkey32> {
        if *mint == self.mint_a {
            Some(self.mint_b)
        } else if *mint == self.mint_b {
            Some(self.mint_a)
        } else {
            None
        }
    }
}

/// A detected (not executed) arbitrage opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opportunity {
    pub pool_buy: PoolId,
    pub pool_sell: PoolId,
    /// Mint we start and end with (the token we hold).
    pub base_mint: Pubkey32,
    /// Intermediate mint we route through.
    pub quote_mint: Pubkey32,
    /// Optimal input size in base-mint base units.
    pub amount_in: u128,
    /// Gross profit in base-mint base units, before fees and tip.
    pub gross_profit: u128,
    pub slot: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_state_exposes_reserves_for_a_given_direction() {
        let p = PoolState {
            id: PoolId([1u8; 32]),
            dex: Dex::RaydiumAmmV4,
            mint_a: [10u8; 32],
            mint_b: [20u8; 32],
            reserve_a: 1_000_000,
            reserve_b: 2_000_000,
            fee_bps: 25,
            slot: 42,
        };
        // Spending mint_a: we pay into reserve_a, receive from reserve_b.
        assert_eq!(
            p.reserves_for_input(&[10u8; 32]),
            Some(Reserves { r_in: 1_000_000, r_out: 2_000_000 })
        );
        // Spending mint_b: the other way round.
        assert_eq!(
            p.reserves_for_input(&[20u8; 32]),
            Some(Reserves { r_in: 2_000_000, r_out: 1_000_000 })
        );
        // A mint this pool does not trade.
        assert_eq!(p.reserves_for_input(&[99u8; 32]), None);
    }

    #[test]
    fn other_mint_returns_the_counterparty() {
        let p = PoolState {
            id: PoolId([1u8; 32]),
            dex: Dex::PumpSwap,
            mint_a: [10u8; 32],
            mint_b: [20u8; 32],
            reserve_a: 1,
            reserve_b: 1,
            fee_bps: 25,
            slot: 0,
        };
        assert_eq!(p.other_mint(&[10u8; 32]), Some([20u8; 32]));
        assert_eq!(p.other_mint(&[20u8; 32]), Some([10u8; 32]));
        assert_eq!(p.other_mint(&[99u8; 32]), None);
    }
}
