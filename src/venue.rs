//! Chain neutral view of a two token pool that can quote swaps.

use crate::error::{Error, Result};
use crate::types::{Amount, Id, Side};

/// A pool that quotes swaps without any chain specific detail.
pub trait Venue: Send + Sync {
  /// Identifier of the pool.
  fn pool(&self) -> &Id;

  /// The pool's two tokens in order, token zero first.
  fn tokens(&self) -> [&Id; 2];

  /// Amount of the output token received for `amount_in` swapped in direction `side`.
  fn quote_out(&self, amount_in: Amount, side: Side) -> Result<Amount>;

  /// Short human readable description for logs and the dashboard.
  fn describe(&self) -> String;
}

/// Returns the swap direction that spends `token_in`, or `TokenNotInPool` for a stranger.
pub fn side_for(venue: &dyn Venue, token_in: &Id) -> Result<Side> {
  let [token0, token1] = venue.tokens();
  if token_in == token0 {
    Ok(Side::ZeroForOne)
  } else if token_in == token1 {
    Ok(Side::OneForZero)
  } else {
    Err(Error::TokenNotInPool)
  }
}
