//! Domain types and money-safe arithmetic shared across the workspace.
//!
//! Rule for this crate: **no I/O, no floats in the profit path**. Everything here is
//! a pure function or a plain data type, so the money-critical logic is testable
//! without a network, an RPC key, or a clock.

pub mod amm;
pub mod clmm;
pub mod config;
pub mod path;
pub mod types;

/// Basis points. Fees are carried in parts per million (see [`amm::FeePpm`]); bps is
/// the unit *reported* in, because a human reads "−57 bps" faster than "−5700 ppm".
pub const BPS: u32 = 10_000;

/// Parts per million — the unit every fee in this workspace is stored in.
pub const PPM: u32 = 1_000_000;
