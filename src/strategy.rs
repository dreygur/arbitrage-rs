//! Opportunity search across token pairs and the venues that list them.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::sizing::{best_amount, gain};
use crate::types::{Amount, Id, Opportunity};
use crate::venue::{Venue, side_for};

/// A trade with the venues it buys on and sells on.
type Priced = (Opportunity, Arc<dyn Venue>, Arc<dyn Venue>);

/// Outcome of evaluating one token pair across all of its venues.
pub enum Evaluation {
  /// The best profitable trade of the group.
  Found {
    /// The trade to consider.
    op: Opportunity,
    /// Venue where the non base token is bought.
    buy: Arc<dyn Venue>,
    /// Venue where the non base token is sold.
    sell: Arc<dyn Venue>,
  },
  /// No venue pair of the group produced a trade and one errored at size one.
  Failed {
    /// Venue where the non base token would be bought.
    buy: Arc<dyn Venue>,
    /// Venue where the non base token would be sold.
    sell: Arc<dyn Venue>,
    /// The quote failure.
    error: Error,
  },
}

/// The non base token of `venue`, or `TokenNotInPool` when it does not list `base`.
fn other_token<'a>(venue: &'a dyn Venue, base: &Id) -> Result<&'a Id> {
  side_for(venue, base)?;
  let [token0, token1] = venue.tokens();
  Ok(if token0 == base { token1 } else { token0 })
}

/// Best trade buying on `buy` and selling on `sell`, with a zero floor.
fn directed(
  buy: &dyn Venue,
  sell: &dyn Venue,
  base: &Id,
  other: &Id,
  max_trade: Amount,
) -> Option<Opportunity> {
  let (amount, profit) = best_amount(buy, sell, base, max_trade)?;
  Some(Opportunity {
    buy_pool: buy.pool().clone(),
    sell_pool: sell.pool().clone(),
    token: other.clone(),
    amount,
    profit,
    floor: Amount::ZERO,
  })
}

/// Best trade between two venues of one pair, trying both directions.
///
/// Returns `TokenNotInPool` when the venues do not share the same pair with `base`.
pub fn find(
  a: &dyn Venue,
  b: &dyn Venue,
  base: &Id,
  max_trade: Amount,
) -> Result<Option<Opportunity>> {
  let other = other_token(a, base)?;
  if other_token(b, base)? != other {
    return Err(Error::TokenNotInPool);
  }
  let forward = directed(a, b, base, other, max_trade);
  let backward = directed(b, a, base, other, max_trade);
  Ok(match (forward, backward) {
    (Some(f), Some(r)) => Some(if r.profit > f.profit { r } else { f }),
    (f, r) => f.or(r),
  })
}

/// First direction of the pair whose quote fails at size one.
fn size_one_failure(a: &Arc<dyn Venue>, b: &Arc<dyn Venue>, base: &Id) -> Option<Evaluation> {
  let one = Amount::from(1u64);
  [(a, b), (b, a)].into_iter().find_map(|(buy, sell)| {
    gain(buy.as_ref(), sell.as_ref(), base, one)
      .err()
      .map(|error| Evaluation::Failed {
        buy: buy.clone(),
        sell: sell.clone(),
        error,
      })
  })
}

/// Evaluates one group of venues sharing a token pair.
fn evaluate_group(group: &[Arc<dyn Venue>], base: &Id, max_trade: Amount) -> Option<Evaluation> {
  let mut best: Option<Priced> = None;
  let mut failure = None;
  for (i, a) in group.iter().enumerate() {
    for b in &group[i + 1..] {
      match find(a.as_ref(), b.as_ref(), base, max_trade) {
        Ok(Some(op)) => {
          if best
            .as_ref()
            .is_none_or(|(top, _, _)| op.profit > top.profit)
          {
            let (buy, sell) = if op.buy_pool == *a.pool() {
              (a.clone(), b.clone())
            } else {
              (b.clone(), a.clone())
            };
            best = Some((op, buy, sell));
          }
        }
        Ok(None) => {
          if best.is_none() && failure.is_none() {
            failure = size_one_failure(a, b, base);
          }
        }
        Err(error) => {
          if failure.is_none() {
            failure = Some(Evaluation::Failed {
              buy: a.clone(),
              sell: b.clone(),
              error,
            });
          }
        }
      }
    }
  }
  match best {
    Some((op, buy, sell)) => Some(Evaluation::Found { op, buy, sell }),
    None => failure,
  }
}

/// Evaluates every token pair that lists `base`, best trade per pair.
///
/// Venues are grouped by unordered token pair. When `touched` is given, only groups with a
/// pool in it are evaluated. Results are `Found` by profit descending, then `Failed`.
///
/// A full evaluation (`touched` is `None`) runs the groups in parallel on the rayon pool,
/// which the step bench measured well over 20% faster than a loop. A touched step runs
/// its few groups in a loop, since fanning out to the pool costs more than it saves there
/// (see `docs/superpowers/reviews/bench.md`). Both keep group order, so the output is
/// deterministic.
pub fn find_all(
  venues: &[Arc<dyn Venue>],
  base: &Id,
  max_trade: Amount,
  touched: Option<&HashSet<Id>>,
) -> Vec<Evaluation> {
  let mut groups: BTreeMap<(Id, Id), Vec<Arc<dyn Venue>>> = BTreeMap::new();
  for venue in venues {
    let [token0, token1] = venue.tokens();
    if side_for(venue.as_ref(), base).is_err() {
      continue;
    }
    let key = if token0 <= token1 {
      (token0.clone(), token1.clone())
    } else {
      (token1.clone(), token0.clone())
    };
    groups.entry(key).or_default().push(venue.clone());
  }
  let groups: Vec<Vec<Arc<dyn Venue>>> = groups.into_values().collect();
  let evaluations: Vec<Evaluation> = match touched {
    None => groups
      .par_iter()
      .filter_map(|group| evaluate_group(group, base, max_trade))
      .collect(),
    Some(set) => groups
      .iter()
      .filter(|group| group.iter().any(|v| set.contains(v.pool())))
      .filter_map(|group| evaluate_group(group, base, max_trade))
      .collect(),
  };
  let (mut found, failed): (Vec<Evaluation>, Vec<Evaluation>) = evaluations
    .into_iter()
    .partition(|e| matches!(e, Evaluation::Found { .. }));
  found.sort_by(|x, y| match (x, y) {
    (Evaluation::Found { op: ox, .. }, Evaluation::Found { op: oy, .. }) => {
      oy.profit.cmp(&ox.profit)
    }
    _ => std::cmp::Ordering::Equal,
  });
  found.extend(failed);
  found
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testkit::MockVenue;

  /// Base token id used by the tests.
  fn base() -> Id {
    Id::new(b"base".to_vec())
  }

  /// Id from a name.
  fn id(name: &str) -> Id {
    Id::new(name.as_bytes().to_vec())
  }

  /// Shared handle to a mock pool of `base` and `other`.
  fn venue(pool: &str, other: &str, r1: u64) -> Arc<dyn Venue> {
    Arc::new(MockVenue::new(pool, "base", other, 10_000, r1, 30))
  }

  /// The opportunity's pools in the order found.
  fn pools(eval: &Evaluation) -> (Id, Id) {
    match eval {
      Evaluation::Found { op, .. } => (op.buy_pool.clone(), op.sell_pool.clone()),
      Evaluation::Failed { .. } => panic!("expected Found"),
    }
  }

  /// The pool that holds more of the other token is the cheap one to buy on.
  #[test]
  fn picks_cheap_pool_to_buy_on() {
    let a = MockVenue::new("a", "base", "other", 10_000, 25_000, 30);
    let b = MockVenue::new("b", "base", "other", 10_000, 20_000, 30);
    let max = Amount::from(10_000u64);
    let op = find(&a, &b, &base(), max).unwrap().unwrap();
    assert_eq!(op.buy_pool, id("a"));
    assert_eq!(op.sell_pool, id("b"));
    assert_eq!(op.token, id("other"));
    assert_eq!(op.floor, Amount::ZERO);
    let x = op.amount.to::<u128>();
    let bought = x * 9970 * 25_000 / (10_000 * 10_000 + x * 9970);
    let back = bought * 9970 * 10_000 / (20_000 * 10_000 + bought * 9970);
    assert_eq!(op.profit, Amount::from(back - x));
  }

  /// Swapping the imbalance flips the direction.
  #[test]
  fn reverse_imbalance_flips_direction() {
    let a = MockVenue::new("a", "base", "other", 10_000, 20_000, 30);
    let b = MockVenue::new("b", "base", "other", 10_000, 25_000, 30);
    let op = find(&a, &b, &base(), Amount::from(10_000u64))
      .unwrap()
      .unwrap();
    assert_eq!(op.buy_pool, id("b"));
    assert_eq!(op.sell_pool, id("a"));
  }

  /// Identical pools produce nothing.
  #[test]
  fn balanced_returns_nothing() {
    let a = MockVenue::new("a", "base", "other", 10_000, 20_000, 30);
    let b = MockVenue::new("b", "base", "other", 10_000, 20_000, 30);
    assert!(
      find(&a, &b, &base(), Amount::from(10_000u64))
        .unwrap()
        .is_none()
    );
  }

  /// Venues of different pairs are rejected.
  #[test]
  fn mismatched_pairs_error() {
    let a = MockVenue::new("a", "base", "x", 10_000, 25_000, 30);
    let b = MockVenue::new("b", "base", "y", 10_000, 20_000, 30);
    assert!(matches!(
      find(&a, &b, &base(), Amount::from(10_000u64)),
      Err(Error::TokenNotInPool)
    ));
  }

  /// Each pair yields one result and the more profitable pair comes first.
  #[test]
  fn find_all_one_per_pair_sorted() {
    let venues = vec![
      venue("x1", "x", 25_000),
      venue("x2", "x", 20_000),
      venue("y1", "y", 40_000),
      venue("y2", "y", 20_000),
    ];
    let out = find_all(&venues, &base(), Amount::from(10_000u64), None);
    assert_eq!(out.len(), 2);
    assert_eq!(pools(&out[0]), (id("y1"), id("y2")));
    assert_eq!(pools(&out[1]), (id("x1"), id("x2")));
  }

  /// The best of three venues is picked as the buy and sell pair.
  #[test]
  fn find_all_best_of_three_venues() {
    let venues = vec![
      venue("mid", "x", 20_000),
      venue("rich", "x", 30_000),
      venue("poor", "x", 15_000),
    ];
    let out = find_all(&venues, &base(), Amount::from(10_000u64), None);
    assert_eq!(out.len(), 1);
    assert_eq!(pools(&out[0]), (id("rich"), id("poor")));
  }

  /// A pair with a single venue has nothing to trade against.
  #[test]
  fn find_all_skips_single_venue_pairs() {
    let venues = vec![venue("x1", "x", 25_000)];
    assert!(find_all(&venues, &base(), Amount::from(10_000u64), None).is_empty());
  }

  /// Only pairs with a touched pool are evaluated.
  #[test]
  fn find_all_untouched_pairs_skipped() {
    let venues = vec![
      venue("x1", "x", 25_000),
      venue("x2", "x", 20_000),
      venue("y1", "y", 40_000),
      venue("y2", "y", 20_000),
    ];
    let touched: HashSet<Id> = [id("x2")].into_iter().collect();
    let out = find_all(&venues, &base(), Amount::from(10_000u64), Some(&touched));
    assert_eq!(out.len(), 1);
    assert_eq!(pools(&out[0]), (id("x1"), id("x2")));
  }

  /// A group that cannot quote at all is reported as failed.
  #[test]
  fn find_all_reports_failures() {
    let mut broken = MockVenue::new("x2", "base", "x", 10_000, 20_000, 30);
    broken.fail_above = Some(Amount::ZERO);
    let venues = vec![venue("x1", "x", 25_000), Arc::new(broken)];
    let out = find_all(&venues, &base(), Amount::from(10_000u64), None);
    assert_eq!(out.len(), 1);
    let Evaluation::Failed { buy, sell, error } = &out[0] else {
      panic!("expected Failed");
    };
    assert!(matches!(error, Error::InsufficientLiquidity));
    assert_eq!(buy.pool(), &id("x1"));
    assert_eq!(sell.pool(), &id("x2"));
  }
}
