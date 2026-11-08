//! The trade engine: the one task that signs and sends.
//!
//! It keeps a nonce lane per account, locks both pools of every accepted trade, moves each
//! transaction through `Planned -> Sent -> Executed | Reverted | Dropped`, enforces the gas
//! caps, cancels stuck transactions, reads receipts and balances each block, feeds the
//! breakers and the holding limits, and journals every state change.

use std::collections::HashMap;

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, B256, Bytes, TxKind};
use alloy::providers::Provider;
use alloy::rpc::types::{Log, TransactionReceipt};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use alloy::transports::TransportError;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, warn};

use crate::chain::evm::bindings::{FlashArbitrage, IERC20, IMulticall3, IWETH};
use crate::chain::evm::breakers::Breakers;
use crate::chain::evm::config::{EvmConfig, Family};
use crate::chain::evm::convert::{to_address, to_id};
use crate::chain::evm::feed::FeedBlock;
use crate::chain::evm::gas::{
  GasLimits, bump, fee_cap, fee_per_gas, gas_cost, next_base_fee, out_of_gas, pair_index,
};
use crate::chain::evm::holdings::{Balances, Holdings, TxRequest};
use crate::chain::evm::multicall::{aggregate, decode, read};
use crate::chain::evm::pairs::LegSpec;
use crate::config::Live;
use crate::error::{Error, Result};
use crate::journal::JournalTx;
use crate::types::{
  AccountStatus, Amount, Control, EntryKind, Id, Opportunity, Role, Status, TradeDetail,
  TradeEvent, TradeState, Verdict,
};

/// Commands buffered before a sender waits for the engine.
const COMMANDS: usize = 64;
/// Gas limit of a plain transfer: forwards and cancels.
const TRANSFER_GAS: u64 = 21_000;
/// Gas limit of an owner sweep call, a WETH transfer behind an owner check.
const SWEEP_GAS: u64 = 100_000;
/// Error replies (lowercase) by which geth, reth, erigon and anvil say they already hold a
/// transaction.
const KNOWN_REPLIES: [&str; 3] = ["already known", "known transaction", "already imported"];

/// A request to the engine task.
#[derive(Debug)]
pub enum EngineCmd {
  /// Accept or refuse opportunities, best first, reply with one verdict each, then send.
  Submit {
    /// The opportunities, as the bot priced them.
    ops: Vec<Opportunity>,
    /// Gets every op back unchanged with its verdict.
    reply: oneshot::Sender<Vec<(Opportunity, Verdict)>>,
  },
  /// A new head, and the highest block the bot has applied to its pool state.
  Block {
    /// The new block.
    block: FeedBlock,
    /// Highest block number applied to pool state.
    applied: u64,
  },
  /// An operator command.
  Control(Control),
  /// New live settings.
  SetLive(Live),
}

/// What the engine publishes after every command, for readers that must not wait on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
  /// Fee per gas a trade would pay next block, `None` while the fee cap blocks sending.
  pub fee_per_gas: Option<u128>,
  /// Gas limits `[v2_v2, v2_v4, v4_v4]`.
  pub gas_limits: [u64; 3],
  /// Trades planned or sent and not yet mined or dropped.
  pub in_flight: usize,
  /// Block, fees, balances and accounts for the dashboard.
  pub status: Status,
}

/// One account and its local nonce.
#[derive(Debug)]
struct Lane {
  /// Signs this lane's transactions.
  signer: PrivateKeySigner,
  /// Next nonce to use; `None` until read from the node, and then the lane gets no trades.
  nonce: Option<u64>,
  /// ETH balance at the last block read.
  eth: Amount,
}

/// The trade half of a tracked transaction.
#[derive(Clone, Debug)]
struct TradeInfo {
  /// The opportunity, whose two pools stay locked while this entry is kept.
  op: Opportunity,
  /// Family pair index into the gas limits.
  pair: usize,
}

/// A zero value self transfer replacing a stuck transaction at its nonce.
#[derive(Clone, Debug)]
struct Cancel {
  /// Hash of the cancel.
  tx: B256,
  /// The signed cancel, rebroadcast each block until it or the original is mined, since a
  /// mempool may evict it.
  raw: Bytes,
  /// Fee cap, 10% over the original's.
  fee_cap: u128,
  /// Tip, 10% over the original's.
  tip: u128,
}

/// A transaction the engine plans, sends and follows until it is finished and, for a trade,
/// its locks are released.
#[derive(Clone, Debug)]
struct Pending {
  /// Ledger id, shared by the transaction's cancel.
  id: u64,
  /// Trade, sweep or forward.
  kind: EntryKind,
  /// Index of the sending account.
  lane: usize,
  /// Lifecycle stage.
  state: TradeState,
  /// Recipient.
  to: Address,
  /// Native value sent.
  value: Amount,
  /// Call data.
  data: Bytes,
  /// Gas limit.
  gas_limit: u64,
  /// Fee cap per gas.
  fee_cap: u128,
  /// Priority fee per gas.
  tip: u128,
  /// Nonce, set when signed.
  nonce: u64,
  /// Hash once sent.
  tx: Option<B256>,
  /// Block number current when it was sent.
  sent_block: u64,
  /// Block it or its cancel was mined in.
  mined_block: Option<u64>,
  /// Set for a trade.
  trade: Option<TradeInfo>,
  /// Set once a cancel is sent.
  cancel: Option<Cancel>,
  /// Set when the send failed in a way that may still have reached the mempool.
  unconfirmed: bool,
}

impl Pending {
  /// Whether the transaction reached a final state.
  fn finished(&self) -> bool {
    matches!(
      self.state,
      TradeState::Executed | TradeState::Reverted | TradeState::Dropped
    )
  }

  /// Whether this is a trade still waiting to be mined.
  fn trade_in_flight(&self) -> bool {
    self.trade.is_some() && !self.finished()
  }

  /// The unsigned EIP 1559 transaction at `nonce`.
  fn unsigned(&self, chain_id: u64, nonce: u64) -> TxEip1559 {
    TxEip1559 {
      chain_id,
      nonce,
      gas_limit: self.gas_limit,
      max_fee_per_gas: self.fee_cap,
      max_priority_fee_per_gas: self.tip,
      to: TxKind::Call(self.to),
      value: self.value,
      access_list: Default::default(),
      input: self.data.clone(),
    }
  }
}

/// The executor's code for a pool family.
fn family_code(family: Family) -> u8 {
  match family {
    Family::V2 => 0,
    Family::V4 => 1,
  }
}

/// Encodes a leg on `pool` that swaps with `zero_for_one`: V2 data is
/// `abi.encode(pair, zeroForOne, feeBps)`, V4 data `abi.encode(PoolKey, zeroForOne)`. A V4
/// key must hold `token`; a V2 spec names only token zero, so the pair is trusted.
fn encode_leg(
  pool: &Id,
  spec: &LegSpec,
  token: Address,
  zero_for_one: bool,
) -> Result<FlashArbitrage::Leg> {
  let missing = |what: &str| Error::Config(format!("pool {pool} has no {what}"));
  let data = match spec.family {
    Family::V2 => {
      let pair = spec.pair.ok_or_else(|| missing("pair address"))?;
      (pair, zero_for_one, spec.fee_bps).abi_encode_params()
    }
    Family::V4 => {
      let key = spec.key.ok_or_else(|| missing("pool key"))?;
      if key.currency0 != token && key.currency1 != token {
        return Err(Error::TokenNotInPool);
      }
      (key.pool_key(), zero_for_one).abi_encode_params()
    }
  };
  Ok(FlashArbitrage::Leg {
    family: family_code(spec.family),
    data: data.into(),
  })
}

/// The buy and sell legs of `op`: the buy leg swaps base for `op.token` on `buy_pool`, the
/// sell leg swaps `op.token` for base on `sell_pool`, each with `zeroForOne` set when its
/// input is the pool's token zero (native ETH being the zero address).
pub fn build_legs(
  op: &Opportunity,
  legs: &HashMap<Id, LegSpec>,
) -> Result<(FlashArbitrage::Leg, FlashArbitrage::Leg)> {
  let token = to_address(&op.token)?;
  let spec = |pool: &Id| {
    legs
      .get(pool)
      .ok_or_else(|| Error::Config(format!("no leg spec for pool {pool}")))
  };
  let (buy, sell) = (spec(&op.buy_pool)?, spec(&op.sell_pool)?);
  Ok((
    encode_leg(&op.buy_pool, buy, token, buy.token0 != token)?,
    encode_leg(&op.sell_pool, sell, token, sell.token0 == token)?,
  ))
}

/// Net WETH the executor gained across `logs`: transfers and deposits to it less transfers
/// and withdrawals from it, which is every way WETH9 moves a balance. `None` when it lost.
pub fn weth_gain(logs: &[Log], weth: Address, executor: Address) -> Option<Amount> {
  let mut gained = Amount::ZERO;
  let mut spent = Amount::ZERO;
  for log in logs.iter().filter(|log| log.address() == weth) {
    let data = log.data();
    let (into, out_of, value) = match log.topic0() {
      Some(&IERC20::Transfer::SIGNATURE_HASH) => match IERC20::Transfer::decode_log_data(data) {
        Ok(e) => (e.to == executor, e.from == executor, e.value),
        Err(_) => continue,
      },
      Some(&IWETH::Deposit::SIGNATURE_HASH) => match IWETH::Deposit::decode_log_data(data) {
        Ok(e) => (e.dst == executor, false, e.wad),
        Err(_) => continue,
      },
      Some(&IWETH::Withdrawal::SIGNATURE_HASH) => match IWETH::Withdrawal::decode_log_data(data) {
        Ok(e) => (false, e.src == executor, e.wad),
        Err(_) => continue,
      },
      _ => continue,
    };
    if into {
      gained = gained.checked_add(value)?;
    }
    if out_of {
      spent = spent.checked_add(value)?;
    }
  }
  gained.checked_sub(spent)
}

/// Whether a failed `eth_sendRawTransaction` is a definite rejection: an error reply from the
/// node, other than one saying it already holds the transaction. A timeout, a lost
/// connection, an unreadable reply or "already known" may mean the transaction reached the
/// mempool, so it must be followed rather than dropped.
fn rejected(error: &TransportError) -> bool {
  match error {
    TransportError::ErrorResp(payload) => {
      let message = payload.message.to_lowercase();
      !KNOWN_REPLIES.iter().any(|known| message.contains(known))
    }
    _ => false,
  }
}

/// Signs `tx`, returning the raw envelope and its hash.
fn sign(signer: &PrivateKeySigner, mut tx: TxEip1559) -> Result<(Bytes, B256)> {
  let signature = signer
    .sign_transaction_sync(&mut tx)
    .map_err(|e| Error::Contract(format!("signing failed: {e}")))?;
  let envelope = TxEnvelope::from(tx.into_signed(signature));
  Ok((envelope.encoded_2718().into(), *envelope.tx_hash()))
}

/// The engine state, owned by its task once spawned.
pub struct Engine<P> {
  /// Node connection for sends, receipts, nonces and balances.
  provider: P,
  /// One lane per account, by account index.
  lanes: Vec<Lane>,
  /// Account index of the owner, which only sweeps and cancels its own.
  owner: usize,
  /// Adapter settings: chain id, fees, gas defaults, multicall.
  evm: EvmConfig,
  /// Live settings.
  live: Live,
  /// Per pool and global circuit breakers.
  breakers: Breakers,
  /// Holding limits.
  holdings: Holdings,
  /// The executor contract.
  executor: Address,
  /// How to encode a leg on each tracked pool.
  legs: HashMap<Id, LegSpec>,
  /// Journal writer.
  journal: JournalTx,
  /// Event feed for the dashboard, never waited on.
  events: mpsc::Sender<TradeEvent>,
  /// Gas limits per family pair.
  gas: GasLimits,
  /// Latest block number seen.
  block: u64,
  /// Highest block the bot applied.
  applied: u64,
  /// Base fee forecast for the next block, unknown before the first head.
  next_base_fee: Option<u128>,
  /// Set by `Control::Pause`.
  paused: bool,
  /// Transactions in progress and finished trades still holding locks, in id order.
  pending: Vec<Pending>,
  /// Next ledger id.
  next_id: u64,
  /// Executor WETH at the last read.
  executor_balance: Amount,
  /// Treasury WETH at the last read.
  treasury_balance: Amount,
}

impl<P: Provider> Engine<P> {
  /// Builds an engine over `signers`, account `owner` being the owner; nothing is read from
  /// the node until it is spawned.
  ///
  /// Ledger ids start at `first_id`, which the caller seeds from the journal (the highest id
  /// it holds plus one) so ids stay unique across restarts within the journal's lifetime.
  #[allow(
    clippy::too_many_arguments,
    reason = "the engine owns every piece the chain hands it at connect"
  )]
  pub fn new(
    provider: P,
    signers: Vec<PrivateKeySigner>,
    owner: usize,
    evm: EvmConfig,
    live: Live,
    breakers: Breakers,
    holdings: Holdings,
    executor: Address,
    legs: HashMap<Id, LegSpec>,
    journal: JournalTx,
    events: mpsc::Sender<TradeEvent>,
    first_id: u64,
  ) -> Self {
    let lanes = signers
      .into_iter()
      .map(|signer| Lane {
        signer,
        nonce: None,
        eth: Amount::ZERO,
      })
      .collect();
    Self {
      provider,
      lanes,
      owner,
      gas: GasLimits::new(&evm),
      evm,
      live,
      breakers,
      holdings,
      executor,
      legs,
      journal,
      events,
      block: 0,
      applied: 0,
      next_base_fee: None,
      paused: false,
      pending: Vec::new(),
      next_id: first_id,
      executor_balance: Amount::ZERO,
      treasury_balance: Amount::ZERO,
    }
  }

  /// Fee per gas a transaction pays next block, `None` when unknown or over the cap.
  fn fee(&self) -> Option<u128> {
    self
      .next_base_fee
      .and_then(|next| fee_per_gas(next, &self.evm))
  }

  /// Fee cap for a transaction planned now.
  fn cap(&self) -> u128 {
    fee_cap(self.next_base_fee.unwrap_or(0), &self.evm)
  }

  /// Whether `gas_limit` at `fee` stays within `max_gas_cost`. Callers pass the fee cap a
  /// transaction is signed with, the most it can cost.
  fn within_cost(&self, gas_limit: u64, fee: u128) -> bool {
    gas_cost(gas_limit, fee) <= self.live.max_gas_cost
  }

  /// Trades in flight on `lane`.
  fn lane_in_flight(&self, lane: usize) -> usize {
    self
      .pending
      .iter()
      .filter(|p| p.lane == lane && p.trade_in_flight())
      .count()
  }

  /// Id of the trade holding a lock on `pool`.
  fn lock_holder(&self, pool: &Id) -> Option<u64> {
    self
      .pending
      .iter()
      .find(|p| {
        p.trade
          .as_ref()
          .is_some_and(|t| t.op.buy_pool == *pool || t.op.sell_pool == *pool)
      })
      .map(|p| p.id)
  }

  /// Whether `lane` has a cancel out that is not mined, so its later nonces wait behind a
  /// stuck one and a new trade there would only time out too.
  fn lane_stuck(&self, lane: usize) -> bool {
    self
      .pending
      .iter()
      .any(|p| p.lane == lane && p.cancel.is_some() && !p.finished())
  }

  /// The synced, unstuck operator lane at or above `account_min_gas` with the fewest trades
  /// in flight, the lowest index on a tie; never the owner.
  fn operator_lane(&self) -> Option<usize> {
    let min = self.live.limits.account_min_gas;
    (0..self.lanes.len())
      .filter(|&i| i != self.owner && self.lanes[i].nonce.is_some() && self.lanes[i].eth >= min)
      .filter(|&i| !self.lane_stuck(i))
      .min_by_key(|&i| (self.lane_in_flight(i), i))
  }

  /// The verdict on `op` at `block`, planning it when accepted.
  fn admit(&mut self, op: &Opportunity, fee: Option<u128>, block: u64) -> Verdict {
    if self.paused {
      return Verdict::Paused;
    }
    if fee.is_none() {
      return Verdict::GasCap;
    }
    let (buy, sell) = match build_legs(op, &self.legs) {
      Ok(legs) => legs,
      Err(e) => {
        warn!(buy_pool = %op.buy_pool, sell_pool = %op.sell_pool, error = %e, "legs not encodable");
        return Verdict::QuoteError;
      }
    };
    let families = (self.legs.get(&op.buy_pool), self.legs.get(&op.sell_pool));
    let (Some(buy_spec), Some(sell_spec)) = families else {
      return Verdict::QuoteError;
    };
    let pair = pair_index(buy_spec.family, sell_spec.family);
    let gas_limit = self.gas.limit(pair);
    let fee_cap = self.cap();
    if !self.within_cost(gas_limit, fee_cap) {
      return Verdict::GasCap;
    }
    let holder = [&op.buy_pool, &op.sell_pool]
      .into_iter()
      .find_map(|pool| self.lock_holder(pool));
    if let Some(id) = holder {
      return Verdict::Locked(id);
    }
    if !self.breakers.allows(&op.buy_pool, block) || !self.breakers.allows(&op.sell_pool, block) {
      return Verdict::CircuitOpen;
    }
    let Some(lane) = self.operator_lane() else {
      return Verdict::LowGas;
    };
    let call = FlashArbitrage::executeCall {
      buy,
      sell,
      amount: op.amount,
      minProfit: op.floor,
    };
    self.pending.push(Pending {
      id: self.next_id,
      kind: EntryKind::Trade,
      lane,
      state: TradeState::Planned,
      to: self.executor,
      value: Amount::ZERO,
      data: call.abi_encode().into(),
      gas_limit,
      fee_cap,
      tip: self.evm.priority_fee,
      nonce: 0,
      tx: None,
      sent_block: block,
      mined_block: None,
      trade: Some(TradeInfo {
        op: op.clone(),
        pair,
      }),
      cancel: None,
      unconfirmed: false,
    });
    self.next_id += 1;
    Verdict::Sent
  }

  /// Accepts or refuses `ops` best first: `Paused` while paused, `GasCap` when the fee cap
  /// blocks sending or the gas limit at the fee cap passes `max_gas_cost`, `Locked(id)` when a
  /// pool is held
  /// by trade `id` (an earlier op of this batch included), `CircuitOpen` when a breaker
  /// refuses a pool, `LowGas` when no operator lane can send. Accepted ops become `Planned`
  /// and lock both pools. Every op comes back unchanged.
  pub fn accept(&mut self, mut ops: Vec<Opportunity>, block: u64) -> Vec<(Opportunity, Verdict)> {
    ops.sort_by_key(|op| std::cmp::Reverse(op.profit));
    let fee = self.fee();
    ops
      .into_iter()
      .map(|op| {
        let verdict = self.admit(&op, fee, block);
        (op, verdict)
      })
      .collect()
  }

  /// The detail every entry of `p` carries: its gas settings and, for a trade, the op.
  fn base_detail(&self, p: &Pending) -> TradeDetail {
    let mut detail = TradeDetail {
      gas_limit: Some(p.gas_limit),
      fee_cap: Some(p.fee_cap),
      tip: Some(p.tip),
      ..Default::default()
    };
    if let Some(trade) = &p.trade {
      let op = &trade.op;
      detail.pair = Some((to_id(self.holdings.weth()), op.token.clone()));
      detail.buy_pool = Some(op.buy_pool.clone());
      detail.sell_pool = Some(op.sell_pool.clone());
      detail.amount = Some(op.amount);
      detail.quoted_profit = Some(op.profit);
      detail.floor = Some(op.floor);
    }
    detail
  }

  /// A ledger entry for `p` in `state`, recorded as `kind` with hash `tx`.
  fn event(&self, p: &Pending, kind: EntryKind, state: TradeState, tx: Option<B256>) -> TradeEvent {
    TradeEvent {
      id: p.id,
      kind,
      state,
      block: self.block,
      account: self.lanes[p.lane].signer.address().to_string(),
      nonce: p.nonce,
      tx: tx.map(|hash| hash.to_string()),
      detail: self.base_detail(p),
    }
  }

  /// Journals `event`, waiting if the journal is behind, and offers it to the dashboard
  /// without waiting.
  async fn emit(&self, event: TradeEvent) {
    self.journal.record(event.clone()).await;
    if self.events.try_send(event).is_err() {
      debug!("event channel full or closed, dashboard event skipped");
    }
  }

  /// Reads `lane`'s nonce at `block` from the node, `None` when the read fails.
  async fn nonce_at(&self, lane: usize, block: BlockId) -> Option<u64> {
    let address = self.lanes[lane].signer.address();
    match self
      .provider
      .get_transaction_count(address)
      .block_id(block)
      .await
    {
      Ok(nonce) => Some(nonce),
      Err(e) => {
        warn!(lane, error = %e, "nonce read failed");
        None
      }
    }
  }

  /// Reads `lane`'s pending nonce from the node; on failure the lane waits for a later sync.
  async fn resync(&mut self, lane: usize) {
    self.lanes[lane].nonce = self.nonce_at(lane, BlockId::pending()).await;
  }

  /// Marks the entry at `index` dropped before it reached the node, returning its planned
  /// and dropped entries.
  fn drop_unsent(&mut self, index: usize) -> Vec<TradeEvent> {
    self.pending[index].state = TradeState::Dropped;
    let p = &self.pending[index];
    vec![
      self.event(p, p.kind, TradeState::Planned, None),
      self.event(p, p.kind, TradeState::Dropped, None),
    ]
  }

  /// Signs and sends the planned entry at `index` with its lane's next nonce, returning the
  /// entries to journal. A lane without a known nonce is resynced first, so after a send
  /// error only that lane's next send waits on a nonce read and the entry signs fresh.
  ///
  /// A definite rejection drops the entry. Any other failure keeps it `Sent` under the hash
  /// computed locally, locks held, flagged unconfirmed until a block shows whether the node
  /// has it. Either way the lane's nonce is read again before its next send.
  async fn send_one(&mut self, index: usize) -> Vec<TradeEvent> {
    let lane = self.pending[index].lane;
    let id = self.pending[index].id;
    if self.lanes[lane].nonce.is_none() {
      self.resync(lane).await;
    }
    let Some(nonce) = self.lanes[lane].nonce else {
      warn!(lane, id, "lane nonce unknown, transaction dropped");
      return self.drop_unsent(index);
    };
    self.pending[index].nonce = nonce;
    let unsigned = self.pending[index].unsigned(self.evm.chain_id, nonce);
    let (raw, hash) = match sign(&self.lanes[lane].signer, unsigned) {
      Ok(signed) => signed,
      Err(e) => {
        warn!(lane, id, error = %e, "signing failed, transaction dropped");
        return self.drop_unsent(index);
      }
    };
    let outcome = self.provider.send_raw_transaction(&raw).await;
    if let Err(e) = &outcome
      && rejected(e)
    {
      warn!(lane, nonce, id, error = %e, "send rejected, transaction dropped");
      self.lanes[lane].nonce = None;
      return self.drop_unsent(index);
    }
    let p = &mut self.pending[index];
    p.tx = Some(hash);
    p.state = TradeState::Sent;
    p.sent_block = self.block;
    match outcome {
      Ok(_) => self.lanes[lane].nonce = nonce.checked_add(1),
      Err(e) => {
        warn!(lane, nonce, id, %hash, error = %e, "send outcome unknown, following the hash");
        p.unconfirmed = true;
        self.lanes[lane].nonce = None;
      }
    }
    let p = &self.pending[index];
    vec![
      self.event(p, p.kind, TradeState::Planned, None),
      self.event(p, p.kind, TradeState::Sent, Some(hash)),
    ]
  }

  /// Sends every planned entry in id order, so each lane's nonces are consecutive, then
  /// journals them. Journaling after the whole batch keeps the sends back to back, at the
  /// price that a crash between a send and the journal write loses those lines.
  ///
  /// Accepted delay: lanes interleave in id order, so after a rejected send on one lane,
  /// that lane's next entry resyncs inline and any later entry of another lane in the batch
  /// waits behind that one nonce read. It takes a failed send and two or more trades on a
  /// lane in one batch; reordering the batch around it is not worth the complexity.
  async fn send_planned(&mut self) {
    let mut events = Vec::new();
    for index in 0..self.pending.len() {
      if self.pending[index].state == TradeState::Planned {
        events.extend(self.send_one(index).await);
      }
    }
    for event in events {
      self.emit(event).await;
    }
  }

  /// Forgets finished entries; a mined trade keeps its locks until the bot applied its block.
  fn release(&mut self) {
    let applied = self.applied;
    self
      .pending
      .retain(|p| !(p.finished() && p.mined_block.is_none_or(|mined| applied >= mined)));
  }

  /// The receipt of `tx`, `Ok(None)` while unmined. A failed read is an error, never taken
  /// for "not mined", since a lagging or failing backend would then let a mined entry look
  /// replaced.
  async fn receipt(&self, tx: B256) -> Result<Option<TransactionReceipt>> {
    self
      .provider
      .get_transaction_receipt(tx)
      .await
      .map_err(|e| {
        warn!(%tx, error = %e, "receipt read failed");
        Error::from(e)
      })
  }

  /// Settles the entry at `index` from its own receipt, teaching the gas limits and the
  /// breakers when it is a trade; a cancel it beat is journaled as dropped.
  async fn finish(&mut self, index: usize, receipt: TransactionReceipt) {
    let mined = receipt.block_number.unwrap_or(self.block);
    let ok = receipt.status();
    let (gas_used, price) = (receipt.gas_used, receipt.effective_gas_price);
    let p = &mut self.pending[index];
    p.state = if ok {
      TradeState::Executed
    } else {
      TradeState::Reverted
    };
    p.mined_block = Some(mined);
    if let Some(trade) = &p.trade {
      self.gas.record(trade.pair, gas_used, p.gas_limit, !ok);
      let pools = [&trade.op.buy_pool, &trade.op.sell_pool];
      self
        .breakers
        .record(pools, mined, !ok, gas_cost(gas_used, price));
    }
    let p = &self.pending[index];
    let mut event = self.event(p, p.kind, p.state, p.tx);
    event.detail.mined_block = Some(mined);
    event.detail.gas_used = Some(gas_used);
    event.detail.effective_gas_price = Some(price);
    if ok && p.trade.is_some() {
      event.detail.actual_profit = weth_gain(receipt.logs(), self.holdings.weth(), self.executor);
    }
    if !ok {
      let reason = if out_of_gas(gas_used, p.gas_limit) {
        "out of gas"
      } else {
        "execution reverted"
      };
      event.detail.revert = Some(reason.into());
    }
    let lost = p
      .cancel
      .as_ref()
      .map(|cancel| self.event(p, EntryKind::Cancel, TradeState::Dropped, Some(cancel.tx)));
    self.emit(event).await;
    if let Some(lost) = lost {
      self.emit(lost).await;
    }
  }

  /// Settles the entry at `index` as dropped because its cancel was mined.
  async fn cancelled(&mut self, index: usize, receipt: TransactionReceipt) {
    let mined = receipt.block_number.unwrap_or(self.block);
    let p = &mut self.pending[index];
    p.state = TradeState::Dropped;
    p.mined_block = Some(mined);
    let p = &self.pending[index];
    let Some(cancel) = &p.cancel else {
      return;
    };
    let mut won = self.event(p, EntryKind::Cancel, TradeState::Executed, Some(cancel.tx));
    won.detail = TradeDetail {
      gas_limit: Some(TRANSFER_GAS),
      fee_cap: Some(cancel.fee_cap),
      tip: Some(cancel.tip),
      mined_block: Some(mined),
      gas_used: Some(receipt.gas_used),
      effective_gas_price: Some(receipt.effective_gas_price),
      ..Default::default()
    };
    let dropped = self.event(p, p.kind, TradeState::Dropped, p.tx);
    self.emit(won).await;
    self.emit(dropped).await;
  }

  /// Settles the entry at `index` as dropped because another transaction took its nonce:
  /// neither it nor its cancel has a receipt, yet the nonce is used. Its locks are freed
  /// once, its cancel is journaled as dropped too, and its lane is resynced before its next
  /// send.
  fn replaced(&mut self, index: usize) -> Vec<TradeEvent> {
    let block = self.block;
    let p = &mut self.pending[index];
    warn!(
      id = p.id,
      lane = p.lane,
      nonce = p.nonce,
      "nonce taken by another transaction, dropped"
    );
    p.state = TradeState::Dropped;
    p.mined_block = Some(block);
    self.lanes[p.lane].nonce = None;
    let p = &self.pending[index];
    let mut events = vec![self.event(p, p.kind, TradeState::Dropped, p.tx)];
    if let Some(cancel) = &p.cancel {
      events.push(self.event(p, EntryKind::Cancel, TradeState::Dropped, Some(cancel.tx)));
    }
    events
  }

  /// Settles whether the unconfirmed entry at `index` reached the node: known by hash, it
  /// is followed like any sent entry. It is dropped as never received only once
  /// `trade_timeout` blocks have passed since it was sent, its hash is still unknown and the
  /// lane's pending nonce has not passed its nonce. Before that an unknown hash may be node
  /// lag or slow propagation, so it stays `Sent` with its locks held and the next block asks
  /// again.
  async fn confirm(&mut self, index: usize) -> Vec<TradeEvent> {
    let p = &self.pending[index];
    let (Some(tx), lane, nonce) = (p.tx, p.lane, p.nonce) else {
      return Vec::new();
    };
    let timed_out = self.block.saturating_sub(p.sent_block) > self.live.trade_timeout;
    match self.provider.get_transaction_by_hash(tx).await {
      Ok(Some(_)) => {
        self.pending[index].unconfirmed = false;
        return Vec::new();
      }
      Ok(None) if timed_out => {}
      Ok(None) => return Vec::new(),
      Err(e) => {
        warn!(%tx, error = %e, "transaction read failed");
        return Vec::new();
      }
    }
    let pending = self.nonce_at(lane, BlockId::pending()).await;
    if !pending.is_some_and(|pending| pending <= nonce) {
      return Vec::new();
    }
    warn!(
      id = self.pending[index].id,
      lane, nonce, "node never took the transaction, dropped"
    );
    self.pending[index].state = TradeState::Dropped;
    self.lanes[lane].nonce = None;
    let p = &self.pending[index];
    vec![self.event(p, p.kind, TradeState::Dropped, p.tx)]
  }

  /// Reads the receipt of every sent entry, and of its cancel while the original has none.
  ///
  /// An entry with neither receipt that is unconfirmed or past `trade_timeout` has its
  /// lane's nonce read at this block (once per lane): a nonce past the entry's means another
  /// transaction took it, which no receipt we follow will ever show. An unconfirmed entry
  /// still holding its nonce is then checked by hash. In a block where a receipt read for
  /// the entry or its cancel failed, neither check runs; the next block reads again.
  async fn check_receipts(&mut self) {
    let mut used_nonces: HashMap<usize, Option<u64>> = HashMap::new();
    let mut events = Vec::new();
    for index in 0..self.pending.len() {
      let p = &self.pending[index];
      if p.state != TradeState::Sent {
        continue;
      }
      let (Some(tx), cancel) = (p.tx, p.cancel.as_ref().map(|cancel| cancel.tx)) else {
        continue;
      };
      let original = self.receipt(tx).await;
      if let Ok(Some(receipt)) = original {
        self.finish(index, receipt).await;
        continue;
      }
      let mut unread = original.is_err();
      if let Some(cancel) = cancel {
        match self.receipt(cancel).await {
          Ok(Some(receipt)) => {
            self.cancelled(index, receipt).await;
            continue;
          }
          Ok(None) => {}
          Err(_) => unread = true,
        }
      }
      if unread {
        continue;
      }
      let p = &self.pending[index];
      let timed_out = self.block.saturating_sub(p.sent_block) > self.live.trade_timeout;
      if !p.unconfirmed && !timed_out {
        continue;
      }
      let (lane, nonce) = (p.lane, p.nonce);
      let used = match used_nonces.get(&lane) {
        Some(used) => *used,
        None => {
          let used = self.nonce_at(lane, BlockId::number(self.block)).await;
          used_nonces.insert(lane, used);
          used
        }
      };
      if used.is_some_and(|used| used > nonce) {
        events.extend(self.replaced(index));
      } else if self.pending[index].unconfirmed {
        events.extend(self.confirm(index).await);
      }
    }
    for event in events {
      self.emit(event).await;
    }
  }

  /// Replaces the stuck entry at `index` with a zero value self transfer at its nonce, fee
  /// cap and tip bumped 10%, which stays under `max_fee_per_gas` by the fee cap's design.
  /// A rejected send is retried next block; a send whose outcome is unknown counts as sent,
  /// since the rebroadcast each block delivers it either way.
  async fn send_cancel(&mut self, index: usize) {
    let p = &self.pending[index];
    let (fee_cap, tip) = (bump(p.fee_cap), bump(p.tip));
    if !self.within_cost(TRANSFER_GAS, fee_cap) {
      warn!(id = p.id, "cancel gas cost above max_gas_cost, not sent");
      return;
    }
    let signer = &self.lanes[p.lane].signer;
    let unsigned = TxEip1559 {
      chain_id: self.evm.chain_id,
      nonce: p.nonce,
      gas_limit: TRANSFER_GAS,
      max_fee_per_gas: fee_cap,
      max_priority_fee_per_gas: tip,
      to: TxKind::Call(signer.address()),
      value: Amount::ZERO,
      access_list: Default::default(),
      input: Bytes::new(),
    };
    let (raw, hash) = match sign(signer, unsigned) {
      Ok(signed) => signed,
      Err(e) => {
        warn!(id = p.id, error = %e, "cancel signing failed, retrying next block");
        return;
      }
    };
    if let Err(e) = self.provider.send_raw_transaction(&raw).await {
      if rejected(&e) {
        warn!(id = self.pending[index].id, error = %e, "cancel rejected, retrying next block");
        return;
      }
      warn!(id = self.pending[index].id, error = %e, "cancel send outcome unknown, rebroadcasting");
    }
    let cancel = Cancel {
      tx: hash,
      raw,
      fee_cap,
      tip,
    };
    self.pending[index].cancel = Some(cancel);
    let p = &self.pending[index];
    let detail = TradeDetail {
      gas_limit: Some(TRANSFER_GAS),
      fee_cap: Some(fee_cap),
      tip: Some(tip),
      ..Default::default()
    };
    let mut planned = self.event(p, EntryKind::Cancel, TradeState::Planned, None);
    planned.detail = detail.clone();
    let mut sent = self.event(p, EntryKind::Cancel, TradeState::Sent, Some(hash));
    sent.detail = detail;
    self.emit(planned).await;
    self.emit(sent).await;
  }

  /// Cancels every entry sent more than `trade_timeout` blocks ago and rebroadcasts every
  /// cancel already out, unless the fee cap blocks sending, in which case the next block
  /// tries again. A rebroadcast needs no new bump: the cancel already sits at the most the
  /// fee cap allows, and "already known" replies are expected.
  async fn send_cancels(&mut self) {
    if self.fee().is_none() {
      return;
    }
    for index in 0..self.pending.len() {
      let p = &self.pending[index];
      let stuck = self.block.saturating_sub(p.sent_block) > self.live.trade_timeout;
      if p.state != TradeState::Sent || !stuck {
        continue;
      }
      match &p.cancel {
        None => self.send_cancel(index).await,
        Some(cancel) => {
          if let Err(e) = self.provider.send_raw_transaction(&cancel.raw).await {
            debug!(id = p.id, error = %e, "cancel rebroadcast not taken");
          }
        }
      }
    }
  }

  /// Reads executor and treasury WETH and every account's ETH at `block` in one multicall,
  /// storing them; `None` when the read fails.
  async fn read_balances(&mut self, block: BlockId) -> Option<Balances> {
    let weth = self.holdings.weth();
    let mut calls = vec![
      read(
        weth,
        &IERC20::balanceOfCall {
          account: self.executor,
        },
      ),
      read(
        weth,
        &IERC20::balanceOfCall {
          account: self.holdings.treasury(),
        },
      ),
    ];
    calls.extend(self.lanes.iter().map(|lane| {
      read(
        self.evm.multicall,
        &IMulticall3::getEthBalanceCall {
          addr: lane.signer.address(),
        },
      )
    }));
    let decoded = aggregate(&self.provider, self.evm.multicall, block, calls)
      .await
      .and_then(|results| {
        let executor = decode::<IERC20::balanceOfCall>(&results[0])?;
        let treasury = decode::<IERC20::balanceOfCall>(&results[1])?;
        let eth = results[2..]
          .iter()
          .map(decode::<IMulticall3::getEthBalanceCall>)
          .collect::<Result<Vec<_>>>()?;
        Ok((executor, treasury, eth))
      });
    let (executor, treasury, eth) = match decoded {
      Ok(values) => values,
      Err(e) => {
        warn!(error = %e, "balance read failed");
        return None;
      }
    };
    self.executor_balance = executor;
    self.treasury_balance = treasury;
    for (lane, eth) in self.lanes.iter_mut().zip(eth) {
      lane.eth = eth;
    }
    Some(Balances {
      executor_weth: executor,
      accounts: self
        .lanes
        .iter()
        .map(|lane| (lane.signer.address(), lane.eth))
        .collect(),
    })
  }

  /// Plans a sweep or forward, returning whether it was planned: not while the fee cap
  /// blocks sending, while one of its kind is still in progress on that lane, or when its
  /// gas limit at the fee cap it would be signed with passes `max_gas_cost`.
  fn queue(&mut self, request: TxRequest) -> bool {
    if self.fee().is_none() {
      warn!(kind = ?request.kind, "fee over the cap, transfer not sent");
      return false;
    }
    if request.from >= self.lanes.len() {
      warn!(from = request.from, "request from an unknown account");
      return false;
    }
    let busy = self
      .pending
      .iter()
      .any(|p| p.lane == request.from && p.kind == request.kind && !p.finished());
    if busy {
      debug!(from = request.from, kind = ?request.kind, "previous transfer still in progress");
      return false;
    }
    let gas_limit = if request.data.is_empty() {
      TRANSFER_GAS
    } else {
      SWEEP_GAS
    };
    let fee_cap = self.cap();
    if !self.within_cost(gas_limit, fee_cap) {
      warn!(kind = ?request.kind, "transfer gas cost above max_gas_cost, not sent");
      return false;
    }
    self.pending.push(Pending {
      id: self.next_id,
      kind: request.kind,
      lane: request.from,
      state: TradeState::Planned,
      to: request.to,
      value: request.value,
      data: request.data,
      gas_limit,
      fee_cap,
      tip: self.evm.priority_fee,
      nonce: 0,
      tx: None,
      sent_block: self.block,
      mined_block: None,
      trade: None,
      cancel: None,
      unconfirmed: false,
    });
    self.next_id += 1;
    true
  }

  /// Reads the nonce of every lane that has none.
  async fn sync_lanes(&mut self) {
    for lane in 0..self.lanes.len() {
      if self.lanes[lane].nonce.is_none() {
        self.resync(lane).await;
      }
    }
  }

  /// Handles a new head: forecasts the base fee, reads receipts, releases locks, cancels
  /// stuck entries, reads balances and sends the transfers the holding limits ask for.
  async fn on_block(&mut self, block: FeedBlock, applied: u64) {
    self.block = block.number;
    self.applied = self.applied.max(applied);
    self.next_base_fee = Some(next_base_fee(
      block.base_fee,
      block.gas_used,
      block.gas_limit,
    ));
    self.sync_lanes().await;
    self.check_receipts().await;
    self.release();
    self.send_cancels().await;
    let fee = self.fee();
    let Some(balances) = self.read_balances(BlockId::number(block.number)).await else {
      return;
    };
    for request in self.holdings.on_block(&balances, fee.is_some()) {
      self.queue(request);
    }
    self.send_planned().await;
    self.release();
  }

  /// Sweeps the executor's whole WETH balance, read now, from the owner lane. The operator
  /// asked for it, so a sweep that cannot go out says why at `warn`.
  async fn sweep_all(&mut self) {
    if self.fee().is_none() {
      warn!("fee over the cap, sweep not sent");
      return;
    }
    let Some(balances) = self.read_balances(BlockId::latest()).await else {
      return;
    };
    if balances.executor_weth.is_zero() {
      return;
    }
    let call = FlashArbitrage::sweepCall {
      token: self.holdings.weth(),
      amount: balances.executor_weth,
    };
    let request = TxRequest {
      from: self.owner,
      to: self.executor,
      value: Amount::ZERO,
      data: call.abi_encode().into(),
      kind: EntryKind::Sweep,
    };
    if !self.queue(request) {
      warn!("sweep not sent: a sweep is still in progress or it costs too much gas");
      return;
    }
    self.send_planned().await;
    self.release();
  }

  /// Applies an operator command; resuming also closes the global breaker.
  async fn control(&mut self, command: Control) {
    match command {
      Control::Pause => self.paused = true,
      Control::Resume => {
        self.paused = false;
        self.breakers.resume();
      }
      Control::Sweep => self.sweep_all().await,
    }
  }

  /// Applies new live settings, the holding limits included.
  fn set_live(&mut self, live: Live) {
    self.holdings.set_limits(live.limits.clone());
    self.live = live;
  }

  /// The values readers see.
  fn snapshot(&self) -> Published {
    let min_gas = self.live.limits.account_min_gas;
    let accounts = self
      .lanes
      .iter()
      .enumerate()
      .map(|(index, lane)| AccountStatus {
        address: lane.signer.address().to_string(),
        role: if index == self.owner {
          Role::Owner
        } else {
          Role::Operator
        },
        eth: lane.eth,
        nonce: lane.nonce.unwrap_or(0),
        in_flight: self
          .pending
          .iter()
          .filter(|p| p.lane == index && !p.finished())
          .count(),
        low_gas: lane.eth < min_gas,
      })
      .collect();
    Published {
      fee_per_gas: self.fee(),
      gas_limits: self.gas.limits(),
      in_flight: self.pending.iter().filter(|p| p.trade_in_flight()).count(),
      status: Status {
        block: self.block,
        next_base_fee: self.next_base_fee.unwrap_or(0),
        fee_cap: self.cap(),
        max_fee_per_gas: self.evm.max_fee_per_gas,
        // The engine never sees the feed; `EvmChain::status` fills the lag in.
        feed_lag_ms: 0,
        executor: self.executor.to_string(),
        executor_balance: self.executor_balance,
        treasury: self.holdings.treasury().to_string(),
        treasury_balance: self.treasury_balance,
        accounts,
      },
    }
  }

  /// Runs one command. A submit is answered before its trades are sent, so the bot is not
  /// held up by the sends.
  async fn handle(&mut self, command: EngineCmd) {
    match command {
      EngineCmd::Submit { ops, reply } => {
        let verdicts = self.accept(ops, self.block);
        if reply.send(verdicts).is_err() {
          debug!("submitter gone before the reply");
        }
        self.send_planned().await;
        self.release();
      }
      EngineCmd::Block { block, applied } => self.on_block(block, applied).await,
      EngineCmd::Control(command) => self.control(command).await,
      EngineCmd::SetLive(live) => self.set_live(live),
    }
  }

  /// Starts the engine task: it reads every lane's nonce, then runs commands in order and
  /// publishes after each one. The task ends when every command sender is dropped.
  pub fn spawn(mut self) -> (mpsc::Sender<EngineCmd>, watch::Receiver<Published>)
  where
    P: 'static,
  {
    let (commands, mut receiver) = mpsc::channel(COMMANDS);
    let (published, subscriber) = watch::channel(self.snapshot());
    tokio::spawn(async move {
      self.sync_lanes().await;
      published.send_replace(self.snapshot());
      while let Some(command) = receiver.recv().await {
        self.handle(command).await;
        published.send_replace(self.snapshot());
      }
    });
    (commands, subscriber)
  }
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};
  use std::time::Duration;

  use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
  use alloy::primitives::{Bloom, U64};
  use alloy::providers::mock::Asserter;
  use alloy::signers::local::MnemonicBuilder;
  use alloy::signers::local::coins_bip39::English;
  use alloy::transports::TransportErrorKind;

  use super::*;
  use crate::chain::evm::bindings::PoolKey;
  use crate::chain::evm::venue::v4::V4Key;
  use crate::config::{BreakerConfig, Limits};
  use crate::journal::{Journal, read_last};
  use crate::testkit::{mock_evm_config, mock_provider, push_multicall};

  /// One gwei in wei.
  const GWEI: u128 = 1_000_000_000;
  /// One ether in wei.
  const ETH: u64 = 1_000_000_000_000_000_000;
  /// The anvil test mnemonic.
  const ANVIL: &str = "test test test test test test test test test test test junk";

  /// The non base token of every test pool; below WETH, so token zero of a V2 pair.
  fn token() -> Address {
    Address::repeat_byte(0x77)
  }

  /// The base token.
  fn weth() -> Address {
    Address::repeat_byte(0xee)
  }

  /// The executor contract.
  fn executor() -> Address {
    Address::repeat_byte(0xbb)
  }

  /// The treasury.
  fn treasury() -> Address {
    Address::repeat_byte(0xaa)
  }

  /// Wei as an amount.
  fn wei(n: u64) -> Amount {
    Amount::from(n)
  }

  /// Test pool `n`.
  fn pool(n: u8) -> Id {
    Id::new(vec![n; 20])
  }

  /// A V2 pair at `0xnn..nn` pairing `token()` (token zero) with WETH.
  fn v2_spec(n: u8) -> LegSpec {
    LegSpec {
      family: Family::V2,
      pair: Some(Address::repeat_byte(n)),
      key: None,
      token0: token(),
      fee_bps: 30,
    }
  }

  /// Leg specs for V2 pools 1 to 8.
  fn legs() -> HashMap<Id, LegSpec> {
    (1..=8).map(|n| (pool(n), v2_spec(n))).collect()
  }

  /// Buy on `buy`, sell on `sell`, quoted at `profit`.
  fn op(buy: u8, sell: u8, profit: u64) -> Opportunity {
    Opportunity {
      buy_pool: pool(buy),
      sell_pool: pool(sell),
      token: to_id(token()),
      amount: wei(ETH),
      profit: wei(profit),
      floor: wei(1),
    }
  }

  /// Limits no one ETH balance reaches, so no transfer is due.
  fn limits() -> Limits {
    Limits {
      executor_max_hold: wei(10 * ETH),
      executor_keep: wei(ETH),
      account_max_hold: wei(10 * ETH),
      account_keep: wei(ETH / 2),
      account_min_gas: wei(ETH / 10),
      min_transfer: wei(ETH / 100),
    }
  }

  /// Live settings with a three block timeout.
  fn live() -> Live {
    Live {
      min_profit: wei(1),
      max_trade: wei(ETH) * wei(100),
      max_gas_cost: wei(ETH),
      trade_timeout: 3,
      limits: limits(),
    }
  }

  /// Breakers that open on one revert's worth of gas.
  fn breaker_config() -> BreakerConfig {
    BreakerConfig {
      max_reverts: 3,
      revert_window: 10,
      cooldown_blocks: 5,
      max_revert_gas: wei(1000),
    }
  }

  /// An engine, its mock node and where its events go.
  struct Fixture<P> {
    /// The engine under test.
    engine: Engine<P>,
    /// Queued node answers.
    asserter: Asserter,
    /// Dashboard events.
    events: mpsc::Receiver<TradeEvent>,
    /// Journal file.
    journal: PathBuf,
  }

  /// An engine over the first `accounts` anvil accounts, owner 0, every lane synced at nonce
  /// zero with 1 ETH, at block 10 with a next base fee of 10 gwei, its ledger ids starting at
  /// `first_id`; `events` is the dashboard channel capacity.
  async fn fixture_from(
    name: &str,
    accounts: u32,
    events: usize,
    first_id: u64,
  ) -> Fixture<impl Provider + use<>> {
    let asserter = Asserter::new();
    let signers = (0..accounts)
      .map(|i| {
        MnemonicBuilder::<English>::default()
          .phrase(ANVIL)
          .index(i)
          .unwrap()
          .build()
          .unwrap()
      })
      .collect();
    let dir = std::env::temp_dir().join(format!("engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("trades.jsonl");
    let journal = Journal::spawn(&path).await.unwrap();
    let (events_tx, events_rx) = mpsc::channel(events);
    let holdings = Holdings::new(limits(), 0, treasury(), executor(), weth());
    let mut engine = Engine::new(
      mock_provider(&asserter),
      signers,
      0,
      mock_evm_config(),
      live(),
      Breakers::new(breaker_config()),
      holdings,
      executor(),
      legs(),
      journal,
      events_tx,
      first_id,
    );
    for lane in &mut engine.lanes {
      lane.nonce = Some(0);
      lane.eth = wei(ETH);
    }
    engine.block = 10;
    engine.applied = 10;
    engine.next_base_fee = Some(10 * GWEI);
    Fixture {
      engine,
      asserter,
      events: events_rx,
      journal: path,
    }
  }

  /// [`fixture_from`] with ledger ids starting at 1.
  async fn fixture(name: &str, accounts: u32, events: usize) -> Fixture<impl Provider + use<>> {
    fixture_from(name, accounts, events, 1).await
  }

  /// Queues a nonce read answer.
  fn push_nonce(asserter: &Asserter, nonce: u64) {
    asserter.push_success(&U64::from(nonce));
  }

  /// A head at `number` exactly at its gas target, so the base fee holds at 10 gwei.
  fn head(number: u64) -> FeedBlock {
    FeedBlock {
      number,
      hash: B256::repeat_byte(number as u8),
      base_fee: 10 * GWEI,
      gas_used: 15_000_000,
      gas_limit: 30_000_000,
      logs: Vec::new(),
      gap: false,
    }
  }

  /// Queues a balance multicall answer: no WETH anywhere and `eth` for each of `lanes`.
  fn push_balances(asserter: &Asserter, lanes: usize, eth: u64) {
    let mut results = vec![Some(wei(0).abi_encode()), Some(wei(0).abi_encode())];
    results.extend((0..lanes).map(|_| Some(wei(eth).abi_encode())));
    push_multicall(asserter, results);
  }

  /// Queues an accepted `eth_sendRawTransaction`.
  fn push_sent(asserter: &Asserter) {
    asserter.push_success(&B256::repeat_byte(0x99));
  }

  /// Queues an empty receipt answer, a transaction not mined yet.
  fn push_unmined(asserter: &Asserter) {
    asserter.push_success(&serde_json::Value::Null);
  }

  /// A receipt mined in `block` that succeeded when `ok`, with `gas_used` and `logs`.
  fn receipt(block: u64, ok: bool, gas_used: u64, logs: Vec<Log>) -> TransactionReceipt {
    TransactionReceipt {
      inner: ReceiptEnvelope::Eip1559(ReceiptWithBloom {
        receipt: Receipt {
          status: Eip658Value::Eip658(ok),
          cumulative_gas_used: gas_used,
          logs,
        },
        logs_bloom: Bloom::default(),
      }),
      transaction_hash: B256::repeat_byte(0x99),
      transaction_index: Some(0),
      block_hash: Some(B256::repeat_byte(block as u8)),
      block_number: Some(block),
      gas_used,
      effective_gas_price: 11 * GWEI,
      blob_gas_used: None,
      blob_gas_price: None,
      from: Address::ZERO,
      to: Some(executor()),
      contract_address: None,
    }
  }

  /// Queues a receipt answer.
  fn push_mined(asserter: &Asserter, block: u64, ok: bool, gas_used: u64) {
    asserter.push_success(&receipt(block, ok, gas_used, Vec::new()));
  }

  /// A WETH log of `event` emitted by `address`.
  fn log<E: SolEvent>(address: Address, event: &E) -> Log {
    Log {
      inner: alloy::primitives::Log {
        address,
        data: event.encode_log_data(),
      },
      ..Default::default()
    }
  }

  /// A WETH transfer of `value` from `from` to `to`.
  fn transfer(from: Address, to: Address, value: u64) -> Log {
    log(
      weth(),
      &IERC20::Transfer {
        from,
        to,
        value: wei(value),
      },
    )
  }

  /// Every event queued on the dashboard channel, as `(id, kind, state)`.
  fn drain(events: &mut mpsc::Receiver<TradeEvent>) -> Vec<(u64, EntryKind, TradeState)> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
      out.push((event.id, event.kind, event.state));
    }
    out
  }

  /// Waits until the journal at `path` holds `count` entries and returns them.
  async fn journal_entries(path: &Path, count: usize) -> Vec<TradeEvent> {
    for _ in 0..200 {
      if let Ok((entries, _)) = read_last(path, 100).await
        && entries.len() >= count
      {
        return entries.into_iter().map(|entry| entry.event).collect();
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("journal never reached {count} entries");
  }

  /// In a batch taken best first, an op sharing a pool with a better one is refused with that
  /// trade's id, and every op comes back unchanged.
  #[tokio::test]
  async fn overlapping_ops_keep_the_better() {
    let mut f = fixture("overlap", 3, 16).await;
    let verdicts = f
      .engine
      .accept(vec![op(1, 2, 5), op(2, 3, 9), op(4, 5, 1)], 10);
    assert_eq!(
      verdicts,
      vec![
        (op(2, 3, 9), Verdict::Sent),
        (op(1, 2, 5), Verdict::Locked(1)),
        (op(4, 5, 1), Verdict::Sent),
      ]
    );
    assert!(
      f.engine
        .pending
        .iter()
        .all(|p| p.state == TradeState::Planned)
    );
  }

  /// A locked pool is refused whichever account would send, even with idle lanes left.
  #[tokio::test]
  async fn locked_pool_refused_for_every_account() {
    let mut f = fixture("locked", 4, 16).await;
    assert_eq!(f.engine.accept(vec![op(1, 2, 9)], 10)[0].1, Verdict::Sent);
    let verdicts = f.engine.accept(vec![op(2, 3, 9), op(4, 1, 8)], 10);
    assert_eq!(verdicts[0].1, Verdict::Locked(1));
    assert_eq!(verdicts[1].1, Verdict::Locked(1));
    assert_eq!(f.engine.lane_in_flight(2), 0);
    assert_eq!(f.engine.lane_in_flight(3), 0);
  }

  /// A mined trade keeps its locks until the bot reports its block applied.
  #[tokio::test]
  async fn lock_held_until_block_applied() {
    let mut f = fixture("applied", 2, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    push_mined(&f.asserter, 11, true, 200_000);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(11), 10).await;
    assert_eq!(f.engine.pending[0].state, TradeState::Executed);
    assert_eq!(
      f.engine.accept(vec![op(1, 3, 9)], 11)[0].1,
      Verdict::Locked(1)
    );
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(12), 11).await;
    assert_eq!(f.engine.accept(vec![op(1, 3, 9)], 12)[0].1, Verdict::Sent);
    assert!(f.asserter.read_q().is_empty());
  }

  /// A trade unmined after `trade_timeout` blocks gets one cancel at its nonce with fee cap
  /// and tip bumped 10%, under `max_fee_per_gas`.
  #[tokio::test]
  async fn timeout_sends_bumped_cancel() {
    let mut f = fixture("timeout", 2, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    push_unmined(&f.asserter);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(13), 13).await;
    assert!(f.engine.pending[0].cancel.is_none());
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(14), 14).await;
    let trade = &f.engine.pending[0];
    let cancel = trade.cancel.clone().expect("cancel sent");
    assert_eq!(trade.state, TradeState::Sent);
    assert_eq!(trade.nonce, 0);
    assert_eq!(cancel.fee_cap, bump(trade.fee_cap));
    assert_eq!(cancel.tip, bump(trade.tip));
    assert!(cancel.fee_cap <= f.engine.evm.max_fee_per_gas);
    assert_eq!(f.engine.lanes[1].nonce, Some(1));
    push_unmined(&f.asserter);
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    f.asserter.push_failure_msg("already known");
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(15), 15).await;
    assert_eq!(
      f.engine.pending[0].cancel.as_ref().map(|c| c.tx),
      Some(cancel.tx)
    );
    assert!(f.asserter.read_q().is_empty());
    let events = drain(&mut f.events);
    assert!(events.contains(&(1, EntryKind::Cancel, TradeState::Sent)));
    assert_eq!(
      events
        .iter()
        .filter(|e| e.1 == EntryKind::Cancel && e.2 == TradeState::Sent)
        .count(),
      1
    );
  }

  /// A cancelled trade keeps its locks until the original or the cancel is mined.
  #[tokio::test]
  async fn locks_held_until_cancel_or_original_mined() {
    let mut f = fixture("cancel-locks", 3, 64).await;
    f.engine.accept(vec![op(1, 2, 9), op(3, 4, 8)], 10);
    push_sent(&f.asserter);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    for _ in 0..2 {
      push_unmined(&f.asserter);
      push_nonce(&f.asserter, 0);
    }
    push_sent(&f.asserter);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 3, ETH);
    f.engine.on_block(head(14), 14).await;
    assert!(f.engine.pending.iter().all(|p| p.cancel.is_some()));
    for _ in 0..2 {
      push_unmined(&f.asserter);
      push_unmined(&f.asserter);
      push_nonce(&f.asserter, 0);
    }
    push_sent(&f.asserter);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 3, ETH);
    f.engine.on_block(head(15), 15).await;
    let verdicts = f.engine.accept(vec![op(1, 5, 9), op(3, 6, 8)], 15);
    assert_eq!(verdicts[0].1, Verdict::Locked(1));
    assert_eq!(verdicts[1].1, Verdict::Locked(2));
    push_mined(&f.asserter, 16, true, 200_000);
    push_unmined(&f.asserter);
    push_mined(&f.asserter, 16, true, 21_000);
    push_balances(&f.asserter, 3, ETH);
    f.engine.on_block(head(16), 16).await;
    assert!(f.engine.pending.is_empty());
    let verdicts = f.engine.accept(vec![op(1, 5, 9), op(3, 6, 8)], 16);
    assert_eq!(verdicts[0].1, Verdict::Sent);
    assert_eq!(verdicts[1].1, Verdict::Sent);
    let events = drain(&mut f.events);
    assert!(events.contains(&(1, EntryKind::Trade, TradeState::Executed)));
    assert!(events.contains(&(1, EntryKind::Cancel, TradeState::Dropped)));
    assert!(events.contains(&(2, EntryKind::Cancel, TradeState::Executed)));
    assert!(events.contains(&(2, EntryKind::Trade, TradeState::Dropped)));
    assert!(f.asserter.read_q().is_empty());
  }

  /// A send error drops that trade, resyncs the lane nonce, and the lane's next trade is
  /// signed with the fresh nonce.
  #[tokio::test]
  async fn send_error_resigns_later_nonces() {
    let mut f = fixture("resign", 2, 16).await;
    f.engine.lanes[1].nonce = Some(5);
    f.engine.accept(vec![op(1, 2, 9), op(3, 4, 8)], 10);
    f.asserter.push_failure_msg("nonce too low");
    f.asserter.push_success(&U64::from(7));
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    assert_eq!(f.engine.pending[0].state, TradeState::Dropped);
    assert_eq!(f.engine.pending[0].nonce, 5);
    assert_eq!(f.engine.pending[1].state, TradeState::Sent);
    assert_eq!(f.engine.pending[1].nonce, 7);
    assert_eq!(f.engine.lanes[1].nonce, Some(8));
    f.engine.release();
    assert_eq!(f.engine.accept(vec![op(1, 2, 9)], 10)[0].1, Verdict::Sent);
    assert!(f.asserter.read_q().is_empty());
  }

  /// Trades go to the operator lane with the fewest in flight, the lowest index on a tie.
  #[tokio::test]
  async fn least_busy_operator_chosen() {
    let mut f = fixture("least-busy", 4, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    f.engine
      .accept(vec![op(3, 4, 9), op(5, 6, 8), op(7, 8, 7)], 10);
    let lanes: Vec<usize> = f.engine.pending.iter().map(|p| p.lane).collect();
    assert_eq!(lanes, vec![1, 2, 3, 1]);
  }

  /// A lane below `account_min_gas` gets no trades, and with none left the op is `LowGas`.
  #[tokio::test]
  async fn low_gas_account_skipped() {
    let mut f = fixture("low-gas", 3, 16).await;
    f.engine.lanes[1].eth = wei(ETH / 20);
    f.engine.accept(vec![op(1, 2, 9), op(3, 4, 8)], 10);
    let lanes: Vec<usize> = f.engine.pending.iter().map(|p| p.lane).collect();
    assert_eq!(lanes, vec![2, 2]);
    f.engine.lanes[2].eth = Amount::ZERO;
    assert_eq!(f.engine.accept(vec![op(5, 6, 9)], 10)[0].1, Verdict::LowGas);
  }

  /// The owner never sends trades, wherever it sits in the account list.
  #[tokio::test]
  async fn owner_is_not_an_operator_lane() {
    let mut f = fixture("owner-only", 1, 16).await;
    f.engine.lanes[0].eth = wei(ETH) * wei(100);
    assert_eq!(f.engine.accept(vec![op(1, 2, 9)], 10)[0].1, Verdict::LowGas);
    let mut f = fixture("owner-second", 2, 16).await;
    f.engine.owner = 1;
    f.engine.accept(vec![op(1, 2, 9), op(3, 4, 8)], 10);
    let lanes: Vec<usize> = f.engine.pending.iter().map(|p| p.lane).collect();
    assert_eq!(lanes, vec![0, 0]);
  }

  /// A base fee over the cap, or a gas cost over `max_gas_cost` at the fee cap the trade
  /// would be signed with, refuses with `GasCap`.
  #[tokio::test]
  async fn fee_cap_refuses_trades() {
    let mut f = fixture("fee-cap", 2, 16).await;
    f.engine.next_base_fee = Some(45 * GWEI);
    assert_eq!(f.engine.accept(vec![op(1, 2, 9)], 10)[0].1, Verdict::GasCap);
    f.engine.next_base_fee = Some(10 * GWEI);
    f.engine.live.max_gas_cost = gas_cost(250_000, 21 * GWEI) - wei(1);
    assert_eq!(f.engine.accept(vec![op(1, 2, 9)], 10)[0].1, Verdict::GasCap);
    f.engine.live.max_gas_cost = gas_cost(250_000, 21 * GWEI);
    assert_eq!(f.engine.accept(vec![op(1, 2, 9)], 10)[0].1, Verdict::Sent);
    assert_eq!(f.engine.pending[0].fee_cap, 21 * GWEI);
    assert_eq!(f.engine.pending[0].tip, GWEI);
  }

  /// V2 and V4 legs carry the executor's encodings with `zeroForOne` from the input side,
  /// native ETH included.
  #[test]
  fn legs_built_for_v2_and_v4() {
    let key = V4Key {
      currency0: Address::ZERO,
      currency1: token(),
      fee: 500,
      tick_spacing: 10,
      hooks: Address::ZERO,
    };
    let mut specs = legs();
    specs.insert(
      pool(9),
      LegSpec {
        family: Family::V4,
        pair: None,
        key: Some(key),
        token0: Address::ZERO,
        fee_bps: 5,
      },
    );
    let (buy, sell) = build_legs(&op(9, 1, 9), &specs).unwrap();
    assert_eq!(buy.family, 1);
    let (decoded, zero_for_one) = <(PoolKey, bool)>::abi_decode_params(&buy.data).unwrap();
    assert_eq!(decoded.abi_encode(), key.pool_key().abi_encode());
    assert!(zero_for_one);
    assert_eq!(sell.family, 0);
    assert_eq!(
      <(Address, bool, u16)>::abi_decode_params(&sell.data).unwrap(),
      (Address::repeat_byte(1), true, 30)
    );
    let (buy, sell) = build_legs(&op(1, 9, 9), &specs).unwrap();
    assert_eq!(
      <(Address, bool, u16)>::abi_decode_params(&buy.data).unwrap(),
      (Address::repeat_byte(1), false, 30)
    );
    assert!(!<(PoolKey, bool)>::abi_decode_params(&sell.data).unwrap().1);
    assert!(build_legs(&op(1, 99, 9), &specs).is_err());
    let mut other = op(9, 1, 9);
    other.token = to_id(Address::repeat_byte(0x55));
    assert!(matches!(
      build_legs(&other, &specs),
      Err(Error::TokenNotInPool)
    ));
  }

  /// Profit is WETH into the executor less WETH out of it, wraps and unwraps included.
  #[test]
  fn actual_profit_from_weth_logs() {
    let pair = Address::repeat_byte(0x01);
    let logs = vec![
      log(
        weth(),
        &IWETH::Deposit {
          dst: executor(),
          wad: wei(100),
        },
      ),
      transfer(executor(), pair, 100),
      transfer(pair, executor(), 106),
      log(
        weth(),
        &IWETH::Withdrawal {
          src: executor(),
          wad: wei(100),
        },
      ),
      log(
        Address::repeat_byte(0x02),
        &IERC20::Transfer {
          from: pair,
          to: executor(),
          value: wei(1000),
        },
      ),
    ];
    assert_eq!(weth_gain(&logs, weth(), executor()), Some(wei(6)));
    let losing = vec![transfer(executor(), pair, 5)];
    assert_eq!(weth_gain(&losing, weth(), executor()), None);
  }

  /// The published values follow blocks and trades.
  #[tokio::test]
  async fn published_values_update() {
    let f = fixture("published", 2, 16).await;
    let asserter = f.asserter.clone();
    let (commands, mut published) = f.engine.spawn();
    let first = published.borrow_and_update().clone();
    assert_eq!(first.gas_limits, [250_000, 300_000, 350_000]);
    assert_eq!(first.fee_per_gas, Some(11 * GWEI));
    assert_eq!(first.in_flight, 0);
    push_balances(&asserter, 2, 3 * ETH);
    let mut full = head(11);
    full.base_fee = 20 * GWEI;
    full.gas_used = full.gas_limit;
    commands
      .send(EngineCmd::Block {
        block: full,
        applied: 11,
      })
      .await
      .unwrap();
    published.changed().await.unwrap();
    let after = published.borrow_and_update().clone();
    assert_eq!(after.fee_per_gas, Some(22_500_000_000 + GWEI));
    assert_eq!(after.status.block, 11);
    assert_eq!(after.status.next_base_fee, 22_500_000_000);
    assert_eq!(after.status.accounts[1].eth, wei(3 * ETH));
    assert_eq!(after.status.accounts[0].role, Role::Owner);
    push_sent(&asserter);
    let (reply, verdicts) = oneshot::channel();
    commands
      .send(EngineCmd::Submit {
        ops: vec![op(1, 2, 9)],
        reply,
      })
      .await
      .unwrap();
    assert_eq!(verdicts.await.unwrap(), vec![(op(1, 2, 9), Verdict::Sent)]);
    published.changed().await.unwrap();
    let sent = published.borrow_and_update().clone();
    assert_eq!(sent.in_flight, 1);
    assert_eq!(sent.status.accounts[1].in_flight, 1);
    assert_eq!(sent.status.accounts[1].nonce, 1);
  }

  /// A trade's planned, sent and executed states reach the journal and the dashboard, the
  /// executed one with receipt values.
  #[tokio::test]
  async fn every_state_change_journaled() {
    let mut f = fixture("journaled", 2, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    let logs = vec![transfer(Address::repeat_byte(1), executor(), 5)];
    f.asserter.push_success(&receipt(11, true, 200_000, logs));
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(11), 11).await;
    let entries = journal_entries(&f.journal, 3).await;
    let states: Vec<TradeState> = entries.iter().map(|e| e.state).collect();
    assert_eq!(
      states,
      vec![TradeState::Planned, TradeState::Sent, TradeState::Executed]
    );
    assert!(
      entries
        .iter()
        .all(|e| e.id == 1 && e.kind == EntryKind::Trade)
    );
    assert!(entries[1].tx.is_some());
    let executed = &entries[2].detail;
    assert_eq!(executed.gas_used, Some(200_000));
    assert_eq!(executed.mined_block, Some(11));
    assert_eq!(executed.actual_profit, Some(wei(5)));
    assert_eq!(executed.effective_gas_price, Some(11 * GWEI));
    assert_eq!(executed.buy_pool, Some(pool(1)));
    assert_eq!(
      drain(&mut f.events),
      vec![
        (1, EntryKind::Trade, TradeState::Planned),
        (1, EntryKind::Trade, TradeState::Sent),
        (1, EntryKind::Trade, TradeState::Executed),
      ]
    );
  }

  /// A full dashboard channel neither blocks the engine nor costs a journal entry.
  #[tokio::test]
  async fn full_event_channel_does_not_block() {
    let mut f = fixture("full-events", 2, 1).await;
    tokio::time::timeout(Duration::from_secs(5), async {
      f.engine.accept(vec![op(1, 2, 9)], 10);
      push_sent(&f.asserter);
      f.engine.send_planned().await;
      push_mined(&f.asserter, 11, false, 100_000);
      push_balances(&f.asserter, 2, ETH);
      f.engine.on_block(head(11), 11).await;
    })
    .await
    .expect("engine blocked on a full event channel");
    assert_eq!(drain(&mut f.events).len(), 1);
    let entries = journal_entries(&f.journal, 3).await;
    assert_eq!(entries[2].state, TradeState::Reverted);
  }

  /// `Control::Sweep` sends the executor's whole WETH balance to the treasury from the owner.
  #[tokio::test]
  async fn sweep_control_sends_full_balance() {
    let mut f = fixture("sweep", 2, 16).await;
    push_multicall(
      &f.asserter,
      vec![
        Some(wei(7).abi_encode()),
        Some(wei(0).abi_encode()),
        Some(wei(ETH).abi_encode()),
        Some(wei(ETH).abi_encode()),
      ],
    );
    push_sent(&f.asserter);
    f.engine.control(Control::Sweep).await;
    let sweep = &f.engine.pending[0];
    assert_eq!(
      (sweep.kind, sweep.lane, sweep.state),
      (EntryKind::Sweep, 0, TradeState::Sent)
    );
    let call = FlashArbitrage::sweepCall {
      token: weth(),
      amount: wei(7),
    };
    assert_eq!(sweep.data, Bytes::from(call.abi_encode()));
    assert_eq!(sweep.to, executor());
  }

  /// `Control::Resume` closes an open global breaker.
  #[tokio::test]
  async fn resume_closes_breaker() {
    let mut f = fixture("resume", 2, 16).await;
    f.engine
      .breakers
      .record([&pool(7), &pool(8)], 10, true, wei(5000));
    assert_eq!(
      f.engine.accept(vec![op(1, 2, 9)], 10)[0].1,
      Verdict::CircuitOpen
    );
    f.engine.control(Control::Resume).await;
    assert_eq!(f.engine.accept(vec![op(1, 2, 9)], 10)[0].1, Verdict::Sent);
  }

  /// Ledger ids start at the `first_id` the caller seeds from the journal.
  #[tokio::test]
  async fn ids_start_at_first_id() {
    let mut f = fixture_from("first-id", 2, 16, 41).await;
    let verdicts = f.engine.accept(vec![op(1, 2, 9), op(2, 3, 5)], 10);
    assert_eq!(verdicts[1].1, Verdict::Locked(41));
    assert_eq!(f.engine.pending[0].id, 41);
    assert_eq!(f.engine.next_id, 42);
  }

  /// A stuck trade whose nonce another transaction consumed is dropped, its locks are
  /// released, no cancel is sent, and its lane is resynced.
  #[tokio::test]
  async fn replaced_nonce_releases_locks() {
    let mut f = fixture("replaced", 3, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 1);
    push_balances(&f.asserter, 3, ETH);
    f.engine.on_block(head(14), 14).await;
    assert!(f.engine.pending.is_empty());
    assert_eq!(f.engine.lanes[1].nonce, None);
    assert!(drain(&mut f.events).contains(&(1, EntryKind::Trade, TradeState::Dropped)));
    let verdicts = f.engine.accept(vec![op(1, 3, 9)], 14);
    assert_eq!(verdicts[0].1, Verdict::Sent);
    assert_eq!(f.engine.pending[0].lane, 2);
    assert!(f.asserter.read_q().is_empty());
  }

  /// A cancel that is not mined is rebroadcast each block, its lane gets no new trades, and
  /// once the nonce is consumed with neither receipt both are dropped and the locks freed.
  #[tokio::test]
  async fn stuck_cancel_rebroadcast_until_replaced() {
    let mut f = fixture("stuck-cancel", 2, 32).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(14), 14).await;
    assert!(f.engine.pending[0].cancel.is_some());
    assert_eq!(f.engine.accept(vec![op(3, 4, 9)], 14)[0].1, Verdict::LowGas);
    push_unmined(&f.asserter);
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(15), 15).await;
    assert_eq!(f.engine.pending[0].state, TradeState::Sent);
    push_unmined(&f.asserter);
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 1);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(16), 16).await;
    assert!(f.engine.pending.is_empty());
    let events = drain(&mut f.events);
    assert!(events.contains(&(1, EntryKind::Trade, TradeState::Dropped)));
    assert!(events.contains(&(1, EntryKind::Cancel, TradeState::Dropped)));
    assert!(f.asserter.read_q().is_empty());
  }

  /// An ambiguous send error keeps the trade sent with its local hash and its pools locked,
  /// and the receipt then settles it.
  #[tokio::test]
  async fn ambiguous_send_keeps_trade_sent() {
    let mut f = fixture("ambiguous", 3, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    f.asserter.push_failure_msg("already known");
    f.engine.send_planned().await;
    f.engine.release();
    let trade = &f.engine.pending[0];
    assert_eq!(trade.state, TradeState::Sent);
    assert!(trade.tx.is_some() && trade.unconfirmed);
    assert_eq!(f.engine.lanes[1].nonce, None);
    assert_eq!(
      f.engine.accept(vec![op(1, 3, 9)], 10)[0].1,
      Verdict::Locked(1)
    );
    push_nonce(&f.asserter, 1);
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_unmined(&f.asserter);
    push_balances(&f.asserter, 3, 2 * ETH);
    f.engine.on_block(head(11), 11).await;
    assert_eq!(f.engine.lanes[1].eth, wei(2 * ETH), "reads out of order");
    assert_eq!(f.engine.pending[0].state, TradeState::Sent);
    assert_eq!(
      f.engine.accept(vec![op(1, 3, 9)], 11)[0].1,
      Verdict::Locked(1)
    );
    push_mined(&f.asserter, 12, true, 200_000);
    push_balances(&f.asserter, 3, ETH);
    f.engine.on_block(head(12), 12).await;
    assert!(f.engine.pending.is_empty());
    assert_eq!(
      drain(&mut f.events),
      vec![
        (1, EntryKind::Trade, TradeState::Planned),
        (1, EntryKind::Trade, TradeState::Sent),
        (1, EntryKind::Trade, TradeState::Executed),
      ]
    );
    assert!(f.asserter.read_q().is_empty());
  }

  /// An ambiguous send the node does not know is kept, locks held, while the node may only
  /// be lagging; once `trade_timeout` blocks pass with the hash still unknown and the lane's
  /// pending nonce not past it, it is dropped and its locks released.
  #[tokio::test]
  async fn unknown_ambiguous_send_dropped() {
    let mut f = fixture("ambiguous-lost", 3, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    f.asserter.push_failure_msg("already known");
    f.engine.send_planned().await;
    push_nonce(&f.asserter, 0);
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_unmined(&f.asserter);
    push_balances(&f.asserter, 3, 2 * ETH);
    f.engine.on_block(head(11), 11).await;
    assert_eq!(f.engine.lanes[1].eth, wei(2 * ETH), "reads out of order");
    assert_eq!(f.engine.pending[0].state, TradeState::Sent);
    assert_eq!(
      f.engine.accept(vec![op(1, 3, 9)], 11)[0].1,
      Verdict::Locked(1)
    );
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_balances(&f.asserter, 3, ETH);
    f.engine.on_block(head(14), 14).await;
    assert!(f.engine.pending.is_empty());
    assert_eq!(f.engine.lanes[1].nonce, None);
    assert!(drain(&mut f.events).contains(&(1, EntryKind::Trade, TradeState::Dropped)));
    assert_eq!(f.engine.accept(vec![op(1, 3, 9)], 14)[0].1, Verdict::Sent);
    assert!(f.asserter.read_q().is_empty());
  }

  /// A failed receipt read is not taken for "not mined": in that block a used nonce does
  /// not drop the entry, and the next block's receipt settles it as executed.
  #[tokio::test]
  async fn failed_receipt_read_defers_replaced() {
    let mut f = fixture("receipt-error", 2, 32).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(14), 14).await;
    assert!(f.engine.pending[0].cancel.is_some());
    f.asserter.push_failure_msg("header not found");
    push_unmined(&f.asserter);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 2, 2 * ETH);
    f.engine.on_block(head(15), 15).await;
    assert_eq!(f.engine.lanes[1].eth, wei(2 * ETH), "reads out of order");
    assert_eq!(f.engine.pending[0].state, TradeState::Sent);
    push_mined(&f.asserter, 15, true, 200_000);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(16), 16).await;
    assert!(f.engine.pending.is_empty());
    let events = drain(&mut f.events);
    assert!(events.contains(&(1, EntryKind::Trade, TradeState::Executed)));
    assert!(events.contains(&(1, EntryKind::Cancel, TradeState::Dropped)));
    assert!(!events.contains(&(1, EntryKind::Trade, TradeState::Dropped)));
    assert!(f.asserter.read_q().is_empty());
  }

  /// Only a node's error reply that is not "already known" is a definite rejection; timeouts
  /// and other transport failures may have reached the mempool.
  #[tokio::test]
  async fn send_rejection_classified() {
    let asserter = Asserter::new();
    let provider = mock_provider(&asserter);
    let reply = async |message: &'static str| {
      asserter.push_failure_msg(message);
      provider.send_raw_transaction(&[0x02]).await.unwrap_err()
    };
    assert!(rejected(
      &reply("insufficient funds for gas * price + value").await
    ));
    assert!(rejected(&reply("nonce too low").await));
    assert!(!rejected(&reply("already known").await));
    assert!(!rejected(&reply("ALREADY KNOWN").await));
    assert!(!rejected(&TransportErrorKind::custom_str(
      "request timed out"
    )));
    assert!(!rejected(&TransportError::NullResp));
  }

  /// A send error on one lane sends the other lanes' trades with no RPC in between, and that
  /// lane resyncs only before its own next send.
  #[tokio::test]
  async fn send_error_does_not_delay_other_lanes() {
    let mut f = fixture("other-lanes", 3, 16).await;
    f.engine
      .accept(vec![op(1, 2, 9), op(3, 4, 8), op(5, 6, 7)], 10);
    let lanes: Vec<usize> = f.engine.pending.iter().map(|p| p.lane).collect();
    assert_eq!(lanes, vec![1, 2, 1]);
    f.asserter
      .push_failure_msg("insufficient funds for gas * price + value");
    push_sent(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    let states: Vec<(TradeState, u64)> = f
      .engine
      .pending
      .iter()
      .map(|p| (p.state, p.nonce))
      .collect();
    assert_eq!(
      states,
      vec![
        (TradeState::Dropped, 0),
        (TradeState::Sent, 0),
        (TradeState::Sent, 0)
      ]
    );
    assert_eq!(f.engine.lanes[1].nonce, Some(1));
    assert!(f.asserter.read_q().is_empty());
  }

  /// An out of gas receipt raises the cached limit, and the next trade is planned with it.
  #[tokio::test]
  async fn out_of_gas_receipt_raises_next_limit() {
    let mut f = fixture("oog", 2, 16).await;
    f.engine.breakers = Breakers::new(BreakerConfig {
      max_revert_gas: wei(ETH),
      ..breaker_config()
    });
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    push_mined(&f.asserter, 11, false, 250_000);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(11), 11).await;
    assert_eq!(f.engine.snapshot().gas_limits[0], 375_000);
    assert_eq!(f.engine.accept(vec![op(3, 4, 9)], 11)[0].1, Verdict::Sent);
    assert_eq!(f.engine.pending[0].gas_limit, 375_000);
    let entries = journal_entries(&f.journal, 3).await;
    assert_eq!(entries[2].detail.revert.as_deref(), Some("out of gas"));
  }

  /// No cancel goes out while the fee cap blocks sending; the next block sends it.
  #[tokio::test]
  async fn no_cancel_while_fee_over_cap() {
    let mut f = fixture("cancel-fee", 2, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    let mut expensive = head(14);
    expensive.base_fee = 50 * GWEI;
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(expensive, 14).await;
    assert!(f.engine.pending[0].cancel.is_none());
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(15), 15).await;
    assert!(f.engine.pending[0].cancel.is_some());
    assert!(f.asserter.read_q().is_empty());
  }

  /// A cancel whose send fails is tried again the next block.
  #[tokio::test]
  async fn failed_cancel_retried_next_block() {
    let mut f = fixture("cancel-retry", 2, 16).await;
    f.engine.accept(vec![op(1, 2, 9)], 10);
    push_sent(&f.asserter);
    f.engine.send_planned().await;
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    f.asserter
      .push_failure_msg("replacement transaction underpriced");
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(14), 14).await;
    assert!(f.engine.pending[0].cancel.is_none());
    push_unmined(&f.asserter);
    push_nonce(&f.asserter, 0);
    push_sent(&f.asserter);
    push_balances(&f.asserter, 2, ETH);
    f.engine.on_block(head(15), 15).await;
    assert!(f.engine.pending[0].cancel.is_some());
    assert!(f.asserter.read_q().is_empty());
  }

  /// A forward is refused when its gas cost at the signed fee cap passes `max_gas_cost`,
  /// even though the expected fee would fit.
  #[tokio::test]
  async fn forward_cost_checked_at_fee_cap() {
    let mut f = fixture("forward-cap", 2, 16).await;
    f.engine.live.max_gas_cost = gas_cost(TRANSFER_GAS, 21 * GWEI) - wei(1);
    let forward = TxRequest {
      from: 1,
      to: treasury(),
      value: wei(ETH),
      data: Bytes::new(),
      kind: EntryKind::Forward,
    };
    assert!(!f.engine.queue(forward.clone()));
    assert!(f.engine.pending.is_empty());
    f.engine.live.max_gas_cost = gas_cost(TRANSFER_GAS, 21 * GWEI);
    assert!(f.engine.queue(forward));
    assert_eq!(f.engine.pending.len(), 1);
  }
}
