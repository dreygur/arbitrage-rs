//! Block feed: new heads over WebSocket, each block's logs fetched whole by block hash.
//!
//! One task owns the subscription and sends every event on one channel, so a block's logs
//! arrive together and in order. Tick window refetches run on the same task between heads,
//! pinned to the last block it delivered: the window then lands in the channel right after
//! that block and before the next, and no `ModifyLiquidity` applied locally is lost under it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::{Filter, Header, Log};
use alloy::sol_types::SolEvent;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::chain::evm::bindings::{IPoolManager, IUniswapV2Pair};
use crate::chain::evm::ticks::{BoundedTicks, fetch_window};
use crate::chain::evm::venue::v4::V4Key;
use crate::error::Result;
use crate::types::Id;

/// Events buffered before the feed waits for the bot to catch up.
const EVENTS: usize = 64;
/// Pause before reconnecting a dropped or failed WebSocket.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// One block's header values and the tracked pools' logs in it.
#[derive(Clone, Debug)]
pub struct FeedBlock {
  /// Block number.
  pub number: u64,
  /// Block hash the logs were fetched by.
  pub hash: B256,
  /// Base fee per gas, zero before London.
  pub base_fee: u128,
  /// Gas used by the block.
  pub gas_used: u64,
  /// Gas limit of the block.
  pub gas_limit: u64,
  /// Logs of tracked pools, as the node returned them.
  pub logs: Vec<Log>,
  /// Whether blocks may have been missed (a reconnect, or a number that does not follow the
  /// last one), so state must be reloaded.
  pub gap: bool,
}

/// What the feed delivers to the bot.
#[derive(Clone, Debug)]
pub enum FeedEvent {
  /// A new block.
  Block(FeedBlock),
  /// A refetched tick window for the V4 pool with this id.
  Window(Id, BoundedTicks),
}

/// A tick window refetch the bot asked for.
struct Refetch {
  /// Core id of the pool.
  id: Id,
  /// The pool key.
  key: V4Key,
  /// Tick to center the window on.
  tick: i32,
}

/// Locks the set of pools with a refetch queued, recovering it if a holder panicked.
fn lock(pending: &Mutex<HashSet<Id>>) -> MutexGuard<'_, HashSet<Id>> {
  pending.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether block `number` does not directly follow `last`: a skipped, repeated or earlier
/// number means a missed block, a reorg or a refork. The first block is never a gap.
pub fn is_gap(last: Option<u64>, number: u64) -> bool {
  last.is_some_and(|last| last.checked_add(1) != Some(number))
}

/// Whether `log` is a tracked pool's: any `Sync` (the filter already limits addresses), or a
/// PoolManager event whose pool id is in `v4_ids`.
pub fn wanted(log: &Log, v4_ids: &HashSet<B256>) -> bool {
  if log.topic0() == Some(&IUniswapV2Pair::Sync::SIGNATURE_HASH) {
    return true;
  }
  log.topics().get(1).is_some_and(|id| v4_ids.contains(id))
}

/// The bot's side of the feed: asks for tick window refetches and stops the feed when dropped.
pub struct FeedHandle {
  /// Refetch requests to the feed task.
  requests: mpsc::UnboundedSender<Refetch>,
  /// Pools with a refetch queued or running, so one pool is fetched once at a time.
  pending: Arc<Mutex<HashSet<Id>>>,
  /// The feed task, aborted on drop.
  task: Option<JoinHandle<()>>,
}

impl FeedHandle {
  /// A handle with no task yet and the receiving end of its requests.
  fn new() -> (Self, mpsc::UnboundedReceiver<Refetch>) {
    let (requests, receiver) = mpsc::unbounded_channel();
    let handle = Self {
      requests,
      pending: Arc::new(Mutex::new(HashSet::new())),
      task: None,
    };
    (handle, receiver)
  }

  /// Asks for the tick window of pool `id` around `tick`, delivered later as
  /// `FeedEvent::Window`. A pool already waiting for one is not asked for twice.
  pub fn refetch(&self, id: Id, key: V4Key, tick: i32) {
    if !lock(&self.pending).insert(id.clone()) {
      return;
    }
    if self.requests.send(Refetch { id, key, tick }).is_err() {
      warn!("feed stopped, refetch dropped");
    }
  }
}

impl Drop for FeedHandle {
  /// Stops the feed task with the handle.
  fn drop(&mut self) {
    if let Some(task) = self.task.take() {
      task.abort();
    }
  }
}

/// The feed task's state.
pub struct Feed<P> {
  /// HTTP provider for logs and tick windows.
  provider: P,
  /// Contracts whose logs are fetched: tracked V2 pairs and the PoolManager.
  addresses: Vec<Address>,
  /// Tracked V4 pool ids; PoolManager logs of other pools are dropped.
  v4_ids: HashSet<B256>,
  /// Multicall3 address for tick window reads.
  multicall: Address,
  /// StateView address for tick window reads.
  state_view: Address,
  /// Bitmap words fetched each side of the current word.
  tick_words: u16,
  /// Events to the bot.
  events: mpsc::Sender<FeedEvent>,
  /// Refetch requests from the bot.
  requests: mpsc::UnboundedReceiver<Refetch>,
  /// Pools with a refetch queued or running, shared with the handle.
  pending: Arc<Mutex<HashSet<Id>>>,
  /// Number of the last block delivered.
  last: Option<u64>,
  /// Hash of the last block delivered, the block refetches are pinned to.
  last_hash: Option<B256>,
  /// Whether the WebSocket reconnected since the last block delivered.
  reconnected: bool,
}

impl<P: Provider + Send + Sync + 'static> Feed<P> {
  /// Fetches the tracked logs of the block `header` names and delivers it.
  ///
  /// A failed fetch skips the block; the next one then counts as a gap. Returns false once
  /// the bot is gone.
  async fn on_head(&mut self, header: Header) -> bool {
    let number = header.inner.number;
    let hash = header.hash;
    let filter = Filter::new()
      .address(self.addresses.clone())
      .event_signature(vec![
        IUniswapV2Pair::Sync::SIGNATURE_HASH,
        IPoolManager::Swap::SIGNATURE_HASH,
        IPoolManager::ModifyLiquidity::SIGNATURE_HASH,
      ])
      .at_block_hash(hash);
    let logs = match self.provider.get_logs(&filter).await {
      Ok(logs) => logs,
      Err(e) => {
        warn!(block = number, error = %e, "logs unavailable, skipping block");
        return true;
      }
    };
    let block = FeedBlock {
      number,
      hash,
      base_fee: header
        .inner
        .base_fee_per_gas
        .map(u128::from)
        .unwrap_or_default(),
      gas_used: header.inner.gas_used,
      gas_limit: header.inner.gas_limit,
      logs: logs
        .into_iter()
        .filter(|log| wanted(log, &self.v4_ids))
        .collect(),
      gap: self.reconnected || is_gap(self.last, number),
    };
    if self.events.send(FeedEvent::Block(block)).await.is_err() {
      return false;
    }
    self.reconnected = false;
    self.last = Some(number);
    self.last_hash = Some(hash);
    true
  }

  /// Fetches the requested tick window at the last delivered block and delivers it.
  ///
  /// A failed fetch is dropped with a warning; the bot asks again while the pool stays near
  /// its edge. Returns false once the bot is gone.
  async fn on_refetch(&mut self, request: Refetch) -> bool {
    let fetched = match self.last_hash {
      Some(hash) => fetch_window(
        &self.provider,
        self.multicall,
        self.state_view,
        BlockId::hash(hash),
        &request.key,
        request.tick,
        self.tick_words,
      )
      .await
      .map_err(|e| warn!(pool = %request.id, error = %e, "tick window refetch failed"))
      .ok(),
      None => None,
    };
    lock(&self.pending).remove(&request.id);
    match fetched {
      Some(ticks) => self
        .events
        .send(FeedEvent::Window(request.id, ticks))
        .await
        .is_ok(),
      None => true,
    }
  }

  /// Follows one WebSocket connection until it drops. Returns false once the bot is gone.
  async fn session(&mut self, ws_url: &str) -> Result<bool> {
    let ws = ProviderBuilder::new()
      .connect_ws(WsConnect::new(ws_url))
      .await?;
    let mut heads = ws.subscribe_blocks().await?.into_stream();
    loop {
      tokio::select! {
        head = heads.next() => match head {
          Some(header) => {
            if !self.on_head(header).await {
              return Ok(false);
            }
          }
          None => return Ok(true),
        },
        Some(request) = self.requests.recv() => {
          if !self.on_refetch(request).await {
            return Ok(false);
          }
        }
      }
    }
  }

  /// Runs sessions until the bot is gone, reconnecting after each drop or failure and
  /// marking the next block as a gap.
  async fn run(mut self, ws_url: String) {
    loop {
      match self.session(&ws_url).await {
        Ok(false) => return,
        Ok(true) => warn!("block subscription ended, reconnecting"),
        Err(e) => warn!(error = %e, "block subscription failed, reconnecting"),
      }
      self.reconnected = true;
      tokio::time::sleep(RECONNECT_DELAY).await;
    }
  }

  /// Starts the feed task over `ws_url` and returns its handle and event stream.
  ///
  /// `addresses` are the tracked pairs and the PoolManager; `v4_ids` the tracked V4 pools;
  /// `multicall`, `state_view` and `tick_words` are what tick window refetches read with.
  pub fn spawn(
    ws_url: String,
    provider: P,
    addresses: Vec<Address>,
    v4_ids: HashSet<B256>,
    multicall: Address,
    state_view: Address,
    tick_words: u16,
  ) -> (FeedHandle, mpsc::Receiver<FeedEvent>) {
    let (mut handle, requests) = FeedHandle::new();
    let (events, receiver) = mpsc::channel(EVENTS);
    let feed = Self {
      provider,
      addresses,
      v4_ids,
      multicall,
      state_view,
      tick_words,
      events,
      requests,
      pending: handle.pending.clone(),
      last: None,
      last_hash: None,
      reconnected: false,
    };
    handle.task = Some(tokio::spawn(feed.run(ws_url)));
    (handle, receiver)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use alloy::primitives::LogData;

  /// A PoolManager log with `topics` and no data.
  fn log(topics: Vec<B256>) -> Log {
    Log {
      inner: alloy::primitives::Log {
        address: Address::repeat_byte(0x44),
        data: LogData::new_unchecked(topics, Default::default()),
      },
      ..Log::default()
    }
  }

  /// Only a number that follows the last one directly is not a gap.
  #[test]
  fn gap_detected_on_skip_repeat_or_rewind() {
    assert!(!is_gap(None, 7));
    assert!(!is_gap(Some(7), 8));
    assert!(is_gap(Some(7), 9));
    assert!(is_gap(Some(7), 7));
    assert!(is_gap(Some(7), 3));
    assert!(is_gap(Some(u64::MAX), 0));
  }

  /// `Sync` logs pass; PoolManager logs pass only for tracked pool ids.
  #[test]
  fn untracked_v4_logs_dropped() {
    let tracked = B256::repeat_byte(1);
    let ids = HashSet::from([tracked]);
    let swap = IPoolManager::Swap::SIGNATURE_HASH;
    let modify = IPoolManager::ModifyLiquidity::SIGNATURE_HASH;
    assert!(wanted(
      &log(vec![IUniswapV2Pair::Sync::SIGNATURE_HASH]),
      &ids
    ));
    assert!(wanted(&log(vec![swap, tracked]), &ids));
    assert!(wanted(&log(vec![modify, tracked]), &ids));
    assert!(!wanted(&log(vec![swap, B256::repeat_byte(2)]), &ids));
    assert!(!wanted(&log(vec![swap]), &ids));
  }

  /// A pool with a refetch pending is not queued again until the feed finishes it.
  #[test]
  fn refetch_queued_once_while_pending() {
    let (handle, mut requests) = FeedHandle::new();
    let key = V4Key {
      currency0: Address::ZERO,
      currency1: Address::repeat_byte(0x11),
      fee: 3000,
      tick_spacing: 60,
      hooks: Address::ZERO,
    };
    let id = Id::new(key.pool_id().to_vec());
    handle.refetch(id.clone(), key, 100);
    handle.refetch(id.clone(), key, 200);
    assert_eq!(requests.try_recv().unwrap().tick, 100);
    assert!(requests.try_recv().is_err());
    lock(&handle.pending).remove(&id);
    handle.refetch(id, key, 300);
    assert_eq!(requests.try_recv().unwrap().tick, 300);
  }
}
