//! Error type shared by every module.

/// Every failure the bot can report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
  /// A checked arithmetic operation overflowed.
  #[error("arithmetic overflow")]
  Overflow,
  /// A pool cannot fill the requested amount.
  #[error("insufficient liquidity")]
  InsufficientLiquidity,
  /// The token is not one of the pool's two tokens.
  #[error("token not in pool")]
  TokenNotInPool,
  /// The configuration file is invalid.
  #[error("config: {0}")]
  Config(String),
  /// A required environment value is missing or invalid.
  #[error("env: {0}")]
  Env(String),
  /// A filesystem or socket operation failed.
  #[error(transparent)]
  Io(#[from] std::io::Error),
  /// TOML parsing failed.
  #[error(transparent)]
  Toml(#[from] toml::de::Error),
  /// JSON encoding or decoding failed.
  #[error(transparent)]
  Json(#[from] serde_json::Error),
  /// A node RPC call failed.
  #[error("rpc: {0}")]
  Rpc(String),
  /// A contract call or deployment failed.
  #[error("contract: {0}")]
  Contract(String),
  /// The single instance lock could not be taken.
  #[error("lock: {0}")]
  Lock(String),
}

/// Result alias using the crate [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
