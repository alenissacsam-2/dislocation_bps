//! HTTP + WebSocket server for the dashboard.
//!
//! The critical property here is **isolation**: the dashboard is a lossy observer of
//! the trading loop, never a participant. Events go out over a bounded
//! `tokio::sync::broadcast` channel, which drops for slow receivers rather than
//! applying backpressure. A frozen browser tab must never stall the scanner.

pub mod events;
pub mod routes;

pub use events::{Event, EventBus};
pub use routes::serve;
