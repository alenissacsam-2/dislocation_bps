//! cryptobot-desk — the application that owns the instrument.
//!
//! The web dashboard had one structural defect no restyling could fix: it was served
//! by the thing it observed. With `cb-bot` down there was no server, so there was no
//! UI — not a degraded one, none. This crate inverts that. The app is the durable
//! thing; the bot is a child process it supervises, and the ledger is read from disk
//! whether or not anything is running.

pub mod app;
pub mod balances;
pub mod archive;
pub mod config;
pub mod history;
pub mod logs;
pub mod paths;
pub mod runner;
pub mod wallet;
