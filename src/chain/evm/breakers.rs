//! Circuit breakers: a per pool cooldown after repeated reverts and a global stop on revert gas.

use std::collections::{HashMap, VecDeque};

use crate::config::BreakerConfig;
use crate::types::{Amount, Id};

/// Revert history of one pool and the block its cooldown ends at.
#[derive(Debug, Default)]
struct PoolHistory {
  /// Blocks of the reverts still inside the window, oldest first.
  reverts: VecDeque<u64>,
  /// First block at which the pool trades again; zero when not cooling down.
  cooldown_until: u64,
}

/// Tracks reverts per pool and in total, and says whether a pool may be traded.
#[derive(Debug)]
pub struct Breakers {
  /// Thresholds the breakers act on.
  cfg: BreakerConfig,
  /// Revert history of every pool that reverted recently or is cooling down.
  pools: HashMap<Id, PoolHistory>,
  /// Block and gas cost of every revert still inside the window, oldest first.
  reverts: VecDeque<(u64, Amount)>,
  /// Set when revert gas passed the cap; only [`Breakers::resume`] clears it.
  open: bool,
}

/// Whether an event at `at` is still inside the window ending at `block`.
fn in_window(at: u64, block: u64, window: u64) -> bool {
  block.saturating_sub(at) < window
}

impl Breakers {
  /// Creates breakers with everything closed.
  pub fn new(cfg: BreakerConfig) -> Self {
    Self {
      cfg,
      pools: HashMap::new(),
      reverts: VecDeque::new(),
      open: false,
    }
  }

  /// Drops history that fell out of the window or whose cooldown is over, so memory stays bounded.
  fn prune(&mut self, block: u64) {
    let window = self.cfg.revert_window;
    while self
      .reverts
      .front()
      .is_some_and(|(at, _)| !in_window(*at, block, window))
    {
      self.reverts.pop_front();
    }
    self.pools.retain(|_, history| {
      while history
        .reverts
        .front()
        .is_some_and(|at| !in_window(*at, block, window))
      {
        history.reverts.pop_front();
      }
      !history.reverts.is_empty() || history.cooldown_until > block
    });
  }

  /// Records the outcome of a trade across `pools` at `block`; a revert costs `gas_cost`.
  pub fn record(&mut self, pools: [&Id; 2], block: u64, reverted: bool, gas_cost: Amount) {
    self.prune(block);
    if !reverted {
      return;
    }
    for id in pools {
      let history = self.pools.entry(id.clone()).or_default();
      history.reverts.push_back(block);
      if history.reverts.len() >= self.cfg.max_reverts as usize {
        history.cooldown_until = block.saturating_add(self.cfg.cooldown_blocks);
      }
    }
    self.reverts.push_back((block, gas_cost));
    let spent = self
      .reverts
      .iter()
      .fold(Amount::ZERO, |sum, (_, gas)| sum.saturating_add(*gas));
    if spent > self.cfg.max_revert_gas {
      self.open = true;
    }
  }

  /// Whether `pool` may be traded at `block`: false while it cools down or the global breaker is open.
  pub fn allows(&self, pool: &Id, block: u64) -> bool {
    !self.open
      && self
        .pools
        .get(pool)
        .is_none_or(|history| block >= history.cooldown_until)
  }

  /// Whether the global breaker is open.
  pub fn is_open(&self) -> bool {
    self.open
  }

  /// Closes the global breaker and forgets the revert gas that opened it.
  pub fn resume(&mut self) {
    self.open = false;
    self.reverts.clear();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn cfg() -> BreakerConfig {
    BreakerConfig {
      max_reverts: 3,
      revert_window: 10,
      cooldown_blocks: 5,
      max_revert_gas: Amount::from(100u64),
    }
  }

  fn id(n: u8) -> Id {
    Id::new(vec![n; 20])
  }

  fn gas(n: u64) -> Amount {
    Amount::from(n)
  }

  #[test]
  fn pool_cooldown_after_reverts() {
    let (a, b) = (id(1), id(2));
    let mut breakers = Breakers::new(cfg());
    breakers.record([&a, &b], 1, true, gas(1));
    breakers.record([&a, &b], 2, true, gas(1));
    assert!(breakers.allows(&a, 2));
    breakers.record([&a, &id(3)], 3, true, gas(1));
    assert!(!breakers.allows(&a, 3));
    assert!(breakers.allows(&id(3), 3));
    assert!(!breakers.is_open());
  }

  #[test]
  fn cooldown_expires() {
    let a = id(1);
    let mut breakers = Breakers::new(cfg());
    for block in 1..=3 {
      breakers.record([&a, &a], block, true, gas(1));
    }
    assert!(!breakers.allows(&a, 7));
    assert!(breakers.allows(&a, 8));
    breakers.record([&id(9), &id(9)], 40, false, gas(0));
    assert!(breakers.pools.is_empty());
    assert!(breakers.reverts.is_empty());
  }

  #[test]
  fn success_does_not_count() {
    let a = id(1);
    let mut breakers = Breakers::new(cfg());
    for block in 1..=5 {
      breakers.record([&a, &a], block, false, gas(1000));
    }
    assert!(breakers.allows(&a, 5));
    assert!(!breakers.is_open());
  }

  #[test]
  fn global_breaker_opens_on_revert_gas() {
    let mut breakers = Breakers::new(cfg());
    breakers.record([&id(1), &id(2)], 1, true, gas(60));
    assert!(!breakers.is_open());
    breakers.record([&id(3), &id(4)], 2, true, gas(41));
    assert!(breakers.is_open());
    assert!(!breakers.allows(&id(5), 2));
  }

  #[test]
  fn revert_gas_outside_window_ignored() {
    let mut breakers = Breakers::new(cfg());
    breakers.record([&id(1), &id(2)], 1, true, gas(90));
    breakers.record([&id(3), &id(4)], 11, true, gas(90));
    assert!(!breakers.is_open());
    breakers.record([&id(3), &id(4)], 12, true, gas(11));
    assert!(breakers.is_open());
  }

  #[test]
  fn resume_closes_global() {
    let mut breakers = Breakers::new(cfg());
    breakers.record([&id(1), &id(2)], 1, true, gas(101));
    assert!(breakers.is_open());
    breakers.resume();
    assert!(!breakers.is_open());
    assert!(breakers.allows(&id(5), 1));
    breakers.record([&id(1), &id(2)], 2, true, gas(50));
    assert!(!breakers.is_open());
  }
}
