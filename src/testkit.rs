//! Shared test doubles.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::chain::Chain;
use crate::config::Live;
use crate::error::{Error, Result};
use crate::types::{Amount, Control, Id, Opportunity, Side, Status, Update, Verdict};
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

/// Scriptable chain that records what the bot asks of it.
pub struct MockChain {
  /// Base token id.
  base: Id,
  /// Pools returned by `snapshots`.
  venues: Vec<Arc<dyn Venue>>,
  /// Scripted results for `changes`, front first; an empty script errors.
  updates: Mutex<VecDeque<Result<Update>>>,
  /// Cost per buy pool; pools without an entry cost zero.
  costs: Mutex<HashMap<Id, Option<Amount>>>,
  /// Verdict returned for every submitted opportunity.
  verdict: Mutex<Verdict>,
  /// Every batch passed to `submit`.
  pub submits: Mutex<Vec<Vec<Opportunity>>>,
  /// Every command passed to `control`.
  pub controls: Mutex<Vec<Control>>,
  /// Value returned by `in_flight`.
  pub in_flight: AtomicUsize,
  /// Value returned by `status`.
  pub status: Mutex<Status>,
}

impl MockChain {
  /// Builds a chain over `venues` whose base token id is the UTF8 bytes of `base`.
  pub fn new(base: &str, venues: Vec<Arc<dyn Venue>>) -> Self {
    Self {
      base: Id::new(base.as_bytes().to_vec()),
      venues,
      updates: Mutex::new(VecDeque::new()),
      costs: Mutex::new(HashMap::new()),
      verdict: Mutex::new(Verdict::Sent),
      submits: Mutex::new(Vec::new()),
      controls: Mutex::new(Vec::new()),
      in_flight: AtomicUsize::new(0),
      status: Mutex::new(Status {
        block: 0,
        next_base_fee: 0,
        fee_cap: 0,
        max_fee_per_gas: 0,
        feed_lag_ms: 0,
        executor: String::new(),
        executor_balance: Amount::ZERO,
        treasury: String::new(),
        treasury_balance: Amount::ZERO,
        accounts: Vec::new(),
      }),
    }
  }

  /// Queues the result the next unanswered `changes` call returns.
  pub fn push_update(&self, update: Result<Update>) {
    self.updates.lock().unwrap().push_back(update);
  }

  /// Sets the cost of trades buying on `pool`; `None` means the fee cap is hit.
  pub fn set_cost(&self, pool: &str, cost: Option<Amount>) {
    let pool = Id::new(pool.as_bytes().to_vec());
    self.costs.lock().unwrap().insert(pool, cost);
  }

  /// Sets the verdict `submit` returns for every opportunity.
  pub fn set_verdict(&self, verdict: Verdict) {
    *self.verdict.lock().unwrap() = verdict;
  }
}

impl Chain for MockChain {
  /// Pops the next scripted result, or reports an exhausted script.
  async fn changes(&self) -> Result<Update> {
    let next = self.updates.lock().unwrap().pop_front();
    next.unwrap_or_else(|| Err(Error::Rpc("script exhausted".into())))
  }

  /// Returns the configured pools.
  fn snapshots(&self) -> Vec<Arc<dyn Venue>> {
    self.venues.clone()
  }

  /// Returns the scripted cost of the buy pool, zero when none is set.
  fn cost(&self, op: &Opportunity) -> Option<Amount> {
    let costs = self.costs.lock().unwrap();
    costs
      .get(&op.buy_pool)
      .copied()
      .unwrap_or(Some(Amount::ZERO))
  }

  /// Records the batch and answers every opportunity with the scripted verdict.
  async fn submit(&self, ops: Vec<Opportunity>) -> Vec<(Opportunity, Verdict)> {
    self.submits.lock().unwrap().push(ops.clone());
    let verdict = *self.verdict.lock().unwrap();
    ops.into_iter().map(|op| (op, verdict)).collect()
  }

  /// Ignores live settings.
  fn apply_live(&self, _live: &Live) {}

  /// Records the command.
  fn control(&self, command: Control) {
    self.controls.lock().unwrap().push(command);
  }

  /// Returns the settable in flight count.
  fn in_flight(&self) -> usize {
    self.in_flight.load(Ordering::SeqCst)
  }

  /// Returns the base token id.
  fn base(&self) -> Id {
    self.base.clone()
  }

  /// Returns the settable status.
  fn status(&self) -> Status {
    self.status.lock().unwrap().clone()
  }
}
