//! Shared test doubles.

use crate::error::{Error, Result};
use crate::types::{Amount, Id, Side};
use crate::venue::Venue;

/// Fee denominator in basis points.
const BPS: u64 = 10_000;

/// Constant product pool with a fee, standing in for a real venue.
pub struct MockVenue {
  /// Pool identifier.
  pool: Id,
  /// Token zero and token one.
  tokens: [Id; 2],
  /// Reserve of token zero.
  r0: Amount,
  /// Reserve of token one.
  r1: Amount,
  /// Swap fee in basis points.
  fee_bps: u16,
  /// Largest input the pool fills; bigger inputs fail with `InsufficientLiquidity`.
  pub fail_above: Option<Amount>,
}

impl MockVenue {
  /// Builds a pool whose ids are the UTF8 bytes of the given names.
  pub fn new(pool: &str, token0: &str, token1: &str, r0: u64, r1: u64, fee_bps: u16) -> Self {
    Self {
      pool: Id::new(pool.as_bytes().to_vec()),
      tokens: [
        Id::new(token0.as_bytes().to_vec()),
        Id::new(token1.as_bytes().to_vec()),
      ],
      r0: Amount::from(r0),
      r1: Amount::from(r1),
      fee_bps,
      fail_above: None,
    }
  }
}

impl Venue for MockVenue {
  /// Returns the pool id.
  fn pool(&self) -> &Id {
    &self.pool
  }

  /// Returns both token ids.
  fn tokens(&self) -> [&Id; 2] {
    [&self.tokens[0], &self.tokens[1]]
  }

  /// Quotes with `out = in * (1 - fee) * r_out / (r_in + in * (1 - fee))`, using checked math.
  fn quote_out(&self, amount_in: Amount, side: Side) -> Result<Amount> {
    if self.fail_above.is_some_and(|limit| amount_in > limit) {
      return Err(Error::InsufficientLiquidity);
    }
    let (r_in, r_out) = match side {
      Side::ZeroForOne => (self.r0, self.r1),
      Side::OneForZero => (self.r1, self.r0),
    };
    let keep = Amount::from(BPS - u64::from(self.fee_bps));
    let in_fee = amount_in.checked_mul(keep).ok_or(Error::Overflow)?;
    let num = in_fee.checked_mul(r_out).ok_or(Error::Overflow)?;
    let den = r_in
      .checked_mul(Amount::from(BPS))
      .and_then(|scaled| scaled.checked_add(in_fee))
      .ok_or(Error::Overflow)?;
    num.checked_div(den).ok_or(Error::InsufficientLiquidity)
  }

  /// Returns `mock {pool}`.
  fn describe(&self) -> String {
    format!("mock {}", String::from_utf8_lossy(self.pool.as_bytes()))
  }
}
