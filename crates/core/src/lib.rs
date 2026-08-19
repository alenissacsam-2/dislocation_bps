//! Domain types and money-safe arithmetic shared across the workspace.
//!
//! Rule for this crate: **no I/O, no floats in the profit path**. Everything here is
//! a pure function or a plain data type, so the money-critical logic is testable
//! without a network, an RPC key, or a clock.

pub mod amm;
pub mod config;
pub mod types;

/// Basis points, the unit fees are quoted in throughout.
pub const BPS: u32 = 10_000;
