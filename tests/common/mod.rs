//! Shared harness for the fork tests: env setup, a funded and connected bot, a reset of the
//! fork before each test, pool dumps that open an arbitrage, and blocks mined on demand.
//!
//! Every helper talks to the anvil `make test-fork` starts at `127.0.0.1:8546`: pinned to one
//! fork block and mining only when a transaction arrives or a test calls `evm_mine`, so no
//! test waits on a block timer.

#![allow(
  dead_code,
  reason = "each test crate that includes this module uses only some helpers"
)]

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, U256, address};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol_types::SolCall;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use arbitrage::bot::{Bot, Decision};
use arbitrage::chain::Chain;
use arbitrage::chain::evm::EvmChain;
use arbitrage::chain::evm::accounts::signers;
use arbitrage::chain::evm::bindings::{IERC20, IUniswapV2Factory, IUniswapV2Pair, IWETH};
use arbitrage::chain::evm::config::EvmConfig;
use arbitrage::chain::evm::convert::to_id;
use arbitrage::chain::evm::fund::fund;
use arbitrage::config::NetworkConfig;
use arbitrage::types::{EntryKind, Opportunity, Side, TradeEvent, TradeState, Update, Verdict};
use arbitrage::venue::Venue;

/// The anvil test mnemonic, whose accounts the shipped config names.
pub const ANVIL_MNEMONIC: &str = "test test test test test test test test test test test junk";
/// HTTP endpoint of the test anvil, on the `TEST_PORT` the Makefile starts it on, apart from
/// the `make fork` node on 8545.
pub const RPC_URL: &str = "http://127.0.0.1:8546";
/// WebSocket endpoint of the test anvil.
pub const WS_URL: &str = "ws://127.0.0.1:8546";
/// Upstream the fork follows, the `.env.example` default, used only when `FORK_URL` is unset.
pub const FORK_URL: &str = "https://ethereum-rpc.publicnode.com";
/// Config file the tests load, `config/ethereum.toml`.
pub const CONFIG: &str = "ethereum";
/// Network name handed to `connect`, so the executor record is `.arbitrage-fork-test.json`
/// and a user's `.arbitrage-ethereum.json` is never touched.
pub const NETWORK: &str = "fork-test";
/// Anvil account 5, which dumps into pools and stands in for other traders.
pub const DUMPER: Address = address!("9965507D1a55bcC2695C58ba16FB37d819B0A4dc");
/// An address with no role anywhere, impersonated to call the executor.
pub const STRANGER: Address = address!("5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a");
/// Wrapped ether.
pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// USD Coin.
pub const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
/// Dai.
pub const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
/// SushiSwap V2 factory.
pub const SUSHISWAP: Address = address!("C0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac");
/// UniswapV2Router02, a test constant for its pure `getAmountOut`.
pub const ROUTER: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
/// Uniswap V4Quoter, a test constant called with `eth_call`.
pub const QUOTER: Address = address!("52F0E24D1c21C8A0cB1e5a5dD6198556BD9E1203");
/// One ether in wei.
pub const ETHER: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// How long a freshly mined block gets to come through the feed before another is mined.
pub const FEED_WAIT: Duration = Duration::from_secs(5);
/// Seconds between the timestamps of blocks the test fork mines, fixed so they repeat run to
/// run.
pub const BLOCK_SPACING: u64 = 1;
/// Longest wait for one block or one receipt, a safety net only: blocks are mined on demand,
/// so a wait this long means something hung.
pub const WAIT: Duration = Duration::from_secs(60);
/// Blocks to follow a trade or a transfer before giving up.
pub const BLOCKS: usize = 10;
/// Gas limit of every transaction the tests send themselves, so none is estimated against
/// state its predecessors have not changed yet.
pub const GAS: u64 = 300_000;
/// Fee cap of the tests' own transactions, high enough for a raised base fee.
pub const FEE_CAP: u128 = 1_000_000_000_000;
/// Tip of an ordinary dump, the bot's own `priority_fee`.
pub const TIP: u128 = 1_000_000_000;
/// ETH the dumper holds beyond what it wraps, for gas at [`FEE_CAP`].
pub const GAS_FUNDS: U256 = U256::from_limbs([10_000_000_000_000_000_000, 0, 0, 0]);
/// Where each context's journal goes, a fresh file per context.
pub const JOURNALS: &str = "target/test-journals";
/// Decisions buffered for a test before the bot drops them.
pub const DECISIONS: usize = 4096;
/// Pause between polls of the node or the engine.
pub const POLL: Duration = Duration::from_millis(100);

/// Guards the one time env setup.
static SETUP: Once = Once::new();
/// Distinguishes the journals of contexts built in one process.
static JOURNAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Sets the env the adapter reads, once per process: the anvil mnemonic and the local node
/// endpoints, overriding a `.env` that `make` exports, and `FORK_URL` when unset. Also starts
/// a test log writer that `RUST_LOG` turns on.
pub fn setup() {
  SETUP.call_once(|| {
    // SAFETY: runs once, before any test thread reads the environment.
    unsafe {
      std::env::set_var("MNEMONIC_1", ANVIL_MNEMONIC);
      std::env::set_var("RPC_URL", RPC_URL);
      std::env::set_var("WS_URL", WS_URL);
      if std::env::var("FORK_URL").is_err() {
        std::env::set_var("FORK_URL", FORK_URL);
      }
    }
    let _ = tracing_subscriber::fmt()
      .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
      .with_test_writer()
      .try_init();
  });
}

/// A plain HTTP connection to the fork; transactions it sends are signed by anvil, which
/// holds the test accounts unlocked or impersonated.
pub async fn provider() -> DynProvider {
  ProviderBuilder::new()
    .connect(RPC_URL)
    .await
    .expect("test fork not reachable, run make test-fork")
    .erased()
}

/// Calls the node method `method` with `params`, panicking on an error reply.
pub async fn rpc<P>(provider: &DynProvider, method: &'static str, params: P) -> Value
where
  P: serde::Serialize + Clone + std::fmt::Debug + Send + Sync + Unpin + 'static,
{
  provider
    .raw_request::<_, Value>(method.into(), params)
    .await
    .unwrap_or_else(|e| panic!("{method}: {e}"))
}

/// Resets the test anvil to the block it was pinned at, dropping every change a test made,
/// turns automine back on, which a test may have stopped, and spaces new blocks exactly one
/// second apart.
///
/// A reset, unlike `evm_revert`, keeps the remote state anvil already fetched: the old fork
/// backend writes its cache to disk and the new one reads it back, while a revert discards
/// everything fetched after its snapshot. The fork URL must be the one anvil was started
/// with, since that cache is kept per endpoint. Fixed block spacing makes every mined block's
/// timestamp, and so the beacon roots slot its system call writes, the same in every run,
/// which a later run then finds in the disk cache instead of asking the public node for
/// state it may no longer serve.
pub async fn reset(provider: &DynProvider) {
  let info = rpc(provider, "anvil_nodeInfo", ()).await;
  let block = info["forkConfig"]["forkBlockNumber"]
    .as_u64()
    .expect("test fork is not pinned to a block");
  let url = std::env::var("FORK_URL").expect("FORK_URL");
  let forking = json!({ "forking": { "jsonRpcUrl": url, "blockNumber": block } });
  rpc(provider, "anvil_reset", (forking,)).await;
  rpc(provider, "evm_setAutomine", (true,)).await;
  rpc(
    provider,
    "anvil_setBlockTimestampInterval",
    (BLOCK_SPACING,),
  )
  .await;
}

/// Runs `body` on the test fork reset to its pinned block, so no test sees another's dumps,
/// trades or executors. A test that panics needs no cleanup, since the next one resets.
pub async fn forked<F, Fut>(body: F)
where
  F: FnOnce() -> Fut,
  Fut: Future<Output = ()>,
{
  setup();
  reset(&provider().await).await;
  body().await;
}

/// The shipped ethereum config.
pub fn config() -> NetworkConfig {
  NetworkConfig::load(CONFIG).expect("config/ethereum.toml")
}

/// The configured accounts' addresses, in config order; account 0 is the owner.
pub fn accounts(cfg: &NetworkConfig) -> Vec<Address> {
  signers(&cfg.accounts)
    .expect("accounts")
    .iter()
    .map(|signer| signer.address())
    .collect()
}

/// The configured treasury.
pub fn treasury(cfg: &NetworkConfig) -> Address {
  cfg.treasury.parse().expect("treasury")
}

/// A journal path no earlier context used.
fn journal_path() -> std::path::PathBuf {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0, |d| d.as_nanos());
  let seq = JOURNAL_SEQ.fetch_add(1, Ordering::SeqCst);
  std::path::PathBuf::from(format!(
    "{JOURNALS}/{}-{nanos}-{seq}.jsonl",
    std::process::id()
  ))
}

/// A bot connected to the fork.
pub struct Ctx {
  /// The config the chain was built from, overrides applied.
  pub cfg: NetworkConfig,
  /// The connected chain.
  pub chain: Arc<EvmChain>,
  /// The bot over `chain`.
  pub bot: Bot<EvmChain>,
  /// A plain connection for the test's own reads and sends.
  pub provider: DynProvider,
  /// Trade events from the engine.
  pub events: mpsc::Receiver<TradeEvent>,
  /// Every trade event drained from `events` so far, in arrival order.
  pub seen: Vec<TradeEvent>,
  /// The bot's decisions.
  pub decisions: mpsc::Receiver<Decision>,
}

/// Waits until the engine has taken the head of `block`, so its fee and locks are current
/// when the bot steps.
async fn engine_took(ctx: &Ctx, block: u64) {
  let deadline = Instant::now() + WAIT;
  while ctx.chain.status().block < block {
    assert!(Instant::now() < deadline, "engine never took block {block}");
    tokio::time::sleep(POLL).await;
  }
}

/// Folds `update` into `merged`: its touched pools, its block, and any reload or restart.
fn merge(merged: &mut Update, update: Update) {
  merged.touched.extend(update.touched);
  merged.reload |= update.reload;
  merged.restart |= update.restart;
  merged.block = update.block;
}

/// Takes the next change the chain has queued and waits until the engine has taken its
/// head. The change must already be mined; the timeout is a safety net.
pub async fn next_change(ctx: &Ctx) -> Update {
  let update = tokio::time::timeout(WAIT, ctx.chain.changes())
    .await
    .expect("no block from the fork")
    .expect("changes");
  engine_took(ctx, update.block).await;
  update
}

/// Pulls changes until the chain has applied the already mined `block`, at least one, and
/// returns them merged: every touched pool, the last block, and whether any asked for a
/// reload or restart.
pub async fn catch_up(ctx: &Ctx, block: u64) -> Update {
  let mut merged = Update::default();
  loop {
    merge(&mut merged, next_change(ctx).await);
    if merged.block >= block {
      return merged;
    }
  }
}

/// Mines empty blocks with `evm_mine`, one every [`FEED_WAIT`], until the chain delivers a
/// change, then catches up to the last block mined. The engine also runs the maintenance
/// held back for the block before.
///
/// The feed subscribes to heads in its own task, so right after a connect a mined block can
/// come before the subscription and never be delivered; the next one then arrives. Only the
/// re-mining is on a timer: `changes` runs to completion, never dropped halfway through
/// applying a block, and [`WAIT`] cancels it only when no block ever arrives.
pub async fn advance(ctx: &Ctx) -> Update {
  rpc(&ctx.provider, "evm_mine", ()).await;
  let remine = async {
    loop {
      tokio::time::sleep(FEED_WAIT).await;
      rpc(&ctx.provider, "evm_mine", ()).await;
    }
  };
  let first = tokio::select! {
    first = tokio::time::timeout(WAIT, ctx.chain.changes()) => {
      first.expect("the feed delivered no block").expect("changes")
    }
    () = remine => unreachable!("re-mining never ends"),
  };
  engine_took(ctx, first.block).await;
  let mined = ctx.provider.get_block_number().await.expect("block number");
  let block = first.block;
  let mut merged = Update::default();
  merge(&mut merged, first);
  if block < mined {
    merge(&mut merged, catch_up(ctx, mined).await);
  }
  merged
}

/// Loads the shipped config with a fresh journal under `target/test-journals/`, applies
/// `overrides`, funds the accounts and the dumper (dropping their EIP 7702 delegations),
/// connects the chain and a bot over it, and mines until the feed delivers a block, so the
/// feed is live and the engine has a head and a fee before the test starts.
pub async fn ctx(overrides: impl Fn(&mut NetworkConfig)) -> Ctx {
  setup();
  let mut cfg = config();
  cfg.journal = journal_path();
  overrides(&mut cfg);
  let provider = provider().await;
  let fork = EvmConfig::parse(&cfg)
    .expect("adapter config")
    .fork
    .expect("adapter.fork section");
  let mut funded = accounts(&cfg);
  funded.push(DUMPER);
  fund(&provider, &fork, treasury(&cfg), &funded)
    .await
    .expect("fund");
  let (chain, events) = EvmChain::connect(&cfg, NETWORK).await.expect("connect");
  let chain = Arc::new(chain);
  let (decisions_tx, decisions) = mpsc::channel(DECISIONS);
  let bot = Bot::new(chain.clone(), cfg.live.clone(), decisions_tx);
  let ctx = Ctx {
    cfg,
    chain,
    bot,
    provider,
    events,
    seen: Vec::new(),
    decisions,
  };
  advance(&ctx).await;
  ctx
}

/// The executor the chain trades through.
pub fn executor(ctx: &Ctx) -> Address {
  ctx.chain.status().executor.parse().expect("executor")
}

/// The WETH balance of `account`.
pub async fn weth_of(provider: &DynProvider, account: Address) -> U256 {
  IERC20::new(WETH, provider)
    .balanceOf(account)
    .call()
    .await
    .expect("balanceOf")
}

/// The SushiSwap pair of WETH and `token`.
pub async fn sushi_pair(provider: &DynProvider, token: Address) -> Address {
  let pair = IUniswapV2Factory::new(SUSHISWAP, provider)
    .getPair(WETH, token)
    .call()
    .await
    .expect("getPair");
  assert_ne!(pair, Address::ZERO, "no sushiswap pair for {token}");
  pair
}

/// The reserves of `pair` as `(reserve of token_in, reserve of the other, token_in is token0)`.
pub async fn reserves(
  provider: &DynProvider,
  pair: Address,
  token_in: Address,
) -> (U256, U256, bool) {
  let contract = IUniswapV2Pair::new(pair, provider);
  let token0 = contract.token0().call().await.expect("token0");
  let r = contract.getReserves().call().await.expect("getReserves");
  let (r0, r1) = (U256::from(r.reserve0), U256::from(r.reserve1));
  if token0 == token_in {
    (r0, r1, true)
  } else {
    (r1, r0, false)
  }
}

/// A tenth of the WETH `pair` holds, the dump that opens an arbitrage on it.
pub async fn tenth_of_weth(provider: &DynProvider, pair: Address) -> U256 {
  reserves(provider, pair, WETH).await.0 / U256::from(10u8)
}

/// Sends a call of `data` with `value` from `from` to `to` at tip `tip`, with fixed gas and
/// fee cap so nothing is estimated, and returns its hash without waiting for it to be mined.
pub async fn send(
  provider: &DynProvider,
  from: Address,
  to: Address,
  value: U256,
  data: Vec<u8>,
  tip: u128,
) -> B256 {
  let request = TransactionRequest::default()
    .with_from(from)
    .with_to(to)
    .with_value(value)
    .with_input(Bytes::from(data))
    .with_gas_limit(GAS)
    .with_max_fee_per_gas(FEE_CAP)
    .with_max_priority_fee_per_gas(tip);
  *provider
    .send_transaction(request)
    .await
    .expect("send")
    .tx_hash()
}

/// Waits until `tx` is mined and returns its receipt.
pub async fn mined(provider: &DynProvider, tx: B256) -> TransactionReceipt {
  let deadline = Instant::now() + WAIT;
  loop {
    if let Some(receipt) = provider
      .get_transaction_receipt(tx)
      .await
      .expect("receipt read")
    {
      return receipt;
    }
    assert!(Instant::now() < deadline, "{tx} not mined");
    tokio::time::sleep(POLL).await;
  }
}

/// Sends `amount` of `token_in` from the dumper to `pair` and the `swap` that takes the
/// matching output back, both at tip `tip` and unmined. Returns their hashes and the output,
/// priced from the pair's reserves now with V2's 0.3% fee.
pub async fn send_swap(
  provider: &DynProvider,
  pair: Address,
  token_in: Address,
  amount: U256,
  tip: u128,
) -> (Vec<B256>, U256) {
  let (reserve_in, reserve_out, in_is_zero) = reserves(provider, pair, token_in).await;
  let with_fee = amount * U256::from(997u16);
  let out = with_fee * reserve_out / (reserve_in * U256::from(1000u16) + with_fee);
  let (amount0_out, amount1_out) = if in_is_zero {
    (U256::ZERO, out)
  } else {
    (out, U256::ZERO)
  };
  let transfer = IERC20::transferCall {
    to: pair,
    value: amount,
  };
  let swap = IUniswapV2Pair::swapCall {
    amount0Out: amount0_out,
    amount1Out: amount1_out,
    to: DUMPER,
    data: Bytes::new(),
  };
  let hashes = vec![
    send(
      provider,
      DUMPER,
      token_in,
      U256::ZERO,
      transfer.abi_encode(),
      tip,
    )
    .await,
    send(provider, DUMPER, pair, U256::ZERO, swap.abi_encode(), tip).await,
  ];
  (hashes, out)
}

/// Wraps `amount` ETH as WETH for the dumper, topping its ETH up first; unmined.
pub async fn wrap(provider: &DynProvider, amount: U256) -> B256 {
  rpc(provider, "anvil_setBalance", (DUMPER, amount + GAS_FUNDS)).await;
  send(
    provider,
    DUMPER,
    WETH,
    amount,
    IWETH::depositCall {}.abi_encode(),
    TIP,
  )
  .await
}

/// Wraps `weth_amount` ETH from anvil account 5, sends the WETH to the V2 `pair` and swaps it
/// for the matching output. Returns the block it was mined in and the token amount received.
pub async fn dump(provider: &DynProvider, pair: Address, weth_amount: U256) -> (u64, U256) {
  let deposit = wrap(provider, weth_amount).await;
  let (swaps, out) = send_swap(provider, pair, WETH, weth_amount, TIP).await;
  let mut block = 0;
  for tx in std::iter::once(deposit).chain(swaps) {
    let receipt = mined(provider, tx).await;
    assert!(receipt.status(), "dump transaction {tx} reverted");
    block = block.max(receipt.block_number.expect("mined block"));
  }
  (block, out)
}

/// Dumps a tenth of their WETH into each of `pairs` and catches up past the dumps.
pub async fn imbalance(ctx: &Ctx, pairs: &[Address]) -> Update {
  let mut last = 0;
  for &pair in pairs {
    let amount = tenth_of_weth(&ctx.provider, pair).await;
    last = last.max(dump(&ctx.provider, pair, amount).await.0);
  }
  catch_up(ctx, last).await
}

/// Steps the bot on `update`: its touched pools, or every pair after a reload.
pub async fn step(ctx: &Ctx, update: &Update) -> Vec<(Opportunity, Verdict)> {
  let touched = (!update.reload).then_some(&update.touched);
  ctx.bot.step(update.block, touched).await
}

/// The opportunities in `results` the engine took.
pub fn sent(results: &[(Opportunity, Verdict)]) -> Vec<Opportunity> {
  results
    .iter()
    .filter(|(_, verdict)| *verdict == Verdict::Sent)
    .map(|(op, _)| op.clone())
    .collect()
}

/// Moves every event the engine has emitted into `ctx.seen`.
pub fn drain(ctx: &mut Ctx) {
  while let Ok(event) = ctx.events.try_recv() {
    ctx.seen.push(event);
  }
}

/// The first event seen that satisfies `done`, mining up to `blocks` blocks for it.
pub async fn await_event(
  ctx: &mut Ctx,
  blocks: usize,
  done: impl Fn(&TradeEvent) -> bool,
) -> TradeEvent {
  for _ in 0..=blocks {
    drain(ctx);
    if let Some(event) = ctx.seen.iter().find(|event| done(event)) {
      return event.clone();
    }
    advance(ctx).await;
  }
  panic!(
    "no matching event within {blocks} blocks; seen {:#?}",
    ctx.seen
  );
}

/// Whether `event` is a ledger entry of the trade that carries out `op`.
pub fn carries(event: &TradeEvent, op: &Opportunity) -> bool {
  event.kind == EntryKind::Trade
    && event.detail.buy_pool.as_ref() == Some(&op.buy_pool)
    && event.detail.sell_pool.as_ref() == Some(&op.sell_pool)
    && event.detail.amount == Some(op.amount)
}

/// Whether `state` is one a transaction never leaves.
pub fn is_final(state: TradeState) -> bool {
  matches!(
    state,
    TradeState::Executed | TradeState::Reverted | TradeState::Dropped
  )
}

/// The final ledger entry of the trade that carries out `op`.
pub async fn outcome(ctx: &mut Ctx, op: &Opportunity) -> TradeEvent {
  let trade = await_event(ctx, BLOCKS, |event| carries(event, op)).await;
  await_event(ctx, BLOCKS, |event| {
    event.id == trade.id && event.kind == EntryKind::Trade && is_final(event.state)
  })
  .await
}

/// The side that sells the base token into `venue`.
pub fn base_side(venue: &dyn Venue) -> Side {
  if *venue.tokens()[0] == to_id(WETH) {
    Side::ZeroForOne
  } else {
    Side::OneForZero
  }
}

/// The revert data `tx` ended with, read from anvil's call tracer; empty when it succeeded.
pub async fn revert_data(provider: &DynProvider, tx: B256) -> Bytes {
  let trace = rpc(
    provider,
    "debug_traceTransaction",
    (tx, json!({ "tracer": "callTracer" })),
  )
  .await;
  trace["output"]
    .as_str()
    .unwrap_or("0x")
    .parse()
    .expect("trace output")
}

/// Sends `data` to `to` as `from`, impersonated, and returns the mined receipt.
pub async fn impersonated(
  provider: &DynProvider,
  from: Address,
  to: Address,
  data: Vec<u8>,
) -> TransactionReceipt {
  rpc(provider, "anvil_impersonateAccount", (from,)).await;
  let tx = send(provider, from, to, U256::ZERO, data, TIP).await;
  let receipt = mined(provider, tx).await;
  rpc(provider, "anvil_stopImpersonatingAccount", (from,)).await;
  receipt
}
