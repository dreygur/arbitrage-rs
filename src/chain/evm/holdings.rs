//! Holding limits: decides when to sweep executor profit and forward operator ETH to the treasury.
//!
//! A pure state machine; the trade engine sends the requests it returns.

use alloy::primitives::{Address, Bytes};
use alloy::sol_types::SolCall;

use crate::chain::evm::bindings::FlashArbitrage;
use crate::config::Limits;
use crate::types::{Amount, EntryKind};

/// Balances read for one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balances {
  /// WETH held by the executor contract.
  pub executor_weth: Amount,
  /// Address and ETH balance of every account, indexed by account index.
  pub accounts: Vec<(Address, Amount)>,
}

/// A transaction the engine should send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxRequest {
  /// Index of the account that sends it.
  pub from: usize,
  /// Recipient of the transaction.
  pub to: Address,
  /// Native value sent.
  pub value: Amount,
  /// Call data; empty for a plain transfer.
  pub data: Bytes,
  /// Ledger kind the transaction is recorded as.
  pub kind: EntryKind,
}

/// Watches balances against the holding limits.
#[derive(Clone, Debug)]
pub struct Holdings {
  /// Thresholds for sweeps and forwards.
  limits: Limits,
  /// Account index of the owner, the only account allowed to sweep.
  owner: usize,
  /// Recipient of forwarded ETH.
  treasury: Address,
  /// The executor contract.
  executor: Address,
  /// Wrapped native token the executor accumulates profit in.
  weth: Address,
}

/// The excess of `balance` over `keep` when `balance` is above `max` and the excess is worth sending.
fn excess(balance: Amount, max: Amount, keep: Amount, min_transfer: Amount) -> Option<Amount> {
  if balance <= max {
    return None;
  }
  balance
    .checked_sub(keep)
    .filter(|amount| *amount >= min_transfer)
}

impl Holdings {
  /// Creates holdings for an owner account, a treasury, the executor and its WETH.
  pub fn new(
    limits: Limits,
    owner: usize,
    treasury: Address,
    executor: Address,
    weth: Address,
  ) -> Self {
    Self {
      limits,
      owner,
      treasury,
      executor,
      weth,
    }
  }

  /// Replaces the limits; the next block uses them.
  pub fn set_limits(&mut self, limits: Limits) {
    self.limits = limits;
  }

  /// The owner's sweep of executor WETH down to `executor_keep`, if due.
  fn sweep(&self, balances: &Balances) -> Option<TxRequest> {
    let limits = &self.limits;
    let amount = excess(
      balances.executor_weth,
      limits.executor_max_hold,
      limits.executor_keep,
      limits.min_transfer,
    )?;
    let call = FlashArbitrage::sweepCall {
      token: self.weth,
      amount,
    };
    Some(TxRequest {
      from: self.owner,
      to: self.executor,
      value: Amount::ZERO,
      data: call.abi_encode().into(),
      kind: EntryKind::Sweep,
    })
  }

  /// Requests the sweeps and forwards due at this block; none when the fee is over the cap.
  pub fn on_block(&self, balances: &Balances, fee_ok: bool) -> Vec<TxRequest> {
    if !fee_ok {
      return Vec::new();
    }
    let limits = &self.limits;
    let forwards = balances
      .accounts
      .iter()
      .enumerate()
      .filter(|(index, _)| *index != self.owner)
      .filter_map(|(index, (_, balance))| {
        let value = excess(
          *balance,
          limits.account_max_hold,
          limits.account_keep,
          limits.min_transfer,
        )?;
        Some(TxRequest {
          from: index,
          to: self.treasury,
          value,
          data: Bytes::new(),
          kind: EntryKind::Forward,
        })
      });
    self.sweep(balances).into_iter().chain(forwards).collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const OWNER: usize = 0;

  fn eth(n: u64) -> Amount {
    Amount::from(n)
  }

  fn limits() -> Limits {
    Limits {
      executor_max_hold: eth(100),
      executor_keep: eth(10),
      account_max_hold: eth(50),
      account_keep: eth(5),
      account_min_gas: eth(2),
      min_transfer: eth(20),
    }
  }

  fn treasury() -> Address {
    Address::repeat_byte(0xaa)
  }

  fn executor() -> Address {
    Address::repeat_byte(0xbb)
  }

  fn weth() -> Address {
    Address::repeat_byte(0xcc)
  }

  fn holdings() -> Holdings {
    Holdings::new(limits(), OWNER, treasury(), executor(), weth())
  }

  fn balances(executor_weth: u64, accounts: [u64; 3]) -> Balances {
    Balances {
      executor_weth: eth(executor_weth),
      accounts: accounts
        .iter()
        .enumerate()
        .map(|(i, balance)| (Address::repeat_byte(i as u8 + 1), eth(*balance)))
        .collect(),
    }
  }

  #[test]
  fn sweep_drops_to_keep() {
    let requests = holdings().on_block(&balances(150, [0, 0, 0]), true);
    let call = FlashArbitrage::sweepCall {
      token: weth(),
      amount: eth(140),
    };
    assert_eq!(
      requests,
      vec![TxRequest {
        from: OWNER,
        to: executor(),
        value: Amount::ZERO,
        data: call.abi_encode().into(),
        kind: EntryKind::Sweep,
      }]
    );
  }

  #[test]
  fn no_sweep_below_cap() {
    assert!(
      holdings()
        .on_block(&balances(100, [0, 0, 0]), true)
        .is_empty()
    );
  }

  #[test]
  fn transfer_below_minimum_skipped() {
    let mut small = limits();
    small.executor_max_hold = eth(100);
    small.executor_keep = eth(95);
    small.account_max_hold = eth(50);
    small.account_keep = eth(40);
    let holdings = Holdings::new(small, OWNER, treasury(), executor(), weth());
    assert!(
      holdings
        .on_block(&balances(105, [0, 51, 51]), true)
        .is_empty()
    );
  }

  #[test]
  fn account_forward_drops_to_keep() {
    let requests = holdings().on_block(&balances(0, [500, 80, 40]), true);
    assert_eq!(
      requests,
      vec![TxRequest {
        from: 1,
        to: treasury(),
        value: eth(75),
        data: Bytes::new(),
        kind: EntryKind::Forward,
      }]
    );
  }

  #[test]
  fn nothing_sent_over_fee_cap() {
    assert!(
      holdings()
        .on_block(&balances(500, [0, 500, 500]), false)
        .is_empty()
    );
  }

  #[test]
  fn live_limits_apply() {
    let mut holdings = holdings();
    let state = balances(150, [0, 60, 0]);
    assert_eq!(holdings.on_block(&state, true).len(), 2);
    let mut raised = limits();
    raised.executor_max_hold = eth(1000);
    raised.account_max_hold = eth(1000);
    holdings.set_limits(raised);
    assert!(holdings.on_block(&state, true).is_empty());
  }
}
