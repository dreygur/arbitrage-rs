//! Fee and gas limit math for the trade engine: the EIP 1559 base fee forecast, the fee cap
//! that leaves room for a bumped cancel, and gas limits per family pair learned from receipts.

use crate::chain::evm::config::{EvmConfig, Family};
use crate::types::Amount;

/// EIP 1559 elasticity: the gas target is half the block gas limit.
const ELASTICITY: u64 = 2;
/// EIP 1559 bound on the base fee change per block, `1 / 8`.
const BASE_FEE_CHANGE_DENOMINATOR: u64 = 8;
/// Gas limit headroom over the highest `gasUsed` seen, as `6 / 5` (20%).
const HEADROOM: (u64, u64) = (6, 5);
/// Gas limit raise after an out of gas receipt, as `3 / 2`.
const OUT_OF_GAS_RAISE: (u64, u64) = (3, 2);
/// Replacement fee bump nodes require, as `11 / 10` (10%).
const BUMP: (u128, u128) = (11, 10);
/// The share a call frame keeps back from the call it makes under EIP 150, `1 / 64`.
const OUT_OF_GAS_SLACK: u64 = 64;
/// Nested frames whose `1 / 64` a revert may leave unused and still count as out of gas.
///
/// Two match what the fork measured: trades that ran out of gas left 2.7% of their limit
/// unused (243,206 of 250,000, 272,944 of 280,000, 292,025 of 300,000), and two frames allow
/// 3.1%. More would also catch a trade that ran both legs and then reverted `NoProfit`: the
/// learned limit is only 20% over a post-refund `gasUsed`, a revert gets no refund, so such a
/// trade can use 94% of the limit, and counting it as out of gas would raise the limit by 1.5
/// for good on every stale trade.
const CALL_DEPTH: u32 = 2;

/// The base fee of the block after one with `parent_base_fee`, `gas_used` and `gas_limit`,
/// by the EIP 1559 formula: a full block raises it 12.5%, an empty one lowers it 12.5%.
/// It saturates rather than failing: an absurd forecast can only push `fee_per_gas` to
/// `None`, which refuses sends.
pub fn next_base_fee(parent_base_fee: u128, gas_used: u64, gas_limit: u64) -> u128 {
  let target = gas_limit / ELASTICITY;
  if target == 0 || gas_used == target {
    return parent_base_fee;
  }
  let delta = gas_used.abs_diff(target);
  let change = Amount::from(parent_base_fee) * Amount::from(delta)
    / Amount::from(target)
    / Amount::from(BASE_FEE_CHANGE_DENOMINATOR);
  let change = u128::try_from(change).unwrap_or(u128::MAX);
  if gas_used > target {
    parent_base_fee.saturating_add(change.max(1))
  } else {
    parent_base_fee.saturating_sub(change)
  }
}

/// A transaction's fee cap: `min(2 * next_base_fee + priority_fee, max_fee_per_gas * 10 / 11)`,
/// so a cancel bumped 10% over it still fits under `max_fee_per_gas`. Saturating is safe:
/// the result never exceeds the ceiling.
pub fn fee_cap(next_base_fee: u128, evm: &EvmConfig) -> u128 {
  let ceiling = Amount::from(evm.max_fee_per_gas) * Amount::from(BUMP.1) / Amount::from(BUMP.0);
  let ceiling = u128::try_from(ceiling).unwrap_or(u128::MAX);
  next_base_fee
    .saturating_mul(2)
    .saturating_add(evm.priority_fee)
    .min(ceiling)
}

/// The fee per gas a transaction pays next block, `next_base_fee + priority_fee`, or `None`
/// when that exceeds [`fee_cap`] and nothing may be sent.
pub fn fee_per_gas(next_base_fee: u128, evm: &EvmConfig) -> Option<u128> {
  next_base_fee
    .checked_add(evm.priority_fee)
    .filter(|fee| *fee <= fee_cap(next_base_fee, evm))
}

/// `fee` raised by the 10% a replacement needs, rounded up. It saturates at `u128::MAX`,
/// unreachable for a fee that [`fee_cap`] bounded.
pub fn bump(fee: u128) -> u128 {
  let raised = (Amount::from(fee) * Amount::from(BUMP.0)).div_ceil(Amount::from(BUMP.1));
  u128::try_from(raised).unwrap_or(u128::MAX)
}

/// Gas cost of `gas_limit` at `fee` per gas, in wei. A `u64` times a `u128` cannot overflow
/// a `U256`; were it to saturate, the cost would pass any `max_gas_cost` and refuse the send.
pub fn gas_cost(gas_limit: u64, fee: u128) -> Amount {
  Amount::from(gas_limit).saturating_mul(Amount::from(fee))
}

/// Whether a reverted transaction that used `gas_used` of `gas_limit` ran out of gas: it used
/// all but at most what [`CALL_DEPTH`] nested frames hold back, `1 / 64` of what each had.
pub fn out_of_gas(gas_used: u64, gas_limit: u64) -> bool {
  let reachable = (0..CALL_DEPTH).fold(gas_limit, |gas, _| gas - gas / OUT_OF_GAS_SLACK);
  gas_used >= reachable
}

/// `value * ratio.0 / ratio.1`, rounded down and saturating at `u64::MAX`, which the
/// `max_gas_limit` cap applied after it brings back down.
fn scale(value: u64, ratio: (u64, u64)) -> u64 {
  let scaled = u128::from(value) * u128::from(ratio.0) / u128::from(ratio.1);
  u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Index of a family pair in [`GasLimits`] and `Published.gas_limits`: V2 and V2, V2 and V4
/// in either order, V4 and V4.
pub fn pair_index(a: Family, b: Family) -> usize {
  match (a, b) {
    (Family::V2, Family::V2) => 0,
    (Family::V4, Family::V4) => 2,
    _ => 1,
  }
}

/// Gas limits per family pair, starting at the adapter defaults and tracking receipts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GasLimits {
  /// Current limit per family pair.
  limits: [u64; 3],
  /// Highest `gasUsed` of a successful trade per family pair.
  highest: [u64; 3],
  /// Ceiling on every limit.
  max: u64,
}

impl GasLimits {
  /// Limits from the adapter defaults, each capped at `max_gas_limit`.
  pub fn new(evm: &EvmConfig) -> Self {
    let max = evm.max_gas_limit;
    Self {
      limits: [evm.gas_v2_v2, evm.gas_v2_v4, evm.gas_v4_v4].map(|limit| limit.min(max)),
      highest: [0; 3],
      max,
    }
  }

  /// Every limit, in [`pair_index`] order.
  pub fn limits(&self) -> [u64; 3] {
    self.limits
  }

  /// The limit for family pair `pair`.
  pub fn limit(&self, pair: usize) -> u64 {
    self.limits[pair]
  }

  /// Learns from a trade receipt of family pair `pair` sent with `gas_limit`.
  ///
  /// An out of gas revert raises the limit to 1.5 times the one that ran out (never
  /// lowering it, so two such receipts for one limit do not compound). A success that used
  /// more than any before sets the limit to 20% over it. Other reverts teach nothing, since
  /// a trade that reverts early uses less gas than one that completes. Both are capped.
  pub fn record(&mut self, pair: usize, gas_used: u64, gas_limit: u64, reverted: bool) {
    if reverted {
      if out_of_gas(gas_used, gas_limit) {
        let raised = scale(gas_limit, OUT_OF_GAS_RAISE).max(self.limits[pair]);
        self.limits[pair] = raised.min(self.max);
      }
      return;
    }
    if gas_used > self.highest[pair] {
      self.highest[pair] = gas_used;
      self.limits[pair] = scale(gas_used, HEADROOM).min(self.max);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testkit::mock_evm_config;

  /// One gwei in wei.
  const GWEI: u128 = 1_000_000_000;

  /// A full block raises the base fee 12.5%, an empty one lowers it 12.5%, a block at
  /// target keeps it, and the rise is at least one wei.
  #[test]
  fn next_base_fee_follows_eip1559() {
    let base = 80 * GWEI;
    assert_eq!(next_base_fee(base, 30_000_000, 30_000_000), 90 * GWEI);
    assert_eq!(next_base_fee(base, 0, 30_000_000), 70 * GWEI);
    assert_eq!(next_base_fee(base, 15_000_000, 30_000_000), base);
    assert_eq!(next_base_fee(7, 15_000_001, 30_000_000), 8);
    assert_eq!(next_base_fee(base, 0, 0), base);
  }

  /// Nothing may be sent once `next_base_fee + priority_fee` passes `max_fee_per_gas * 10 / 11`.
  #[test]
  fn fee_cap_blocks_sending() {
    let evm = mock_evm_config();
    let ceiling = evm.max_fee_per_gas * 10 / 11;
    assert_eq!(fee_cap(10 * GWEI, &evm), 21 * GWEI);
    assert_eq!(fee_cap(30 * GWEI, &evm), ceiling);
    assert_eq!(fee_per_gas(10 * GWEI, &evm), Some(11 * GWEI));
    let edge = ceiling - evm.priority_fee;
    assert_eq!(fee_per_gas(edge, &evm), Some(ceiling));
    assert_eq!(fee_per_gas(edge + 1, &evm), None);
    assert_eq!(fee_per_gas(u128::MAX, &evm), None);
  }

  /// Wherever sending is allowed, the cap and tip bumped 10% stay under `max_fee_per_gas`.
  #[test]
  fn cancel_fits_under_cap() {
    let evm = mock_evm_config();
    assert_eq!(bump(100), 110);
    assert_eq!(bump(101), 112);
    for next in (0..=50 * GWEI).step_by(GWEI as usize / 7) {
      if fee_per_gas(next, &evm).is_none() {
        continue;
      }
      let cap = fee_cap(next, &evm);
      assert!(bump(cap) <= evm.max_fee_per_gas, "next {next}");
      assert!(bump(evm.priority_fee) <= bump(cap), "next {next}");
      assert!(bump(cap) * 10 >= cap * 11, "next {next}");
    }
  }

  /// The limit follows the highest successful `gasUsed` plus 20%, and a lower one leaves it.
  #[test]
  fn gas_cache_tracks_highest_used() {
    let mut gas = GasLimits::new(&mock_evm_config());
    assert_eq!(gas.limits(), [250_000, 300_000, 350_000]);
    gas.record(0, 200_000, 250_000, false);
    assert_eq!(gas.limit(0), 240_000);
    gas.record(0, 150_000, 240_000, false);
    assert_eq!(gas.limit(0), 240_000);
    gas.record(0, 100_000, 240_000, true);
    assert_eq!(gas.limit(0), 240_000);
    gas.record(0, 220_000, 240_000, false);
    assert_eq!(gas.limit(0), 264_000);
    assert_eq!(gas.limits()[1..], [300_000, 350_000]);
  }

  /// Neither headroom nor an out of gas raise lifts a limit above `max_gas_limit`.
  #[test]
  fn gas_limit_never_above_max() {
    let mut evm = mock_evm_config();
    evm.gas_v4_v4 = 2_000_000;
    let mut gas = GasLimits::new(&evm);
    assert_eq!(gas.limit(2), 1_000_000);
    gas.record(1, 900_000, 1_000_000, false);
    assert_eq!(gas.limit(1), 1_000_000);
    gas.record(0, 1_000_000, 1_000_000, true);
    assert_eq!(gas.limit(0), 1_000_000);
  }

  /// An out of gas receipt raises that pair's limit 1.5 times, once per limit that ran out.
  #[test]
  fn out_of_gas_raises_limit() {
    let mut gas = GasLimits::new(&mock_evm_config());
    gas.record(0, 249_000, 250_000, true);
    assert_eq!(gas.limit(0), 375_000);
    gas.record(0, 250_000, 250_000, true);
    assert_eq!(gas.limit(0), 375_000);
    gas.record(0, 370_000, 375_000, true);
    assert_eq!(gas.limit(0), 562_500);
    assert_eq!(gas.limit(1), 300_000);
  }

  /// A revert that ran out of gas several call frames deep counts as out of gas, though each
  /// frame held back its 1/64: the fork receipt of a V2 against V2 trade used 243,206 of
  /// 250,000. A revert well short of the limit still teaches nothing.
  #[test]
  fn nested_out_of_gas_raises_limit() {
    assert!(out_of_gas(243_206, 250_000));
    assert!(!out_of_gas(220_000, 250_000));
    let mut gas = GasLimits::new(&mock_evm_config());
    gas.record(0, 220_000, 250_000, true);
    assert_eq!(gas.limit(0), 250_000);
    gas.record(0, 243_206, 250_000, true);
    assert_eq!(gas.limit(0), 375_000);
  }

  /// Every out of gas receipt the fork gave still counts as out of gas: V2 against V2 at
  /// 250,000 and 280,000, V2 against V4 at 300,000.
  #[test]
  fn fork_out_of_gas_receipts_detected() {
    for (used, limit) in [(243_206, 250_000), (272_944, 280_000), (292_025, 300_000)] {
      assert!(out_of_gas(used, limit), "{used} of {limit}");
    }
  }

  /// A trade that reverts after running both legs, as a stale `NoProfit` does, at the limit
  /// learned from a success leaves that limit alone. The limit is 20% over the post-refund
  /// `gasUsed` of the fork's V2 against V4 success, 239,869, and a revert earns no refund,
  /// so it spends close to the full execution: 270,000 here, 93.8% of the limit.
  #[test]
  fn full_execution_revert_keeps_learned_limit() {
    let mut gas = GasLimits::new(&mock_evm_config());
    gas.record(1, 239_869, 300_000, false);
    let learned = gas.limit(1);
    assert_eq!(learned, 287_842);
    gas.record(1, 270_000, learned, true);
    assert_eq!(gas.limit(1), learned);
  }
}
