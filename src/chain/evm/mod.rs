//! EVM adapter, the only place that imports alloy.
//!
//! [`EvmChain`] wires the pieces: it checks the node, loads the accounts and the pools,
//! makes sure the executor is deployed, and starts the feed, the journal and the engine. On
//! the bot's task it applies each block to pool state, reloading after a gap and asking for a
//! restart when the executor's code is gone, and it hands trades to the engine.

pub mod accounts;
pub mod bindings;
pub mod breakers;
pub mod config;
pub mod convert;
pub mod engine;
pub mod executor;
pub mod feed;
pub mod fund;
pub mod gas;
pub mod holdings;
pub mod multicall;
pub mod pairs;
pub mod state;
pub mod ticks;
pub mod venue;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, B256, TxKind, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{error, info, warn};

use crate::chain::Chain;
use crate::chain::evm::accounts::signers;
use crate::chain::evm::breakers::Breakers;
use crate::chain::evm::config::EvmConfig;
use crate::chain::evm::convert::to_id;
use crate::chain::evm::engine::{Engine, EngineCmd, Published, SubmitNote, TRANSFER_GAS, Timing};
use crate::chain::evm::executor::ensure_executor;
use crate::chain::evm::feed::{Feed, FeedBlock, FeedEvent, FeedHandle};
use crate::chain::evm::gas::{gas_cost, pair_index};
use crate::chain::evm::holdings::Holdings;
use crate::chain::evm::pairs::{LegSpec, Tracked, discover, leg_specs, listed};
use crate::chain::evm::state::State;
use crate::config::{Live, NetworkConfig, PairMode};
use crate::error::{Error, Result};
use crate::journal::{Journal, read_last};
use crate::types::{Amount, Control, Id, Opportunity, Status, TradeEvent, Update, Verdict};
use crate::venue::Venue;

/// Env var holding the node HTTP endpoint.
const RPC_URL: &str = "RPC_URL";
/// Env var holding the node WebSocket endpoint.
const WS_URL: &str = "WS_URL";
/// Trade events buffered for the dashboard before the engine starts skipping them.
const TRADE_EVENTS: usize = 1024;
/// Pause between attempts of a read that must succeed before the bot may go on.
const RETRY: Duration = Duration::from_millis(500);
/// Forward gas headroom over the node's estimate, as `6 / 5` (20%).
const FORWARD_HEADROOM: (u64, u64) = (6, 5);

/// Reads the env var `name`, treating an unset or empty value as missing.
pub fn env_var(name: &str) -> Result<String> {
  std::env::var(name)
    .ok()
    .filter(|value| !value.trim().is_empty())
    .ok_or_else(|| Error::Env(name.to_string()))
}

/// Parses the address in `text`, naming the config `key` it came from on error.
pub fn parse_address(key: &str, text: &str) -> Result<Address> {
  Address::from_str(text).map_err(|e| Error::Config(format!("{key}: {text}: {e}")))
}

/// Locks `mutex`, recovering the value if a holder panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Milliseconds `now` is past the block `timestamp` (Unix seconds), zero for a block stamped
/// in the future.
fn lag_ms(timestamp: u64, now: SystemTime) -> u64 {
  let now_ms = now
    .duration_since(UNIX_EPOCH)
    .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
  now_ms.saturating_sub(timestamp.saturating_mul(1000))
}

/// The first ledger id this run may use: one past the highest id in the journal at `path`,
/// or 1 when it is missing or empty, so ids stay unique across restarts.
async fn first_id(path: &Path) -> Result<u64> {
  match read_last(path, usize::MAX).await {
    Ok((entries, _)) => Ok(
      entries
        .iter()
        .map(|entry| entry.event.id)
        .max()
        .map_or(1, |id| id.saturating_add(1)),
    ),
    Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(1),
    Err(e) => Err(e),
  }
}

/// The contracts whose logs the feed fetches (every tracked pair and each V4 venue's
/// PoolManager, once each) and the tracked V4 pool ids.
fn feed_targets(tracked: &[Tracked], evm: &EvmConfig) -> (Vec<Address>, HashSet<B256>) {
  let mut addresses = BTreeSet::new();
  let mut v4_ids = HashSet::new();
  for pool in tracked {
    match *pool {
      Tracked::V2 { pair, .. } => {
        addresses.insert(pair);
      }
      Tracked::V4 { venue, key } => {
        addresses.insert(evm.venues[venue].address);
        v4_ids.insert(key.pool_id());
      }
    }
  }
  (addresses.into_iter().collect(), v4_ids)
}

/// Rereads every pool until it succeeds: after a gap the feed flags no second gap, and later
/// blocks would apply on top of state that is no longer true.
async fn reload(provider: &DynProvider, state: &mut State) {
  while let Err(e) = state.reload_all(provider).await {
    warn!(error = %e, "pool reload failed, retrying");
    tokio::time::sleep(RETRY).await;
  }
}

/// Whether `executor` has code, asking until the node answers, since sending to a codeless
/// executor must never happen and a failed read proves nothing.
async fn has_code(provider: &DynProvider, executor: Address) -> bool {
  loop {
    match provider.get_code_at(executor).await {
      Ok(code) => return !code.is_empty(),
      Err(e) => {
        warn!(%executor, error = %e, "executor code read failed, retrying");
        tokio::time::sleep(RETRY).await;
      }
    }
  }
}

/// The gas limit forwards to `treasury` are sent with, measured once at connect: a plain
/// transfer's 21,000 when the treasury has no code; for one with code (a smart wallet, an
/// EIP 7702 delegation) the node's estimate for a transfer from `from` plus 20%, capped at
/// `max_gas_limit`. A failed estimate falls back to 21,000, and the engine's pause after a
/// failed forward keeps that from repeating every block.
async fn forward_gas(
  provider: &DynProvider,
  from: Address,
  treasury: Address,
  max_gas_limit: u64,
) -> Result<u64> {
  if provider.get_code_at(treasury).await?.is_empty() {
    return Ok(TRANSFER_GAS);
  }
  let request = TransactionRequest {
    from: Some(from),
    to: Some(TxKind::Call(treasury)),
    value: Some(U256::from(1u8)),
    ..TransactionRequest::default()
  };
  match provider.estimate_gas(request).await {
    Ok(estimate) => {
      let padded =
        u128::from(estimate) * u128::from(FORWARD_HEADROOM.0) / u128::from(FORWARD_HEADROOM.1);
      Ok(u64::try_from(padded).unwrap_or(u64::MAX).min(max_gas_limit))
    }
    Err(e) => {
      warn!(%treasury, error = %e, "forward gas estimate failed, using a plain transfer");
      Ok(TRANSFER_GAS)
    }
  }
}

/// What only the bot's task touches: pool state and the feed's event stream.
struct Inner {
  /// The tracked pools.
  state: State,
  /// Blocks and tick windows from the feed.
  events: mpsc::Receiver<FeedEvent>,
}

/// An EVM network: pool state fed block by block, and the engine that sends trades.
pub struct EvmChain {
  /// HTTP node connection for reloads and code checks.
  provider: DynProvider,
  /// The executor contract.
  executor: Address,
  /// The wrapped native token's id, the base the bot trades in.
  base: Id,
  /// Leg encoding per pool, for its family when costing a trade.
  legs: HashMap<Id, LegSpec>,
  /// State and feed, locked only by `changes`, which the bot never runs twice at once.
  inner: tokio::sync::Mutex<Inner>,
  /// Snapshots as of the last applied change, so readers never wait on `changes`.
  snapshots: RwLock<Vec<Arc<dyn Venue>>>,
  /// Asks the feed for tick window refetches; dropping it stops the feed.
  feed: FeedHandle,
  /// Commands to the engine task, in the order they must run.
  commands: mpsc::UnboundedSender<EngineCmd>,
  /// Set when a head went to the engine and its maintenance has not: it follows the next
  /// submit, or goes at the start of the next `changes` when the bot submitted nothing.
  maintenance_due: AtomicBool,
  /// What the engine published last.
  published: watch::Receiver<Published>,
  /// How far the last block read lagged its timestamp.
  feed_lag_ms: AtomicU64,
  /// When the last block was seen by the feed and read by the bot.
  last_block: Mutex<Option<(Instant, Instant)>>,
}

impl EvmChain {
  /// Connects to the network `cfg` describes and starts its tasks, returning the chain and
  /// the trade events for the dashboard.
  ///
  /// The node must report `adapter.chain_id`. Accounts come from the env vars the config
  /// names, pools from the token list or discovery, and the executor record lives in
  /// `.arbitrage-<network>.json`.
  pub async fn connect(
    cfg: &NetworkConfig,
    network: &str,
  ) -> Result<(Self, mpsc::Receiver<TradeEvent>)> {
    let evm = EvmConfig::parse(cfg)?;
    let rpc_url = env_var(RPC_URL)?;
    let ws_url = env_var(WS_URL)?;
    let provider = ProviderBuilder::new().connect(&rpc_url).await?.erased();
    let chain_id = provider.get_chain_id().await?;
    if chain_id != evm.chain_id {
      return Err(Error::Config(format!(
        "node chain id {chain_id} does not match adapter.chain_id {}",
        evm.chain_id
      )));
    }
    let signers = signers(&cfg.accounts)?;
    let owner = signers
      .get(cfg.owner)
      .cloned()
      .ok_or_else(|| Error::Config("owner: index is outside the accounts list".into()))?;
    let weth = parse_address("base_token", &cfg.base_token)?;
    let treasury = parse_address("treasury", &cfg.treasury)?;
    let tracked = match cfg.pairs.mode {
      PairMode::List => listed(&provider, cfg, &evm).await?,
      PairMode::Discover => discover(&provider, cfg, &evm).await?,
    };
    if tracked.is_empty() {
      return Err(Error::Config("no tradable pairs".into()));
    }
    let operators: Vec<Address> = signers
      .iter()
      .enumerate()
      .filter(|(index, _)| *index != cfg.owner)
      .map(|(_, signer)| signer.address())
      .collect();
    let deployer = ProviderBuilder::new()
      .wallet(EthereumWallet::from(owner.clone()))
      .connect(&rpc_url)
      .await?;
    let record = PathBuf::from(format!(".arbitrage-{network}.json"));
    let executor = ensure_executor(
      &deployer,
      &record,
      &evm,
      weth,
      treasury,
      owner.address(),
      &operators,
    )
    .await?;
    let forward_gas = forward_gas(&provider, owner.address(), treasury, evm.max_gas_limit).await?;
    let state = State::load(&provider, tracked.clone(), &evm, weth).await?;
    let (addresses, v4_ids) = feed_targets(&tracked, &evm);
    let (feed, events) = Feed::spawn(
      ws_url,
      provider.clone(),
      addresses,
      v4_ids,
      evm.multicall,
      evm.state_view,
    );
    let first_id = first_id(&cfg.journal).await?;
    let journal = Journal::spawn(&cfg.journal).await?;
    let (trade_events, trades) = mpsc::channel(TRADE_EVENTS);
    let legs = leg_specs(&tracked);
    let accounts = signers.len();
    let engine = Engine::new(
      provider.clone(),
      signers,
      cfg.owner,
      evm.clone(),
      cfg.live.clone(),
      Breakers::new(cfg.breakers.clone()),
      Holdings::new(cfg.live.limits.clone(), cfg.owner, treasury, executor, weth),
      executor,
      legs.clone(),
      journal,
      trade_events,
      first_id,
    )
    .with_forward_gas(forward_gas);
    let (commands, published) = engine.spawn();
    let base = to_id(weth);
    let snapshots = state.snapshots();
    let pairs: HashSet<&Id> = snapshots
      .iter()
      .flat_map(|venue| venue.tokens())
      .filter(|token| **token != base)
      .collect();
    info!(
      network,
      pairs = pairs.len(),
      pools = snapshots.len(),
      accounts,
      %executor,
      %treasury,
      max_fee_per_gas = evm.max_fee_per_gas,
      max_gas_cost = %cfg.live.max_gas_cost,
      max_gas_limit = evm.max_gas_limit,
      forward_gas,
      first_id,
      "connected"
    );
    let chain = Self {
      provider,
      executor,
      base,
      legs,
      inner: tokio::sync::Mutex::new(Inner { state, events }),
      snapshots: RwLock::new(snapshots),
      feed,
      commands,
      maintenance_due: AtomicBool::new(false),
      published,
      feed_lag_ms: AtomicU64::new(0),
      last_block: Mutex::new(None),
    };
    Ok((chain, trades))
  }

  /// Queues `command` for the engine, behind every command sent before it.
  fn command(&self, command: EngineCmd) {
    if self.commands.send(command).is_err() {
      error!("engine stopped, command lost");
    }
  }

  /// Sends the held back maintenance for the last head, if any is due.
  fn maintain(&self) {
    if self.maintenance_due.swap(false, Ordering::SeqCst) {
      self.command(EngineCmd::Maintain);
    }
  }

  /// Publishes the snapshots of `state` to readers.
  fn refresh(&self, state: &State) {
    *self.snapshots.write().unwrap_or_else(|e| e.into_inner()) = state.snapshots();
  }

  /// Applies `block` to `state`: a change that cannot be trusted reloads every pool. Every
  /// reload first checks that the executor still has code, whether a feed gap or a skipped
  /// block asked for it, since a fork reset before the feed's first head shows up only as a
  /// skip. When the code is gone the chain asks for a restart and skips the reload, since the
  /// chain is about to be rebuilt and rereading every pool of a reset fork is the slow part.
  /// Pools near their tick window's edge get a refetch, then the engine gets the head, which
  /// it handles without RPC; its maintenance is held back until after the submit.
  ///
  /// The block counts as read once state reflects it, so a reload's time is in the read
  /// stage of a trade's latency rather than in the decision.
  async fn on_block(&self, state: &mut State, block: FeedBlock) -> Result<Update> {
    self.feed_lag_ms.store(
      lag_ms(block.timestamp, SystemTime::now()),
      Ordering::Relaxed,
    );
    let mut update = state.apply(&block);
    if update.reload {
      if !has_code(&self.provider, self.executor).await {
        warn!(executor = %self.executor, "executor has no code before a reload, restarting");
        update.restart = true;
      } else {
        reload(&self.provider, state).await;
      }
    }
    *lock(&self.last_block) = Some((block.seen, Instant::now()));
    for (id, request) in state.near_edge() {
      self.feed.refetch(id, request);
    }
    self.refresh(state);
    let applied = state.block();
    self
      .commands
      .send(EngineCmd::Head { block, applied })
      .map_err(|_| Error::Rpc("engine stopped".into()))?;
    self.maintenance_due.store(true, Ordering::SeqCst);
    Ok(update)
  }
}

impl Chain for EvmChain {
  /// Waits for the next feed event and applies it: a tick window changes no quote inside
  /// the old window and yields an empty update; a block is applied as [`EvmChain::on_block`]
  /// describes. Maintenance still due for the last head, because the bot submitted nothing,
  /// goes first so the engine works on it while this call waits.
  async fn changes(&self) -> Result<Update> {
    self.maintain();
    let mut inner = self.inner.lock().await;
    let Some(event) = inner.events.recv().await else {
      return Err(Error::Rpc("feed stopped".into()));
    };
    match event {
      FeedEvent::Window(id, fetched, ticks) => {
        inner.state.apply_window(&id, fetched, ticks);
        self.refresh(&inner.state);
        Ok(Update {
          block: inner.state.block(),
          ..Update::default()
        })
      }
      FeedEvent::Block(block) => self.on_block(&mut inner.state, block).await,
    }
  }

  /// The snapshots as of the last applied change.
  fn snapshots(&self) -> Vec<Arc<dyn Venue>> {
    self
      .snapshots
      .read()
      .unwrap_or_else(|e| e.into_inner())
      .clone()
  }

  /// The gas limit of the trade's family pair times the fee per gas the engine published;
  /// `None` while the fee cap blocks sending, or for a pool the chain does not track.
  fn cost(&self, op: &Opportunity) -> Option<Amount> {
    let buy = self.legs.get(&op.buy_pool)?;
    let sell = self.legs.get(&op.sell_pool)?;
    let published = self.published.borrow();
    let fee = published.fee_per_gas?;
    let limit = published.gas_limits[pair_index(buy.family, sell.family)];
    Some(gas_cost(limit, fee))
  }

  /// Hands the batch to the engine with its pools' descriptions and the block's timing, queues
  /// the head's held back maintenance behind it, and waits for the verdicts; a stopped engine
  /// yields none, which the bot reports.
  async fn submit(&self, ops: Vec<Opportunity>) -> Vec<(Opportunity, Verdict)> {
    let decided = Instant::now();
    let pools: HashSet<&Id> = ops
      .iter()
      .flat_map(|op| [&op.buy_pool, &op.sell_pool])
      .collect();
    let describe = self
      .snapshots()
      .iter()
      .filter(|venue| pools.contains(venue.pool()))
      .map(|venue| (venue.pool().clone(), venue.describe()))
      .collect();
    let timing = lock(&self.last_block).map(|(seen, read)| Timing {
      seen,
      read,
      decided,
    });
    let (reply, answer) = oneshot::channel();
    let command = EngineCmd::Submit {
      ops,
      note: SubmitNote { describe, timing },
      reply,
    };
    if self.commands.send(command).is_err() {
      error!("engine stopped, trades not submitted");
      return Vec::new();
    }
    self.maintain();
    answer.await.unwrap_or_else(|_| {
      error!("engine dropped a submit without verdicts");
      Vec::new()
    })
  }

  /// Passes the new live settings to the engine.
  fn apply_live(&self, live: &Live) {
    self.command(EngineCmd::SetLive(live.clone()));
  }

  /// Passes the operator command to the engine.
  fn control(&self, command: Control) {
    self.command(EngineCmd::Control(command));
  }

  /// Trades in flight as the engine last published.
  fn in_flight(&self) -> usize {
    self.published.borrow().in_flight
  }

  /// The wrapped native token.
  fn base(&self) -> Id {
    self.base.clone()
  }

  /// The engine's status with the feed lag filled in.
  fn status(&self) -> Status {
    let mut status = self.published.borrow().status.clone();
    status.feed_lag_ms = self.feed_lag_ms.load(Ordering::Relaxed);
    status
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use alloy::primitives::{Bytes, U64};
  use alloy::providers::mock::Asserter;

  use crate::chain::evm::config::Family;
  use crate::chain::evm::ticks::BoundedTicks;
  use crate::journal::JournalEntry;
  use crate::testkit::{MockVenue, mock_evm_config, mock_provider};
  use crate::types::{EntryKind, TradeDetail, TradeState};

  /// One gwei in wei.
  const GWEI: u128 = 1_000_000_000;
  /// Block the test state is loaded at.
  const LOADED: u64 = 10;

  /// The wrapped native token.
  fn weth() -> Address {
    Address::repeat_byte(0xee)
  }

  /// The executor contract.
  fn executor() -> Address {
    Address::repeat_byte(0xbb)
  }

  /// What the engine publishes: `fee` per gas and the default gas limits.
  fn published(fee: Option<u128>) -> Published {
    Published {
      fee_per_gas: fee,
      gas_limits: [250_000, 300_000, 350_000],
      in_flight: 0,
      status: Status {
        block: 0,
        next_base_fee: 0,
        fee_cap: 0,
        max_fee_per_gas: 0,
        feed_lag_ms: 0,
        executor: executor().to_string(),
        executor_balance: Amount::ZERO,
        treasury: String::new(),
        treasury_balance: Amount::ZERO,
        accounts: Vec::new(),
      },
    }
  }

  /// A leg spec of `family` with nothing else set.
  fn spec(family: Family) -> LegSpec {
    LegSpec {
      family,
      pair: None,
      key: None,
      token0: Address::ZERO,
      fee_bps: 30,
    }
  }

  /// A chain over an empty pool list loaded at block 10, with the test holding the feed's
  /// sending end, the engine's receiving end and the published values.
  struct Harness {
    /// The chain under test.
    chain: EvmChain,
    /// Queued node answers.
    asserter: Asserter,
    /// Feeds events to `changes`.
    feed: mpsc::Sender<FeedEvent>,
    /// What the chain asked of the engine.
    commands: mpsc::UnboundedReceiver<EngineCmd>,
    /// Stands in for the engine's publishing side.
    published: watch::Sender<Published>,
  }

  /// A harness whose chain knows the pools in `legs` and tracks `tracked`, loaded from the
  /// answers `load` queues.
  async fn harness_with(
    legs: HashMap<Id, LegSpec>,
    tracked: Vec<Tracked>,
    load: impl FnOnce(&Asserter),
  ) -> Harness {
    let asserter = Asserter::new();
    let provider = mock_provider(&asserter).erased();
    load(&asserter);
    let state = State::load(&provider, tracked, &mock_evm_config(), weth())
      .await
      .unwrap();
    assert!(asserter.read_q().is_empty(), "load left answers unread");
    let (feed, events) = mpsc::channel(8);
    let (commands_tx, commands) = mpsc::unbounded_channel();
    let (published, published_rx) = watch::channel(published(Some(GWEI)));
    let chain = EvmChain {
      provider,
      executor: executor(),
      base: to_id(weth()),
      legs,
      inner: tokio::sync::Mutex::new(Inner { state, events }),
      snapshots: RwLock::new(Vec::new()),
      feed: FeedHandle::detached(),
      commands: commands_tx,
      maintenance_due: AtomicBool::new(false),
      published: published_rx,
      feed_lag_ms: AtomicU64::new(0),
      last_block: Mutex::new(None),
    };
    Harness {
      chain,
      asserter,
      feed,
      commands,
      published,
    }
  }

  /// A harness over no pools loaded at block 10, whose chain knows the pools in `legs`.
  async fn harness(legs: HashMap<Id, LegSpec>) -> Harness {
    harness_with(legs, Vec::new(), |asserter| {
      asserter.push_success(&U64::from(LOADED))
    })
    .await
  }

  /// The names of the commands queued for the engine so far, in order.
  fn sent(commands: &mut mpsc::UnboundedReceiver<EngineCmd>) -> Vec<&'static str> {
    std::iter::from_fn(|| commands.try_recv().ok())
      .map(|command| match command {
        EngineCmd::Submit { .. } => "submit",
        EngineCmd::Head { .. } => "head",
        EngineCmd::Maintain => "maintain",
        EngineCmd::Control(_) => "control",
        EngineCmd::SetLive(_) => "live",
      })
      .collect()
  }

  /// A block at `number` with no logs, stamped at the epoch, flagged as a gap when `gap`.
  fn block(number: u64, gap: bool) -> FeedBlock {
    FeedBlock {
      number,
      hash: B256::repeat_byte(number as u8),
      base_fee: 0,
      gas_used: 0,
      gas_limit: 0,
      timestamp: 0,
      seen: Instant::now(),
      logs: Vec::new(),
      gap,
    }
  }

  /// The highest applied block the chain reported to the engine with its next command.
  fn applied(commands: &mut mpsc::UnboundedReceiver<EngineCmd>) -> u64 {
    match commands.try_recv() {
      Ok(EngineCmd::Head { applied, .. }) => applied,
      other => panic!("expected a head command, got {other:?}"),
    }
  }

  /// After a gap the executor's code is read first, through a failed attempt, and missing
  /// code asks for a restart without rereading any pool: the chain is about to be rebuilt,
  /// and on a reset fork that reload is the slow part. The block number answer queued after
  /// the code is the read a reload would start with, so it must stay unread.
  #[tokio::test(start_paused = true)]
  async fn missing_code_after_gap_requests_restart() {
    let mut h = harness(HashMap::new()).await;
    h.asserter.push_failure_msg("node down");
    h.asserter.push_success(&Bytes::new());
    h.asserter.push_success(&U64::from(12));
    h.feed
      .send(FeedEvent::Block(block(12, true)))
      .await
      .unwrap();
    let update = tokio::time::timeout(Duration::from_secs(5), h.chain.changes())
      .await
      .expect("changes waited on a reload")
      .unwrap();
    assert!(update.reload && update.restart, "{update:?}");
    assert_eq!(h.asserter.read_q().len(), 1, "a pool reload ran");
    assert_eq!(applied(&mut h.commands), 12);
    assert!(h.chain.status().feed_lag_ms > 0);
  }

  /// After a gap an executor that still has code keeps the chain running, its code read
  /// before the pools are reloaded.
  #[tokio::test]
  async fn executor_with_code_survives_gap() {
    let mut h = harness(HashMap::new()).await;
    h.asserter.push_success(&Bytes::from(vec![0x60, 0x80]));
    h.asserter.push_success(&U64::from(12));
    h.feed
      .send(FeedEvent::Block(block(12, true)))
      .await
      .unwrap();
    let update = tokio::time::timeout(Duration::from_secs(5), h.chain.changes())
      .await
      .expect("changes hung on a misordered read")
      .unwrap();
    assert!(update.reload && !update.restart, "{update:?}");
    assert_eq!(applied(&mut h.commands), 12);
  }

  /// A block that follows the loaded one applies without a node call and reaches the engine.
  #[tokio::test]
  async fn next_block_applies_locally() {
    let mut h = harness(HashMap::new()).await;
    h.feed
      .send(FeedEvent::Block(block(LOADED + 1, false)))
      .await
      .unwrap();
    let update = h.chain.changes().await.unwrap();
    assert!(!update.reload && !update.restart);
    assert_eq!(update.block, LOADED + 1);
    assert_eq!(applied(&mut h.commands), LOADED + 1);
  }

  /// A tick window yields an empty update at the current block and tells the engine nothing.
  #[tokio::test]
  async fn window_gives_empty_update() {
    let mut h = harness(HashMap::new()).await;
    let ticks = BoundedTicks::new(Vec::new(), -600, 600);
    h.feed
      .send(FeedEvent::Window(Id::new(vec![1; 32]), LOADED, ticks))
      .await
      .unwrap();
    let update = h.chain.changes().await.unwrap();
    assert_eq!(
      update,
      Update {
        block: LOADED,
        ..Update::default()
      }
    );
    assert!(h.commands.try_recv().is_err());
  }

  /// Cost is the family pair's gas limit times the published fee, and unknown while the fee
  /// cap blocks sending.
  #[tokio::test]
  async fn cost_uses_pair_limit_and_fee() {
    let (v2, v4) = (Id::new(vec![2; 20]), Id::new(vec![4; 32]));
    let legs = HashMap::from([
      (v2.clone(), spec(Family::V2)),
      (v4.clone(), spec(Family::V4)),
    ]);
    let h = harness(legs).await;
    let op = Opportunity {
      buy_pool: v2,
      sell_pool: v4,
      token: Id::new(vec![7; 20]),
      amount: Amount::from(1u64),
      profit: Amount::from(1u64),
      floor: Amount::ZERO,
    };
    assert_eq!(h.chain.cost(&op), Some(gas_cost(300_000, GWEI)));
    h.published.send_replace(published(None));
    assert_eq!(h.chain.cost(&op), None);
  }

  /// A submit carries its pools' descriptions and the timing of the block it answers.
  #[tokio::test]
  async fn submit_carries_describe_and_timing() {
    let venues: Vec<Arc<dyn Venue>> = vec![
      Arc::new(MockVenue::new("a", "base", "x", 1, 1, 30)),
      Arc::new(MockVenue::new("b", "base", "x", 1, 1, 30)),
      Arc::new(MockVenue::new("c", "base", "y", 1, 1, 30)),
    ];
    let mut h = harness(HashMap::new()).await;
    let head = block(LOADED + 1, false);
    let seen = head.seen;
    h.feed.send(FeedEvent::Block(head)).await.unwrap();
    h.chain.changes().await.unwrap();
    applied(&mut h.commands);
    *h.chain.snapshots.write().unwrap() = venues;
    let op = Opportunity {
      buy_pool: Id::new(b"a".to_vec()),
      sell_pool: Id::new(b"b".to_vec()),
      token: Id::new(b"x".to_vec()),
      amount: Amount::from(1u64),
      profit: Amount::from(1u64),
      floor: Amount::ZERO,
    };
    let engine = async {
      let Some(EngineCmd::Submit { ops, note, reply }) = h.commands.recv().await else {
        panic!("expected a submit");
      };
      assert_eq!(note.describe.len(), 2);
      assert_eq!(note.describe[&Id::new(b"a".to_vec())], "mock a");
      assert_eq!(note.describe[&Id::new(b"b".to_vec())], "mock b");
      let timing = note.timing.unwrap();
      assert_eq!(timing.seen, seen);
      assert!(timing.seen <= timing.read && timing.read <= timing.decided);
      reply
        .send(ops.into_iter().map(|op| (op, Verdict::Sent)).collect())
        .unwrap();
    };
    let (verdicts, ()) = tokio::join!(h.chain.submit(vec![op.clone()]), engine);
    assert_eq!(verdicts, vec![(op, Verdict::Sent)]);
    assert_eq!(sent(&mut h.commands), vec!["maintain"]);
  }

  /// A head reaches the engine before the submit it drives, and its maintenance only after
  /// that submit, so no node work sits between the decision and the send.
  #[tokio::test]
  async fn maintenance_follows_submit() {
    let mut h = harness(HashMap::new()).await;
    h.feed
      .send(FeedEvent::Block(block(LOADED + 1, false)))
      .await
      .unwrap();
    h.chain.changes().await.unwrap();
    let engine = async {
      match h.commands.recv().await {
        Some(EngineCmd::Head { .. }) => {}
        other => panic!("expected the head first, got {other:?}"),
      }
      let Some(EngineCmd::Submit { ops, reply, .. }) = h.commands.recv().await else {
        panic!("expected the submit second");
      };
      reply
        .send(ops.into_iter().map(|op| (op, Verdict::Sent)).collect())
        .unwrap();
    };
    let op = Opportunity {
      buy_pool: Id::new(b"a".to_vec()),
      sell_pool: Id::new(b"b".to_vec()),
      token: Id::new(b"x".to_vec()),
      amount: Amount::from(1u64),
      profit: Amount::from(1u64),
      floor: Amount::ZERO,
    };
    tokio::join!(h.chain.submit(vec![op]), engine);
    assert_eq!(sent(&mut h.commands), vec!["maintain"]);
    h.chain.control(Control::Pause);
    assert_eq!(sent(&mut h.commands), vec!["control"]);
  }

  /// Without a submit, the head's maintenance goes at the start of the next `changes`,
  /// before the next head, and only once.
  #[tokio::test]
  async fn maintenance_sent_by_next_changes_without_submit() {
    let mut h = harness(HashMap::new()).await;
    for number in [LOADED + 1, LOADED + 2] {
      h.feed
        .send(FeedEvent::Block(block(number, false)))
        .await
        .unwrap();
      h.chain.changes().await.unwrap();
    }
    assert_eq!(sent(&mut h.commands), vec!["head", "maintain", "head"]);
  }

  /// A reload without a gap, from a skipped block, checks the executor's code too: with
  /// code it rereads the pools, and without it asks for a restart and leaves the reload's
  /// block number read unread. A fork reset before the feed's first head shows up this way.
  #[tokio::test(start_paused = true)]
  async fn reload_without_gap_checks_code() {
    let mut h = harness(HashMap::new()).await;
    h.asserter.push_success(&Bytes::from(vec![0x60, 0x80]));
    h.asserter.push_success(&U64::from(LOADED + 2));
    h.feed
      .send(FeedEvent::Block(block(LOADED + 2, false)))
      .await
      .unwrap();
    let update = tokio::time::timeout(Duration::from_secs(5), h.chain.changes())
      .await
      .expect("changes hung on a misordered read")
      .unwrap();
    assert!(update.reload && !update.restart, "{update:?}");
    assert!(h.asserter.read_q().is_empty());
    assert_eq!(applied(&mut h.commands), LOADED + 2);
    h.asserter.push_success(&Bytes::new());
    h.asserter.push_success(&U64::from(LOADED + 4));
    h.feed
      .send(FeedEvent::Block(block(LOADED + 4, false)))
      .await
      .unwrap();
    let update = tokio::time::timeout(Duration::from_secs(5), h.chain.changes())
      .await
      .expect("changes waited on a reload")
      .unwrap();
    assert!(update.reload && update.restart, "{update:?}");
    assert_eq!(h.asserter.read_q().len(), 1, "a pool reload ran");
  }

  /// A V4 pool whose block moves it near its tick window's edge gets a refetch.
  #[tokio::test]
  async fn near_edge_pool_refetched() {
    use crate::chain::evm::state::tests as fixture;
    let h = harness_with(HashMap::new(), fixture::tracked(), |asserter| {
      fixture::push_load(asserter, fixture::GOOD)
    })
    .await;
    let mut head = block(LOADED + 1, false);
    head.logs = vec![fixture::swap(
      fixture::manager(),
      fixture::key().pool_id(),
      fixture::L,
      fixture::NEAR_UPPER,
    )];
    h.feed.send(FeedEvent::Block(head)).await.unwrap();
    let update = h.chain.changes().await.unwrap();
    assert_eq!(update.touched, HashSet::from([fixture::v4_id()]));
    assert_eq!(h.chain.feed.queued(), vec![fixture::v4_id()]);
  }

  /// A treasury without code takes a plain transfer's gas with no estimate; one with code
  /// gets the estimate plus 20%, capped at `max_gas_limit`, and 21,000 when the estimate
  /// fails.
  #[tokio::test]
  async fn forward_gas_from_treasury_code() {
    let asserter = Asserter::new();
    let provider = mock_provider(&asserter).erased();
    let (from, treasury) = (Address::repeat_byte(1), Address::repeat_byte(9));
    let code = Bytes::from(vec![0xef, 0x01, 0x00]);
    asserter.push_success(&Bytes::new());
    assert_eq!(
      forward_gas(&provider, from, treasury, 1_000_000)
        .await
        .unwrap(),
      TRANSFER_GAS
    );
    asserter.push_success(&code);
    asserter.push_success(&U64::from(50_000));
    assert_eq!(
      forward_gas(&provider, from, treasury, 1_000_000)
        .await
        .unwrap(),
      60_000
    );
    asserter.push_success(&code);
    asserter.push_success(&U64::from(900_000));
    assert_eq!(
      forward_gas(&provider, from, treasury, 1_000_000)
        .await
        .unwrap(),
      1_000_000
    );
    asserter.push_success(&code);
    asserter.push_failure_msg("execution reverted");
    assert_eq!(
      forward_gas(&provider, from, treasury, 1_000_000)
        .await
        .unwrap(),
      TRANSFER_GAS
    );
    assert!(asserter.read_q().is_empty());
  }

  /// Ledger ids continue one past the journal's highest, starting at 1 without a journal.
  #[tokio::test]
  async fn first_id_follows_journal() {
    let dir = std::env::temp_dir().join(format!("evm-first-id-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("trades.jsonl");
    assert_eq!(first_id(&path).await.unwrap(), 1);
    let line = |id: u64| {
      let entry = JournalEntry {
        ts: "2026-11-08T00:00:00Z".into(),
        event: TradeEvent {
          id,
          kind: EntryKind::Trade,
          state: TradeState::Sent,
          block: 1,
          account: "0xabc".into(),
          nonce: 0,
          tx: None,
          detail: TradeDetail::default(),
        },
      };
      serde_json::to_string(&entry).unwrap()
    };
    std::fs::write(&path, format!("{}\n{}\n{}\n", line(4), line(9), line(7))).unwrap();
    assert_eq!(first_id(&path).await.unwrap(), 10);
  }

  /// The feed lag is the time past the block's timestamp, never negative.
  #[test]
  fn feed_lag_from_block_time() {
    let now = UNIX_EPOCH + Duration::from_millis(100_250);
    assert_eq!(lag_ms(100, now), 250);
    assert_eq!(lag_ms(101, now), 0);
  }
}
