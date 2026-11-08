//! Chain neutral flash arbitrage core.

pub mod discovery;
pub mod error;
pub mod sizing;
pub mod strategy;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod types;
pub mod venue;
