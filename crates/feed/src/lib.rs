//! Live account data from Solana.
//!
//! Phase 1 uses plain `accountSubscribe` over WebSocket, which is the free tier. Its
//! limitations are real and must not be papered over — see [`FeedStats`]:
//!
//! - It runs **hundreds of milliseconds behind** the chain head.
//! - It **coalesces** rapid updates, so intermediate states are silently skipped.
//!   We cannot detect what we never saw, so measured opportunity counts are a
//!   *lower bound*, not a census.
//! - It degrades past a few hundred subscriptions.
//!
//! A Yellowstone gRPC implementation behind the same [`Feed`] trait would cut latency
//! to single-digit milliseconds, but costs ~$99+/month — a decision that should be
//! made from measured data, not before.

pub mod ws;

pub use ws::{FeedStats, WsFeed};

use cb_core::types::Pubkey32;

/// One account changed on-chain.
#[derive(Debug, Clone)]
pub struct AccountUpdate {
    pub pubkey: Pubkey32,
    pub data: Vec<u8>,
    pub slot: u64,
    /// When we received it locally. Used to measure our own lag, not the chain's.
    pub received_ms: u64,
}

/// A source of account updates. Implemented by [`WsFeed`] now, gRPC later.
pub trait Feed {
    /// Subscribe to a fixed set of accounts and stream their updates.
    fn subscribe(
        &self,
        accounts: Vec<Pubkey32>,
    ) -> tokio::sync::mpsc::Receiver<AccountUpdate>;
}
