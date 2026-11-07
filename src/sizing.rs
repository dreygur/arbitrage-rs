//! Profit search over trade size for a buy and sell venue pair.

use crate::error::Result;
use crate::types::{Amount, Id, Side};
use crate::venue::{Venue, side_for};

/// Window size at which the ternary search stops and scans every size.
const SCAN_WINDOW: u64 = 3;

/// Profit of borrowing `x` of `base`, buying on `buy` and selling back on `sell`.
///
/// Returns `Ok(None)` when the round trip does not return more than `x`, and `Err` when
/// either quote fails.
pub fn gain(buy: &dyn Venue, sell: &dyn Venue, base: &Id, x: Amount) -> Result<Option<Amount>> {
  let buy_side = side_for(buy, base)?;
  let [token0, token1] = buy.tokens();
  let other = match buy_side {
    Side::ZeroForOne => token1,
    Side::OneForZero => token0,
  };
  let bought = buy.quote_out(x, buy_side)?;
  let back = sell.quote_out(bought, side_for(sell, other)?)?;
  Ok(back.checked_sub(x).filter(|profit| !profit.is_zero()))
}

/// Largest size in `[1, max_trade]` that quotes without error, assuming errors only occur
/// above some limit. `None` when even size one fails.
fn largest_quotable(
  buy: &dyn Venue,
  sell: &dyn Venue,
  base: &Id,
  max_trade: Amount,
) -> Option<Amount> {
  let one = Amount::from(1u64);
  if max_trade.is_zero() || gain(buy, sell, base, one).is_err() {
    return None;
  }
  let (mut lo, mut hi) = (one, max_trade);
  while lo < hi {
    let mid = lo + (hi - lo + one) / Amount::from(2u64);
    if gain(buy, sell, base, mid).is_err() {
      hi = mid - one;
    } else {
      lo = mid;
    }
  }
  Some(lo)
}

/// Finds the size in `[1, max_trade]` with the highest profit.
///
/// Returns `(amount, profit)` with `profit > 0`, or `None` when no size is profitable.
/// Sizes that fail to quote or lose money score as minus infinity.
pub fn best_amount(
  buy: &dyn Venue,
  sell: &dyn Venue,
  base: &Id,
  max_trade: Amount,
) -> Option<(Amount, Amount)> {
  let score = |x: Amount| gain(buy, sell, base, x).ok().flatten();
  let one = Amount::from(1u64);
  let (mut lo, mut hi) = (one, largest_quotable(buy, sell, base, max_trade)?);
  while hi - lo >= Amount::from(SCAN_WINDOW) {
    let third = (hi - lo) / Amount::from(3u64);
    let (m1, m2) = (lo + third, hi - third);
    if score(m1) < score(m2) {
      lo = m1 + one;
    } else {
      hi = m2 - one;
    }
  }
  let mut best: Option<(Amount, Amount)> = None;
  let mut x = lo;
  while x <= hi {
    if let Some(profit) = score(x)
      && best.is_none_or(|(_, top)| profit > top)
    {
      best = Some((x, profit));
    }
    x += one;
  }
  best
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::Error;
  use crate::testkit::MockVenue;

  /// Base token id used by the tests.
  fn base() -> Id {
    Id::new(b"base".to_vec())
  }

  /// Largest profit over every size up to `limit`, found by exhaustive search.
  fn brute(buy: &dyn Venue, sell: &dyn Venue, limit: u64) -> Amount {
    let mut best = Amount::ZERO;
    for x in 1..=limit {
      if let Ok(Some(profit)) = gain(buy, sell, &base(), Amount::from(x)) {
        best = best.max(profit);
      }
    }
    best
  }

  /// A pool token is found on its side and a stranger is rejected.
  #[test]
  fn side_for_finds_token_and_rejects_strangers() {
    let v = MockVenue::new("p", "base", "other", 10, 10, 30);
    assert_eq!(side_for(&v, &base()).unwrap(), Side::ZeroForOne);
    assert_eq!(
      side_for(&v, &Id::new(b"other".to_vec())).unwrap(),
      Side::OneForZero
    );
    assert!(matches!(
      side_for(&v, &Id::new(b"x".to_vec())),
      Err(Error::TokenNotInPool)
    ));
  }

  /// The search lands within one wei of the exhaustive maximum.
  #[test]
  fn finds_near_optimal_size() {
    let buy = MockVenue::new("a", "base", "other", 10_000, 25_000, 30);
    let sell = MockVenue::new("b", "base", "other", 10_000, 20_000, 30);
    let (_, profit) = best_amount(&buy, &sell, &base(), Amount::from(10_000u64)).unwrap();
    let want = brute(&buy, &sell, 10_000);
    assert!(want > Amount::ZERO);
    assert!(profit + Amount::from(1u64) >= want);
  }

  /// Identical pools lose to fees at every size.
  #[test]
  fn balanced_pools_have_no_profit() {
    let buy = MockVenue::new("a", "base", "other", 10_000, 20_000, 30);
    let sell = MockVenue::new("b", "base", "other", 10_000, 20_000, 30);
    assert!(best_amount(&buy, &sell, &base(), Amount::from(10_000u64)).is_none());
  }

  /// A liquidity failure far above the peak does not hide the peak.
  #[test]
  fn error_region_does_not_capture_search() {
    let buy = MockVenue::new("a", "base", "other", 100_000, 202_000, 0);
    let mut sell = MockVenue::new("b", "base", "other", 100_000, 200_000, 0);
    sell.fail_above = Some(Amount::from(3_000u64));
    let want = brute(&buy, &sell, 3_000);
    assert!(want > Amount::ZERO);
    let (_, profit) = best_amount(&buy, &sell, &base(), Amount::from(50_000u64)).unwrap();
    assert_eq!(profit, want);
  }

  /// Integer flats around the peak do not make the search skip it.
  #[test]
  fn ties_do_not_skip_peak() {
    let buy = MockVenue::new("a", "base", "other", 1_000_000_000, 1_002_000_000, 0);
    let sell = MockVenue::new("b", "base", "other", 1_000_000_000, 1_000_000_000, 0);
    let want = brute(&buy, &sell, 1_000_000);
    let (_, profit) = best_amount(&buy, &sell, &base(), Amount::from(1_000_000u64)).unwrap();
    assert!(profit + Amount::from(1u64) >= want);
  }

  /// The returned size never exceeds the cap.
  #[test]
  fn respects_max_trade() {
    let buy = MockVenue::new("a", "base", "other", 10_000, 25_000, 30);
    let sell = MockVenue::new("b", "base", "other", 10_000, 20_000, 30);
    let (amount, _) = best_amount(&buy, &sell, &base(), Amount::from(100u64)).unwrap();
    assert!(amount <= Amount::from(100u64));
  }
}
