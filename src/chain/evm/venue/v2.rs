//! Uniswap V2 style constant product pool.

use crate::error::{Error, Result};
use crate::types::{Amount, Id, Side};
use crate::venue::Venue;

/// Denominator of basis points.
const BPS: u64 = 10_000;

/// A V2 pair holding the reserves last seen on chain.
#[derive(Clone, Debug)]
pub struct V2Pool {
  /// Name of the venue (factory) the pair belongs to.
  venue: String,
  /// Pair address.
  pool: Id,
  /// First token of the pair.
  token0: Id,
  /// Second token of the pair.
  token1: Id,
  /// Reserve of token zero.
  reserve0: Amount,
  /// Reserve of token one.
  reserve1: Amount,
  /// Swap fee in basis points.
  fee_bps: u16,
}

impl V2Pool {
  /// Builds a pool from its identity, reserves and fee.
  pub fn new(
    venue: String,
    pool: Id,
    token0: Id,
    token1: Id,
    reserve0: Amount,
    reserve1: Amount,
    fee_bps: u16,
  ) -> Self {
    Self {
      venue,
      pool,
      token0,
      token1,
      reserve0,
      reserve1,
      fee_bps,
    }
  }

  /// Replaces the reserves after a `Sync` event.
  pub fn set_reserves(&mut self, reserve0: Amount, reserve1: Amount) {
    self.reserve0 = reserve0;
    self.reserve1 = reserve1;
  }
}

impl Venue for V2Pool {
  /// Pair address.
  fn pool(&self) -> &Id {
    &self.pool
  }

  /// Both tokens, token zero first.
  fn tokens(&self) -> [&Id; 2] {
    [&self.token0, &self.token1]
  }

  /// Uniswap V2 `getAmountOut` with this pool's fee, in checked arithmetic.
  fn quote_out(&self, amount_in: Amount, side: Side) -> Result<Amount> {
    let (reserve_in, reserve_out) = match side {
      Side::ZeroForOne => (self.reserve0, self.reserve1),
      Side::OneForZero => (self.reserve1, self.reserve0),
    };
    if reserve_in.is_zero() || reserve_out.is_zero() {
      return Err(Error::InsufficientLiquidity);
    }
    let keep = Amount::from(BPS - u64::from(self.fee_bps.min(BPS as u16)));
    let with_fee = amount_in.checked_mul(keep).ok_or(Error::Overflow)?;
    let numerator = with_fee.checked_mul(reserve_out).ok_or(Error::Overflow)?;
    let denominator = reserve_in
      .checked_mul(Amount::from(BPS))
      .and_then(|v| v.checked_add(with_fee))
      .ok_or(Error::Overflow)?;
    Ok(numerator / denominator)
  }

  /// Venue name, fee and reserves.
  fn describe(&self) -> String {
    format!(
      "{} {}bps r0={} r1={}",
      self.venue, self.fee_bps, self.reserve0, self.reserve1
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Builds a pool with the given reserves and fee.
  fn pool(r0: Amount, r1: Amount, fee_bps: u16) -> V2Pool {
    V2Pool::new(
      "uniswap_v2".into(),
      Id::new(vec![1; 20]),
      Id::new(vec![2; 20]),
      Id::new(vec![3; 20]),
      r0,
      r1,
      fee_bps,
    )
  }

  /// The quote matches the Uniswap V2 formula at 30 bps.
  #[test]
  fn quote_out_matches_uniswap() {
    let p = pool(Amount::from(1000u64), Amount::from(2000u64), 30);
    assert_eq!(
      p.quote_out(Amount::from(1000u64), Side::ZeroForOne)
        .unwrap(),
      Amount::from(998u64)
    );
  }

  /// A different fee gives a different quote.
  #[test]
  fn fee_is_not_hardcoded() {
    let low = pool(Amount::from(1000u64), Amount::from(2000u64), 0);
    let high = pool(Amount::from(1000u64), Amount::from(2000u64), 300);
    let a = low
      .quote_out(Amount::from(1000u64), Side::ZeroForOne)
      .unwrap();
    let b = high
      .quote_out(Amount::from(1000u64), Side::ZeroForOne)
      .unwrap();
    assert_eq!(a, Amount::from(1000u64));
    assert!(b < a);
  }

  /// An empty side cannot fill a swap.
  #[test]
  fn zero_reserves_are_insufficient() {
    let p = pool(Amount::ZERO, Amount::from(2000u64), 30);
    assert!(matches!(
      p.quote_out(Amount::from(1u64), Side::ZeroForOne),
      Err(Error::InsufficientLiquidity)
    ));
    assert!(matches!(
      p.quote_out(Amount::from(1u64), Side::OneForZero),
      Err(Error::InsufficientLiquidity)
    ));
  }

  /// Reserves at the uint112 maximum quote without overflow.
  #[test]
  fn uint112_max_reserves_do_not_overflow() {
    let max = (Amount::from(1u64) << 112) - Amount::from(1u64);
    let p = pool(max, max, 30);
    let out = p.quote_out(max, Side::ZeroForOne).unwrap();
    assert!(out > Amount::ZERO && out < max);
  }

  /// The description names the venue, fee and reserves.
  #[test]
  fn describe_names_venue() {
    let p = pool(Amount::from(5u64), Amount::from(7u64), 30);
    assert_eq!(p.describe(), "uniswap_v2 30bps r0=5 r1=7");
  }
}
