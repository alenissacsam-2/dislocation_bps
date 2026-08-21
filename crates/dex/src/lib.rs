//! Per-DEX account decoding and quote math.
//!
//! Each venue is one module exposing `PROGRAM_ID` and pure decode functions.
//! Decoders take reserves as parameters rather than fetching them, so they remain
//! testable without a network.

pub mod pumpswap;
pub mod raydium_v4;
