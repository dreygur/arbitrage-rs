//! In memory state of the tracked pools, kept current from each block's logs.
//!
//! The bot task is the only writer. Readers get `Arc` snapshots; an update builds a new value
//! for the pool it touches (`Arc::make_mut` clones whenever a snapshot is still held), so a
//! snapshot handed out never changes under its reader.

use std::collections::HashMap;
use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, U160};
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use tracing::warn;

use crate::chain::evm::bindings::{
  Call3, IERC20, IPoolManager, IStateView, IUniswapV2Pair, Result3,
};
use crate::chain::evm::config::{EvmConfig, EvmVenue};
use crate::chain::evm::convert::to_id;
use crate::chain::evm::feed::FeedBlock;
use crate::chain::evm::multicall::{aggregate, decode, read};
use crate::chain::evm::pairs::Tracked;
use crate::chain::evm::ticks::{BoundedTicks, WindowRequest, fetch_windows};
use crate::chain::evm::venue::v2::V2Pool;
use crate::chain::evm::venue::v4::V4Pool;
use crate::error::{Error, Result};
use crate::types::{Amount, Id, Update};
use crate::venue::Venue;

/// Decimals of native ETH, which has no `decimals()` to call.
const NATIVE_DECIMALS: u8 = 18;

/// The latest snapshot of one pool.
#[derive(Clone)]
enum Snapshot {
  /// A V2 pair.
  V2(Arc<V2Pool>),
  /// A V4 pool.
  V4(Arc<V4Pool>),
}

impl Snapshot {
  /// The snapshot as the chain neutral venue the core quotes from.
  fn venue(&self) -> Arc<dyn Venue> {
    match self {
      Self::V2(pool) => pool.clone(),
      Self::V4(pool) => pool.clone(),
    }
  }
}

/// A loaded pool and where it came from.
struct Entry {
  /// The pool as discovery found it.
  tracked: Tracked,
  /// Its latest snapshot.
  snapshot: Snapshot,
}

/// What the first batch says about one pool, before any V4 tick window is read.
enum Head {
  /// A ready V2 snapshot.
  V2(V2Pool),
  /// A V4 pool's slot0 price and tick, in range liquidity and currency decimals.
  V4 {
    /// Square root price.
    sqrt_price: U160,
    /// Current tick.
    tick: i32,
    /// In range liquidity.
    liquidity: u128,
    /// Decimals of currency zero and one.
    decimals: (u8, u8),
    /// Slot0's packed protocol fee, both directions.
    protocol_fees: u32,
  },
}

/// The first batch reads for `pool`: reserves for V2; slot0, liquidity and the decimals of
/// each non native currency for V4.
fn head_calls(pool: &Tracked, state_view: Address) -> Vec<Call3> {
  match *pool {
    Tracked::V2 { pair, .. } => vec![read(pair, &IUniswapV2Pair::getReservesCall {})],
    Tracked::V4 { key, .. } => {
      let pool_id = key.pool_id();
      let mut calls = vec![
        read(state_view, &IStateView::getSlot0Call { poolId: pool_id }),
        read(
          state_view,
          &IStateView::getLiquidityCall { poolId: pool_id },
        ),
      ];
      for currency in [key.currency0, key.currency1] {
        if currency != Address::ZERO {
          calls.push(read(currency, &IERC20::decimalsCall {}));
        }
      }
      calls
    }
  }
}

/// Decodes the answers to `head_calls(pool)`, failing for a reverted read or an unset V4
/// price.
fn decode_head(pool: &Tracked, venue: &EvmVenue, results: &[Result3]) -> Result<Head> {
  match *pool {
    Tracked::V2 {
      pair,
      token0,
      token1,
      fee_bps,
      ..
    } => {
      let reserves = decode::<IUniswapV2Pair::getReservesCall>(&results[0])?;
      Ok(Head::V2(V2Pool::new(
        venue.name.clone(),
        to_id(pair),
        to_id(token0),
        to_id(token1),
        Amount::from(reserves.reserve0.to::<u128>()),
        Amount::from(reserves.reserve1.to::<u128>()),
        fee_bps,
      )))
    }
    Tracked::V4 { key, .. } => {
      let slot0 = decode::<IStateView::getSlot0Call>(&results[0])?;
      let liquidity = decode::<IStateView::getLiquidityCall>(&results[1])?;
      let mut answers = results[2..].iter();
      let mut decimals = [NATIVE_DECIMALS; 2];
      for (places, currency) in decimals.iter_mut().zip([key.currency0, key.currency1]) {
        if currency != Address::ZERO {
          let result = answers
            .next()
            .ok_or_else(|| Error::Contract("decimals missing from multicall".into()))?;
          *places = decode::<IERC20::decimalsCall>(result)?;
        }
      }
      if slot0.sqrtPriceX96.is_zero() {
        return Err(Error::Config("price is not set".into()));
      }
      Ok(Head::V4 {
        sqrt_price: slot0.sqrtPriceX96,
        tick: slot0.tick.as_i32(),
        liquidity,
        decimals: (decimals[0], decimals[1]),
        protocol_fees: slot0.protocolFee.to::<u32>(),
      })
    }
  }
}

/// Reads every tracked pool at `block`: reserves for V2; slot0, liquidity, currency decimals
/// and the tick window for V4.
///
/// Three batches in all, every V4 window read together. Each pool stands alone: a failed or
/// undecodable read, an unset price or a key the SDK refuses skips that pool
/// with a warning naming it, so a reload drops a pool that went bad. Only a failed request
/// fails the whole read.
async fn read_pools<P: Provider>(
  provider: &P,
  tracked: &[Tracked],
  evm: &EvmConfig,
  base: Address,
  block: BlockId,
) -> Result<Vec<Entry>> {
  let calls: Vec<Vec<Call3>> = tracked
    .iter()
    .map(|pool| head_calls(pool, evm.state_view))
    .collect();
  let counts: Vec<usize> = calls.iter().map(Vec::len).collect();
  let results = aggregate(provider, evm.multicall, block, calls.concat()).await?;
  let mut offset = 0;
  let mut heads = Vec::with_capacity(tracked.len());
  let mut requests = Vec::new();
  for (&pool, count) in tracked.iter().zip(counts) {
    let answers = &results[offset..offset + count];
    offset += count;
    let venue = &evm.venues[pool.venue()];
    match decode_head(&pool, venue, answers) {
      Ok(head) => {
        if let (Head::V4 { tick, .. }, Tracked::V4 { key, .. }) = (&head, pool) {
          requests.push(WindowRequest {
            key,
            tick: *tick,
            words: venue.tick_words.unwrap_or_default(),
          });
        }
        heads.push((pool, head));
      }
      Err(e) => warn!(pool = %pool.id(), error = %e, "skipping pool"),
    }
  }
  let windows = fetch_windows(provider, evm.multicall, evm.state_view, block, &requests).await?;
  let mut windows = windows.into_iter();
  let base_id = to_id(base);
  let mut entries = Vec::with_capacity(heads.len());
  for (pool, head) in heads {
    let snapshot = match (head, pool) {
      (Head::V2(snapshot), _) => Ok(Snapshot::V2(Arc::new(snapshot))),
      (
        Head::V4 {
          sqrt_price,
          tick,
          liquidity,
          decimals,
          protocol_fees,
        },
        Tracked::V4 { key, venue },
      ) => windows
        .next()
        .unwrap_or_else(|| Err(Error::Contract("tick window missing".into())))
        .and_then(|ticks| {
          V4Pool::new(
            evm.venues[venue].name.clone(),
            pool.id(),
            base_id.clone(),
            key,
            protocol_fees,
            decimals,
            sqrt_price,
            liquidity,
            tick,
            ticks,
          )
        })
        .map(|built| Snapshot::V4(Arc::new(built))),
      (Head::V4 { .. }, Tracked::V2 { .. }) => {
        Err(Error::Contract("v4 answers for a v2 pair".into()))
      }
    };
    match snapshot {
      Ok(snapshot) => entries.push(Entry {
        tracked: pool,
        snapshot,
      }),
      Err(e) => warn!(pool = %pool.id(), error = %e, "skipping pool"),
    }
  }
  Ok(entries)
}

/// The tracked pools' latest state and the block it is current at.
pub struct State {
  /// Every pool discovery chose, including any skipped at the last load.
  tracked: Vec<Tracked>,
  /// Adapter settings: venues, StateView and Multicall3.
  evm: EvmConfig,
  /// Wrapped native token, the id native currency is reported as.
  base: Address,
  /// Loaded pools in discovery order.
  entries: Vec<Entry>,
  /// Entry index by V2 pair address.
  pairs: HashMap<Address, usize>,
  /// Entry index by V4 pool id.
  pools: HashMap<B256, usize>,
  /// Entry index by core pool id.
  ids: HashMap<Id, usize>,
  /// Last block applied or loaded.
  block: u64,
}

impl State {
  /// Empty state over `tracked` at `block`, filled by `reload_all`.
  fn build(tracked: Vec<Tracked>, evm: EvmConfig, base: Address, block: u64) -> Self {
    Self {
      tracked,
      evm,
      base,
      entries: Vec::new(),
      pairs: HashMap::new(),
      pools: HashMap::new(),
      ids: HashMap::new(),
      block,
    }
  }

  /// Replaces every entry and rebuilds the lookups.
  fn replace(&mut self, entries: Vec<Entry>, block: u64) {
    self.pairs.clear();
    self.pools.clear();
    self.ids.clear();
    for (index, entry) in entries.iter().enumerate() {
      match entry.tracked {
        Tracked::V2 { pair, .. } => {
          self.pairs.insert(pair, index);
        }
        Tracked::V4 { key, .. } => {
          self.pools.insert(key.pool_id(), index);
        }
      }
      self.ids.insert(entry.tracked.id(), index);
    }
    self.entries = entries;
    self.block = block;
  }

  /// Rereads every tracked pool at the latest block, replacing every snapshot.
  ///
  /// Blocks the feed delivers afterwards up to that block are already reflected and ignored.
  pub async fn reload_all<P: Provider>(&mut self, provider: &P) -> Result<()> {
    let block = provider.get_block_number().await?;
    let entries = read_pools(
      provider,
      &self.tracked,
      &self.evm,
      self.base,
      BlockId::number(block),
    )
    .await?;
    self.replace(entries, block);
    Ok(())
  }

  /// Reads every tracked pool at the latest block.
  ///
  /// `base` is the wrapped native token; V4 pools report native currency as it.
  pub async fn load<P: Provider>(
    provider: &P,
    tracked: Vec<Tracked>,
    evm: &EvmConfig,
    base: Address,
  ) -> Result<Self> {
    let mut state = Self::build(tracked, evm.clone(), base, 0);
    state.reload_all(provider).await?;
    Ok(state)
  }

  /// Applies one log to the pool it concerns, returning that pool's id.
  ///
  /// Logs from untracked addresses or pools, and logs that do not decode, are ignored.
  fn apply_log(&mut self, log: &Log) -> Result<Option<Id>> {
    let Some(&topic) = log.topic0() else {
      return Ok(None);
    };
    let index = if topic == IUniswapV2Pair::Sync::SIGNATURE_HASH {
      self.pairs.get(&log.address()).copied()
    } else {
      log
        .topics()
        .get(1)
        .and_then(|id| self.pools.get(id))
        .copied()
        .filter(|&i| self.evm.venues[self.entries[i].tracked.venue()].address == log.address())
    };
    let Some(index) = index else {
      return Ok(None);
    };
    let data = log.data();
    match &mut self.entries[index].snapshot {
      Snapshot::V2(pool) if topic == IUniswapV2Pair::Sync::SIGNATURE_HASH => {
        let Ok(sync) = IUniswapV2Pair::Sync::decode_log_data(data) else {
          return Ok(None);
        };
        Arc::make_mut(pool).set_reserves(
          Amount::from(sync.reserve0.to::<u128>()),
          Amount::from(sync.reserve1.to::<u128>()),
        );
      }
      Snapshot::V4(pool) if topic == IPoolManager::Swap::SIGNATURE_HASH => {
        let Ok(swap) = IPoolManager::Swap::decode_log_data(data) else {
          return Ok(None);
        };
        Arc::make_mut(pool).apply_swap(swap.sqrtPriceX96, swap.liquidity, swap.tick.as_i32());
      }
      Snapshot::V4(pool) if topic == IPoolManager::ModifyLiquidity::SIGNATURE_HASH => {
        let Ok(modify) = IPoolManager::ModifyLiquidity::decode_log_data(data) else {
          return Ok(None);
        };
        let delta = i128::try_from(modify.liquidityDelta).map_err(|_| Error::Overflow)?;
        Arc::make_mut(pool).apply_modify(
          modify.tickLower.as_i32(),
          modify.tickUpper.as_i32(),
          delta,
        )?;
      }
      _ => return Ok(None),
    }
    Ok(Some(self.entries[index].tracked.id()))
  }

  /// Applies the logs of `feed_block` in log index order and reports the pools they touched.
  ///
  /// `Update.reload` is the one signal the bot acts on: it is set for a feed gap (a reconnect,
  /// reorg or refork), a removed log, a block that skips ahead, or a log that cannot be
  /// applied, since local state can no longer be trusted. Without a gap, a block at or below
  /// the current one is already reflected (after a reload) and changes nothing.
  pub fn apply(&mut self, feed_block: &FeedBlock) -> Update {
    let block = feed_block.number;
    let logs = &feed_block.logs;
    let mut update = Update {
      block,
      ..Update::default()
    };
    if feed_block.gap {
      self.block = block;
      update.reload = true;
      return update;
    }
    if block <= self.block {
      return update;
    }
    let skipped = block > self.block + 1;
    self.block = block;
    if skipped || logs.iter().any(|log| log.removed) {
      update.reload = true;
      return update;
    }
    let mut ordered: Vec<&Log> = logs.iter().collect();
    ordered.sort_by_key(|log| log.log_index);
    for log in ordered {
      match self.apply_log(log) {
        Ok(Some(id)) => {
          update.touched.insert(id);
        }
        Ok(None) => {}
        Err(e) => {
          warn!(block, error = %e, "log could not be applied, reloading");
          update.reload = true;
        }
      }
    }
    update
  }

  /// V4 pools whose current tick is within one bitmap word of their tick window's edge, as
  /// (id, window request with the venue's `tick_words`) for `FeedHandle::refetch`.
  pub fn near_edge(&self) -> Vec<(Id, WindowRequest)> {
    self
      .entries
      .iter()
      .filter_map(|entry| match (&entry.snapshot, entry.tracked) {
        (Snapshot::V4(pool), Tracked::V4 { key, venue }) if pool.near_edge() => {
          let request = WindowRequest {
            key,
            tick: pool.tick(),
            words: self.evm.venues[venue].tick_words.unwrap_or_default(),
          };
          Some((entry.tracked.id(), request))
        }
        _ => None,
      })
      .collect()
  }

  /// Replaces the tick window of the V4 pool `id` with one read at block `fetched`.
  ///
  /// A window older than the current block is dropped: a reload since then made it stale, and
  /// the pool asks again while it stays near its edge. An unknown id (a pool dropped by a
  /// reload) is ignored.
  pub fn apply_window(&mut self, id: &Id, fetched: u64, ticks: BoundedTicks) {
    if fetched < self.block {
      return;
    }
    let Some(&index) = self.ids.get(id) else {
      return;
    };
    if let Snapshot::V4(pool) = &mut self.entries[index].snapshot {
      Arc::make_mut(pool).replace_window(ticks);
    }
  }

  /// Current snapshot of every loaded pool.
  pub fn snapshots(&self) -> Vec<Arc<dyn Venue>> {
    self.entries.iter().map(|e| e.snapshot.venue()).collect()
  }

  /// The block the state is current at, which the engine needs before it frees a trade's
  /// locks.
  pub fn block(&self) -> u64 {
    self.block
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;
  use std::collections::HashSet;

  use alloy::primitives::aliases::{I24, U24, U112};
  use alloy::primitives::{I256, LogData, U64, U160, U256};
  use alloy::providers::mock::Asserter;
  use alloy::sol_types::SolCall;
  use uniswap_v3_sdk::prelude::{TickIndex, get_sqrt_ratio_at_tick};

  use crate::chain::evm::venue::v4::V4Key;
  use crate::testkit::{mock_evm_config, mock_provider, push_multicall};
  use crate::types::Side;

  /// Liquidity of the V4 test pool's one position.
  pub(crate) const L: u128 = 10_000_000_000;
  /// Block the test state is loaded at.
  const LOADED: u64 = 10;
  /// A tick within one word of the upper edge of the window loaded around tick zero.
  pub(crate) const NEAR_UPPER: i32 = 16_000;
  /// Answers of a load where every pool reads cleanly at block 10.
  pub(crate) const GOOD: Answers = Answers {
    block: LOADED,
    protocol_fee: 0,
    reserves: true,
    bitmaps: true,
  };

  /// How the mocked node answers one load.
  #[derive(Clone, Copy)]
  pub(crate) struct Answers {
    /// Latest block number.
    block: u64,
    /// The V4 pool's protocol fee.
    protocol_fee: u32,
    /// Whether the pair's `getReserves` succeeds.
    reserves: bool,
    /// Whether the V4 pool's first bitmap read succeeds.
    bitmaps: bool,
  }

  /// Wrapped native token, token one of the V2 pair.
  fn weth() -> Address {
    Address::repeat_byte(0xee)
  }

  /// The other token of both pools.
  fn token() -> Address {
    Address::repeat_byte(0x11)
  }

  /// The tracked V2 pair.
  fn pair() -> Address {
    Address::repeat_byte(0x22)
  }

  /// Key of the native ETH / token V4 pool at fee 3000, spacing 60.
  pub(crate) fn key() -> V4Key {
    V4Key {
      currency0: Address::ZERO,
      currency1: token(),
      fee: 3000,
      tick_spacing: 60,
      hooks: Address::ZERO,
    }
  }

  /// Core id of the V4 pool.
  pub(crate) fn v4_id() -> Id {
    Id::new(key().pool_id().to_vec())
  }

  /// The V2 pair and the V4 pool, as discovery would return them.
  pub(crate) fn tracked() -> Vec<Tracked> {
    vec![
      Tracked::V2 {
        venue: 0,
        pair: pair(),
        token0: token(),
        token1: weth(),
        fee_bps: 30,
      },
      Tracked::V4 {
        venue: 1,
        key: key(),
      },
    ]
  }

  /// Square root price at `tick`.
  fn price_at(tick: i32) -> U160 {
    get_sqrt_ratio_at_tick(tick.to_i24()).unwrap()
  }

  /// Queues the answers a load reads, shaped by `answers`: block number, the pool batch, and
  /// the tick window batches (one position over -600..600).
  pub(crate) fn push_load(asserter: &Asserter, answers: Answers) {
    asserter.push_success(&U64::from(answers.block));
    let protocol_fee = answers.protocol_fee;
    let reserves = IUniswapV2Pair::getReservesReturn {
      reserve0: U112::from(1_000_000u64),
      reserve1: U112::from(2_000_000u64),
      blockTimestampLast: 0,
    };
    let slot0 = IStateView::getSlot0Return {
      sqrtPriceX96: price_at(0),
      tick: I24::ZERO,
      protocolFee: U24::from(protocol_fee),
      lpFee: U24::from(3000),
    };
    push_multicall(
      asserter,
      vec![
        answers
          .reserves
          .then(|| IUniswapV2Pair::getReservesCall::abi_encode_returns(&reserves)),
        Some(IStateView::getSlot0Call::abi_encode_returns(&slot0)),
        Some(IStateView::getLiquidityCall::abi_encode_returns(&L)),
        Some(IERC20::decimalsCall::abi_encode_returns(&18u8)),
      ],
    );
    let bitmap =
      |bit: usize| IStateView::getTickBitmapCall::abi_encode_returns(&(U256::from(1u8) << bit));
    push_multicall(
      asserter,
      vec![
        answers.bitmaps.then(|| bitmap(246)),
        Some(bitmap(10)),
        Some(IStateView::getTickBitmapCall::abi_encode_returns(
          &U256::ZERO,
        )),
      ],
    );
    if !answers.bitmaps {
      return;
    }
    let tick = |net: i128| {
      IStateView::getTickLiquidityCall::abi_encode_returns(&IStateView::getTickLiquidityReturn {
        liquidityGross: L,
        liquidityNet: net,
      })
    };
    push_multicall(
      asserter,
      vec![Some(tick(L as i128)), Some(tick(-(L as i128)))],
    );
  }

  /// State loaded through `mock_provider` from `answers`, checking every answer was read.
  async fn load_with(answers: Answers) -> State {
    let asserter = Asserter::new();
    push_load(&asserter, answers);
    let state = State::load(
      &mock_provider(&asserter),
      tracked(),
      &mock_evm_config(),
      weth(),
    )
    .await
    .unwrap();
    assert!(asserter.read_q().is_empty(), "load left answers unread");
    state
  }

  /// State loaded through `mock_provider` at block 10 with every pool healthy.
  async fn loaded() -> State {
    load_with(GOOD).await
  }

  /// Applies `logs` as the block `number`, with no feed gap.
  fn apply(state: &mut State, number: u64, logs: &[Log]) -> Update {
    state.apply(&FeedBlock {
      number,
      hash: B256::ZERO,
      base_fee: 0,
      gas_used: 0,
      gas_limit: 0,
      timestamp: 0,
      seen: std::time::Instant::now(),
      logs: logs.to_vec(),
      gap: false,
    })
  }

  /// A log emitted by `address` at position `index` in its block.
  fn log(address: Address, data: LogData, index: u64) -> Log {
    Log {
      inner: alloy::primitives::Log { address, data },
      log_index: Some(index),
      ..Log::default()
    }
  }

  /// A `Sync` log from the tracked pair.
  fn sync(reserve0: u64, reserve1: u64, index: u64) -> Log {
    let event = IUniswapV2Pair::Sync {
      reserve0: U112::from(reserve0),
      reserve1: U112::from(reserve1),
    };
    log(pair(), event.encode_log_data(), index)
  }

  /// A PoolManager `Swap` log for `id` leaving the pool at `tick` with `liquidity`.
  pub(crate) fn swap(from: Address, id: B256, liquidity: u128, tick: i32) -> Log {
    let event = IPoolManager::Swap {
      id,
      sender: Address::ZERO,
      amount0: 0,
      amount1: 0,
      sqrtPriceX96: price_at(tick),
      liquidity,
      tick: I24::try_from(tick).unwrap(),
      fee: U24::from(3000),
    };
    log(from, event.encode_log_data(), 0)
  }

  /// The PoolManager address of the V4 venue.
  pub(crate) fn manager() -> Address {
    mock_evm_config().venues[1].address
  }

  /// Descriptions of every snapshot, V2 first.
  fn described(state: &State) -> Vec<String> {
    state.snapshots().iter().map(|v| v.describe()).collect()
  }

  /// `Sync` logs set the reserves, applied in log index order whatever order they come in.
  #[tokio::test]
  async fn sync_updates_reserves() {
    let mut state = loaded().await;
    assert_eq!(
      described(&state)[0],
      "uniswap_v2 30bps r0=1000000 r1=2000000"
    );
    let update = apply(&mut state, LOADED + 1, &[sync(9, 9, 1), sync(5, 7, 0)]);
    assert_eq!(update.touched, HashSet::from([to_id(pair())]));
    assert_eq!(update.block, LOADED + 1);
    assert!(!update.reload);
    assert_eq!(described(&state)[0], "uniswap_v2 30bps r0=9 r1=9");
  }

  /// A `Swap` log sets the V4 price, liquidity and tick.
  #[tokio::test]
  async fn swap_updates_v4() {
    let mut state = loaded().await;
    let update = apply(
      &mut state,
      LOADED + 1,
      &[swap(manager(), key().pool_id(), L / 2, -300)],
    );
    assert_eq!(update.touched, HashSet::from([v4_id()]));
    let expected = format!("uniswap_v4 30bps p={} L={} t=-300", price_at(-300), L / 2);
    assert_eq!(described(&state)[1], expected);
  }

  /// A `ModifyLiquidity` log in range adds to the active liquidity without any RPC.
  #[tokio::test]
  async fn modify_liquidity_applied_locally() {
    let mut state = loaded().await;
    let amount = crate::types::Amount::from(1_000_000u64);
    let side = crate::types::Side::ZeroForOne;
    let before = state.snapshots()[1].quote_out(amount, side).unwrap();
    let event = IPoolManager::ModifyLiquidity {
      id: key().pool_id(),
      sender: Address::ZERO,
      tickLower: I24::try_from(-1200).unwrap(),
      tickUpper: I24::try_from(1200).unwrap(),
      liquidityDelta: I256::try_from(L).unwrap(),
      salt: B256::ZERO,
    };
    let update = apply(
      &mut state,
      LOADED + 1,
      &[log(manager(), event.encode_log_data(), 0)],
    );
    assert_eq!(update.touched, HashSet::from([v4_id()]));
    assert!(described(&state)[1].contains(&format!(" L={} ", 2 * L)));
    let after = state.snapshots()[1].quote_out(amount, side).unwrap();
    assert!(
      after > before,
      "deeper pool should quote more: {after} vs {before}"
    );
  }

  /// A removed log means a reorg, so state asks for a reload and keeps its values.
  #[tokio::test]
  async fn removed_log_requests_reload() {
    let mut state = loaded().await;
    let before = described(&state);
    let mut removed = sync(5, 7, 0);
    removed.removed = true;
    let update = apply(&mut state, LOADED + 1, &[removed]);
    assert!(update.reload);
    assert!(update.touched.is_empty());
    assert_eq!(described(&state), before);
  }

  /// A block number that skips ahead means missed logs, so state asks for a reload.
  #[tokio::test]
  async fn skipped_block_requests_reload() {
    let mut state = loaded().await;
    let before = described(&state);
    let update = apply(&mut state, LOADED + 2, &[sync(5, 7, 0)]);
    assert!(update.reload);
    assert_eq!(described(&state), before);
    assert!(!apply(&mut state, LOADED + 3, &[]).reload);
  }

  /// A block the state already reflects changes nothing.
  #[tokio::test]
  async fn applied_block_ignored() {
    let mut state = loaded().await;
    let before = described(&state);
    let update = apply(&mut state, LOADED, &[sync(5, 7, 0)]);
    assert!(!update.reload);
    assert!(update.touched.is_empty());
    assert_eq!(described(&state), before);
  }

  /// Logs from untracked pairs, for untracked V4 pools, or claiming a tracked pool id from
  /// another contract change nothing.
  #[tokio::test]
  async fn foreign_log_ignored() {
    let mut state = loaded().await;
    let before = described(&state);
    let mut stranger = sync(5, 7, 0);
    stranger.inner.address = Address::repeat_byte(0x99);
    let logs = [
      stranger,
      swap(manager(), B256::repeat_byte(0x77), L, -300),
      swap(Address::repeat_byte(0x98), key().pool_id(), L, -300),
    ];
    let update = apply(&mut state, LOADED + 1, &logs);
    assert!(update.touched.is_empty());
    assert!(!update.reload);
    assert_eq!(described(&state), before);
  }

  /// A snapshot handed out before an update keeps its values; the update makes a new one.
  #[tokio::test]
  async fn old_arc_unchanged_after_apply() {
    let mut state = loaded().await;
    let old = state.snapshots();
    apply(&mut state, LOADED + 1, &[sync(5, 7, 0)]);
    let new = state.snapshots();
    assert_eq!(old[0].describe(), "uniswap_v2 30bps r0=1000000 r1=2000000");
    assert_eq!(new[0].describe(), "uniswap_v2 30bps r0=5 r1=7");
    assert!(!Arc::ptr_eq(&old[0], &new[0]));
    assert!(Arc::ptr_eq(&old[1], &new[1]));
  }

  /// A V4 pool whose tick nears its window's edge is listed with its key, tick and words.
  #[tokio::test]
  async fn near_edge_listed_for_refetch() {
    let mut state = loaded().await;
    assert!(state.near_edge().is_empty());
    apply(
      &mut state,
      LOADED + 1,
      &[swap(manager(), key().pool_id(), L, NEAR_UPPER)],
    );
    let request = WindowRequest {
      key: key(),
      tick: NEAR_UPPER,
      words: 1,
    };
    assert_eq!(state.near_edge(), vec![(v4_id(), request)]);
  }

  /// A refetched window centered on the current tick replaces the old one.
  #[tokio::test]
  async fn window_event_replaces_ticks() {
    let mut state = loaded().await;
    apply(
      &mut state,
      LOADED + 1,
      &[swap(manager(), key().pool_id(), L, NEAR_UPPER)],
    );
    let (lower, upper) = BoundedTicks::bounds(NEAR_UPPER, key().tick_spacing, 1);
    state.apply_window(
      &v4_id(),
      LOADED + 1,
      BoundedTicks::new(Vec::new(), lower, upper),
    );
    assert!(state.near_edge().is_empty());
    state.apply_window(
      &Id::new(vec![1; 32]),
      LOADED + 1,
      BoundedTicks::new(Vec::new(), 0, 0),
    );
  }

  /// A window read before a reload is dropped; one read at the reloaded block is applied.
  #[tokio::test]
  async fn stale_window_after_reload_dropped() {
    let mut state = loaded().await;
    let reloaded = LOADED + 10;
    let asserter = Asserter::new();
    push_load(
      &asserter,
      Answers {
        block: reloaded,
        ..GOOD
      },
    );
    state.reload_all(&mock_provider(&asserter)).await.unwrap();
    let narrow = || BoundedTicks::new(Vec::new(), -100, 100);
    state.apply_window(&v4_id(), LOADED + 1, narrow());
    assert!(state.near_edge().is_empty());
    state.apply_window(&v4_id(), reloaded, narrow());
    assert_eq!(state.near_edge().len(), 1);
  }

  /// A feed gap asks for a reload even for a block at or below the current one.
  #[tokio::test]
  async fn feed_gap_requests_reload() {
    let mut state = loaded().await;
    let before = described(&state);
    let update = state.apply(&FeedBlock {
      number: LOADED,
      hash: B256::repeat_byte(1),
      base_fee: 0,
      gas_used: 0,
      gas_limit: 0,
      timestamp: 0,
      seen: std::time::Instant::now(),
      logs: vec![sync(5, 7, 0)],
      gap: true,
    });
    assert!(update.reload);
    assert_eq!(described(&state), before);
  }

  /// A V4 pool with a protocol fee is loaded and priced at the combined swap fee: one sided
  /// here, so only zero for one pays more than a pool without it.
  #[tokio::test]
  async fn protocol_fee_pool_loaded() {
    let state = load_with(Answers {
      protocol_fee: 100,
      ..GOOD
    })
    .await;
    let plain = loaded().await;
    assert_eq!(
      described(&state),
      vec![
        "uniswap_v2 30bps r0=1000000 r1=2000000".to_string(),
        format!("uniswap_v4 30bps pf=0x64 p={} L={L} t=0", price_at(0)),
      ]
    );
    let quote = |state: &State, side: Side| {
      state.snapshots()[1]
        .quote_out(Amount::from(1_000_000u64), side)
        .unwrap()
    };
    assert!(quote(&state, Side::ZeroForOne) < quote(&plain, Side::ZeroForOne));
    assert_eq!(
      quote(&state, Side::OneForZero),
      quote(&plain, Side::OneForZero)
    );
  }

  /// A pool whose reads fail is skipped alone, at load and when a reload finds it broken.
  #[tokio::test]
  async fn bad_pool_skipped_at_load() {
    let state = load_with(Answers {
      bitmaps: false,
      ..GOOD
    })
    .await;
    assert_eq!(
      described(&state),
      vec!["uniswap_v2 30bps r0=1000000 r1=2000000"]
    );
    let mut state = loaded().await;
    assert_eq!(state.snapshots().len(), 2);
    let asserter = Asserter::new();
    push_load(
      &asserter,
      Answers {
        block: LOADED + 1,
        reserves: false,
        ..GOOD
      },
    );
    state.reload_all(&mock_provider(&asserter)).await.unwrap();
    assert!(asserter.read_q().is_empty());
    let left = described(&state);
    assert_eq!(left.len(), 1);
    assert!(left[0].starts_with("uniswap_v4 "), "{left:?}");
  }
}
