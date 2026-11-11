//! Chain neutral flash arbitrage core.

pub mod bot;
pub mod chain;
pub mod config;
pub mod discovery;
pub mod error;
pub mod journal;
pub mod sizing;
pub mod strategy;
pub mod supervisor;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod types;
pub mod ui;
pub mod venue;
