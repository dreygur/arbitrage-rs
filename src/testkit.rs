//! Shared test doubles.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, Bytes};
use alloy::providers::mock::Asserter;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol_types::SolCall;

use crate::chain::Chain;
use crate::chain::evm::bindings::{IMulticall3, Result3};
use crate::chain::evm::config::{EvmConfig, EvmVenue, Family};
use crate::config::Live;
use crate::error::{Error, Result};
use crate::types::{Amount, Control, Id, Opportunity, Side, Status, Update, Verdict};
use crate::venue::Venue;

pub use crate::chain::evm::venue::v4::{v4_fixture, v4_fixture_quoting};

/// Fee denominator in basis points.
const BPS: u64 = 10_000;

/// Provider that answers each request with the next response queued on `asserter`.
pub fn mock_provider(asserter: &Asserter) -> impl Provider + Clone + use<> {
  ProviderBuilder::new().connect_mocked_client(asserter.clone())
}

/// Adapter settings with one V2 venue (`uniswap_v2`, factory `0x55..55`, 30 bps) and one V4
/// venue (`uniswap_v4`, PoolManager `0x44..44`, one tick word), StateView `0x33..33` and
/// Multicall3 `0xca..ca`.
pub fn mock_evm_config() -> EvmConfig {
  EvmConfig {
    chain_id: 1,
    pool_manager: Address::repeat_byte(0x44),
    state_view: Address::repeat_byte(0x33),
    multicall: Address::repeat_byte(0xca),
    priority_fee: 1_000_000_000,
    max_fee_per_gas: 50_000_000_000,
    gas_v2_v2: 250_000,
    gas_v2_v4: 300_000,
    gas_v4_v4: 350_000,
    max_gas_limit: 1_000_000,
    venues: vec![
      EvmVenue {
        name: "uniswap_v2".into(),
        family: Family::V2,
        address: Address::repeat_byte(0x55),
        fee_bps: Some(30),
        tick_words: None,
      },
      EvmVenue {
        name: "uniswap_v4".into(),
        family: Family::V4,
        address: Address::repeat_byte(0x44),
        fee_bps: None,
        tick_words: Some(1),
      },
    ],
    fork: None,
  }
}

/// Queues one Multicall3 `aggregate3` answer: `Some(data)` is a call that returned `data`,
/// `None` one that reverted.
pub fn push_multicall(asserter: &Asserter, results: Vec<Option<Vec<u8>>>) {
  let results: Vec<Result3> = results
    .into_iter()
    .map(|data| Result3 {
      success: data.is_some(),
      returnData: data.unwrap_or_default().into(),
    })
    .collect();
  let encoded = IMulticall3::aggregate3Call::abi_encode_returns(&results);
  asserter.push_success(&Bytes::from(encoded));
}

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
  /// Scripted replies, one per `submit` call, front first. Each entry pairs an index into the
  /// submitted batch with its verdict, in the order returned; omitted indices get no reply.
  replies: Mutex<VecDeque<Vec<(usize, Verdict)>>>,
  /// Every batch passed to `submit`.
  pub submits: Mutex<Vec<Vec<Opportunity>>>,
  /// Number of `cost` calls.
  pub cost_calls: AtomicUsize,
  /// Every command passed to `control`.
  pub controls: Mutex<Vec<Control>>,
  /// Value returned by `in_flight`, shareable so a test can hold it for a chain the code
  /// under test builds.
  pub in_flight: Arc<AtomicUsize>,
  /// Value returned by `status`, shareable like `in_flight`.
  pub status: Arc<Mutex<Status>>,
  /// Every live setting passed to `apply_live`.
  pub lives: Mutex<Vec<Live>>,
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
      replies: Mutex::new(VecDeque::new()),
      submits: Mutex::new(Vec::new()),
      cost_calls: AtomicUsize::new(0),
      controls: Mutex::new(Vec::new()),
      in_flight: Arc::new(AtomicUsize::new(0)),
      lives: Mutex::new(Vec::new()),
      status: Arc::new(Mutex::new(Status {
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
      })),
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

  /// Queues the reply of the next unanswered `submit` call as `(batch index, verdict)` pairs.
  pub fn push_reply(&self, reply: Vec<(usize, Verdict)>) {
    self.replies.lock().unwrap().push_back(reply);
  }

  /// Sets the fallback verdict `submit` returns for every opportunity when no reply is queued.
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
    self.cost_calls.fetch_add(1, Ordering::SeqCst);
    let costs = self.costs.lock().unwrap();
    costs
      .get(&op.buy_pool)
      .copied()
      .unwrap_or(Some(Amount::ZERO))
  }

  /// Records the batch and answers every opportunity with the scripted verdict.
  async fn submit(&self, ops: Vec<Opportunity>) -> Vec<(Opportunity, Verdict)> {
    self.submits.lock().unwrap().push(ops.clone());
    if let Some(reply) = self.replies.lock().unwrap().pop_front() {
      return reply
        .into_iter()
        .map(|(index, verdict)| (ops[index].clone(), verdict))
        .collect();
    }
    let verdict = *self.verdict.lock().unwrap();
    ops.into_iter().map(|op| (op, verdict)).collect()
  }

  /// Records the live settings.
  fn apply_live(&self, live: &Live) {
    self.lives.lock().unwrap().push(live.clone());
  }

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
