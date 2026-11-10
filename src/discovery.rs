//! Liquidity filter that picks the token pairs worth tracking from discovered pools.

use std::collections::{BTreeMap, HashSet};

use crate::types::{Amount, Id};

/// A pool found by discovery, with the data the filter needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolInfo {
  /// Index of the venue that lists the pool.
  pub venue: usize,
  /// Pool identifier.
  pub pool: Id,
  /// Token zero of the pool.
  pub token0: Id,
  /// Token one of the pool.
  pub token1: Id,
  /// Reserve of the base token held by the pool.
  pub base_reserve: Amount,
}

/// The token paired with `base` in `pool`, or `None` when the pool does not list `base`.
fn other_token<'a>(pool: &'a PoolInfo, base: &Id) -> Option<&'a Id> {
  if &pool.token0 == base {
    Some(&pool.token1)
  } else if &pool.token1 == base {
    Some(&pool.token0)
  } else {
    None
  }
}

/// Groups the pools that list `base` and hold at least `min_base_reserve` of it by the other token.
///
/// A group is kept when its pools span two or more distinct venues, since a trade needs a buy
/// and a sell venue. One venue may contribute several pools (V4 fee tiers). Groups are sorted
/// by token id.
pub fn select(pools: &[PoolInfo], base: &Id, min_base_reserve: Amount) -> Vec<Vec<PoolInfo>> {
  let mut groups: BTreeMap<&Id, Vec<PoolInfo>> = BTreeMap::new();
  for pool in pools {
    if pool.base_reserve < min_base_reserve {
      continue;
    }
    if let Some(other) = other_token(pool, base) {
      groups.entry(other).or_default().push(pool.clone());
    }
  }
  groups
    .into_values()
    .filter(|group| group.iter().map(|p| p.venue).collect::<HashSet<_>>().len() >= 2)
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn id(name: &str) -> Id {
    Id::new(name.as_bytes().to_vec())
  }

  fn mock_pool(venue: usize, pool: &str, token0: &str, token1: &str, reserve: u64) -> PoolInfo {
    PoolInfo {
      venue,
      pool: id(pool),
      token0: id(token0),
      token1: id(token1),
      base_reserve: Amount::from(reserve),
    }
  }

  #[test]
  fn keeps_pair_on_two_venues() {
    let pools = [
      mock_pool(0, "p0", "base", "x", 100),
      mock_pool(1, "p1", "base", "x", 100),
    ];
    let out = select(&pools, &id("base"), Amount::from(10u64));
    assert_eq!(out, vec![pools.to_vec()]);
  }

  #[test]
  fn drops_pair_on_one_venue() {
    let pools = [mock_pool(0, "p0", "base", "x", 100)];
    assert!(select(&pools, &id("base"), Amount::from(10u64)).is_empty());
  }

  #[test]
  fn drops_shallow_pool() {
    let pools = [
      mock_pool(0, "p0", "base", "x", 100),
      mock_pool(1, "p1", "base", "x", 9),
    ];
    assert!(select(&pools, &id("base"), Amount::from(10u64)).is_empty());
  }

  #[test]
  fn drops_pairs_without_base() {
    let pools = [
      mock_pool(0, "p0", "x", "y", 100),
      mock_pool(1, "p1", "x", "y", 100),
    ];
    assert!(select(&pools, &id("base"), Amount::from(10u64)).is_empty());
  }

  #[test]
  fn base_can_be_either_token() {
    let pools = [
      mock_pool(0, "p0", "base", "x", 100),
      mock_pool(1, "p1", "x", "base", 100),
      mock_pool(0, "q0", "z", "base", 100),
      mock_pool(1, "q1", "base", "z", 100),
    ];
    let out = select(&pools, &id("base"), Amount::from(10u64));
    assert_eq!(out.len(), 2);
    assert_eq!(out[0][0].pool, id("p0"));
    assert_eq!(out[1][0].pool, id("q0"));
  }

  #[test]
  fn keeps_several_v4_tiers() {
    let pools = [
      mock_pool(1, "t500", "base", "x", 100),
      mock_pool(1, "t3000", "base", "x", 100),
      mock_pool(0, "v2", "base", "x", 100),
    ];
    let out = select(&pools, &id("base"), Amount::from(10u64));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len(), 3);
  }

  #[test]
  fn drops_group_of_one_venue_with_several_tiers() {
    let pools = [
      mock_pool(1, "t500", "base", "x", 100),
      mock_pool(1, "t3000", "base", "x", 100),
    ];
    assert!(select(&pools, &id("base"), Amount::from(10u64)).is_empty());
  }
}
