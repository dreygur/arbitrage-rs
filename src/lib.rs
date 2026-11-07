//! Chain neutral flash arbitrage core.

pub mod error;
pub mod sizing;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod types;
pub mod venue;
