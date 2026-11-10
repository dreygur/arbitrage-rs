# Flash Arbitrage Bot Design

Status: reviewed (egm stages 5 to 7), 2026-11-07. Plan:
[2026-11-07-flash-arbitrage.md](../plans/2026-11-07-flash-arbitrage.md).
Code style: [dreygur coding style](https://github.com/dreygur/dreygur-coding-style)
(local copy `/Users/rakib/Code/time_pass/dreygur-coding-style`).

## Problem Statement

The repo holds a demo that reads one Uniswap V2 pair's reserves and sends
an `approve` with `ethers-rs`, a library that is no longer maintained. It
never finds or executes an arbitrage. The goal is a working arbitrage
bot that dreygur can run and study safely on a local mainnet fork,
structured so new DEXs, new EVM networks and later Solana can be added
without rewriting the core.

## Goals

Each goal maps to a test in [Testing](#testing).

- Track many WETH pairs, from a configured token list or from on chain
  discovery filtered by liquidity.
- Quote every tracked pool locally: constant product math for V2, tick
  by tick concentrated liquidity math for V4.
- Pick the best buy and sell pool per pair across all configured DEXs,
  including across families (buy on V4, sell on SushiSwap).
- Execute every profitable pair in a step, best first, through one
  executor contract that borrows from the V4 PoolManager at no fee.
- Revert on chain when a trade would not cover gas plus the configured
  threshold, so a stale quote costs only gas.
- Minimise event to send latency, measured per trade (a `tracing` span)
  and per step (a criterion bench).
- Trade from many accounts, derived from one or more mnemonics or given
  as raw keys, never sending two trades that touch the same pool at once.
- Cap what each hot address holds; move the excess to a treasury address
  that may be a cold wallet.
- Show every evaluated opportunity, why it was sent or skipped, and the
  numbers behind each trade in a terminal UI.
- Reload config and restart the pipeline in place, from the UI or with
  `SIGHUP`.
- Keep a permanent journal of every transaction the bot sends (trades,
  cancels, sweeps, forwards) and every state change, in a file that
  survives restarts.
- Provide a `make` workflow: set up tools, build bindings, test, fork,
  fund, run, sweep, bench, tear tools down.

## Non goals

Out of scope for now:

- Mainnet or testnet deployment. Local fork only.
- The Solana adapter (follow up PR).
- V4 pools with hooks, and V4 pools with fee or tick spacing outside the
  four standard pairs (see Risks).
- Uniswap V3, Curve, Balancer as venues.
- More than two legs per trade, multi hop or triangular routes.
- Pairs without the base token.
- Mempool watching, MEV bundles, Flashbots, gas bidding wars.

## Anti goals

Designs that are not allowed:

- Strategy logic in Solidity. The executor does only the flash borrow,
  the two legs, the repay and the profit check.
- Strategy, sizing or the bot loop knowing a pool's DEX family.
- Foundry as a build dependency: after teardown, `cargo build` and
  `cargo test` still work.
- Floating point in amount math.
- Tests whose assertions pass without the code under test running, such
  as mocks standing in for the function being tested.

## Constraints

- `alloy` 2.x replaces `ethers-rs` (last release 2.0.14). Bindings come
  from alloy's `sol!` macro.
- V4 math uses `uniswap-v4-sdk` 2.x with `uniswap-v3-sdk` 7.x. As of
  2026-11-07, `cargo tree` shows one `alloy` (2.5) and one `ruint`
  (1.21) with them in the tree.
- Latest released dependency versions. `dotenvy` replaces `dotenv`;
  `anyhow` is removed; `eyre` only in binaries.
- Compiled contract artifacts are committed under `abi/`; `make abi`
  rebuilds them.
- Foundry (`forge`, `anvil`) is installed by `make setup` and removed
  completely by `make teardown`. After teardown, fork targets need
  `make setup` again.
- Secrets only in `.env` (gitignored); `.env.example` documents them.
  The TOML names env vars and never holds a secret.
- dreygur coding style throughout; 2 space indent (`.rustfmt.toml`).
- Explanatory doc comments on every module, type, field and function
  (private ones too unless the name says it all) and NatSpec on every
  contract item.
- No over engineering: no abstraction, option or config key without a
  current caller or a decision recorded here.

## Architecture

### Numbers

Amounts are `U256` from `ruint`, the type alloy and the Uniswap SDKs
use, so values cross between them unconverted. The core depends on
`ruint` only. Integer math rounds down like Solidity; our arithmetic is
`checked_*` and overflow is an error.

### Core types and traits

The core never names a chain or a DEX.

```rust
type Amount = U256;
struct Id(Vec<u8>);                 // opaque, parsed only by an adapter
enum Side { ZeroForOne, OneForZero }

struct Opportunity {
  buy_pool: Id, sell_pool: Id, token: Id,
  amount: Amount,                   // base token borrowed
  profit: Amount,                   // quoted gain, before gas
  floor: Amount,                    // gas cost + min_profit, sent as minProfit
}

enum Verdict {
  Sent, BelowMin, GasExceedsProfit, GasCap, QuoteError,
  Locked(u64), LowGas, CircuitOpen, Paused,
}

struct Decision {
  block: u64, op: Option<Opportunity>, cost: Amount, verdict: Verdict,
  buy: Arc<dyn Venue>, sell: Arc<dyn Venue>,   // snapshots, for the UI
}
struct Update { touched: HashSet<Id>, block: u64, reload: bool, restart: bool }
enum TradeState { Planned, Sent, Executed, Reverted, Dropped }
enum Control { Pause, Resume, Sweep }

trait Venue: Send + Sync {
  fn pool(&self) -> &Id;
  fn tokens(&self) -> [&Id; 2];
  fn quote_out(&self, amount_in: Amount, side: Side) -> Result<Amount>;
  fn describe(&self) -> String;   // venue name, fee tier, reserves or price
}

trait Chain {
  async fn changes(&self) -> Result<Update>;
  fn snapshots(&self) -> Vec<Arc<dyn Venue>>;
  fn cost(&self, op: &Opportunity) -> Option<Amount>; // None: fee cap hit
  async fn submit(&self, ops: Vec<Opportunity>) -> Vec<(Opportunity, Verdict)>;
  fn apply_live(&self, live: &Live);
  fn control(&self, c: Control);
  fn in_flight(&self) -> usize;
  fn base(&self) -> Id;          // base token id, for the bot
  fn status(&self) -> Status;    // block, fees, balances, accounts, for the UI
}
```

`Result` is the crate's `thiserror` enum. `Live` holds the settings that
change without a restart (`min_profit`, `max_trade`, `max_gas_cost`,
limits, `trade_timeout`). `cost` and `in_flight` read values the engine
task publishes on `tokio::sync::watch` channels (fee per gas, gas limits
per family pair, trades in flight), so they never wait on the engine.
`Update.restart` asks the supervisor for a full restart (see Fork).

### Pipeline

Two spawned tasks, everything else on the bot's own task:

```text
feed task: new head N -> eth_getLogs(blockHash N) -> channel
bot task:  changes() applies block N -> evaluate touched pairs -> submit()
engine task: accepted trades -> sign, send, receipts, locks, limits
```

The feed subscribes to new heads only. For each head it fetches that
block's logs for the tracked addresses by block hash, so a block is
applied whole and in log order, at the cost of one local RPC per block.
Heads and logs cannot interleave. The bot owns pool state, so there is a
single writer; snapshots are `Arc`s replaced per pool on update.

A V4 pool whose price comes within one bitmap word of its fetched tick
window triggers a background refetch; the new window arrives on the same
channel `changes()` reads and is applied there, keeping one writer. The
pool keeps trading meanwhile, since the bounded tick data makes any
quote past the window a `QuoteError` rather than a wrong number. No RPC
runs between an event and a decision.

### Bot step

For each `Update`: skip if paused; evaluate pairs with a touched pool;
for each pair, pick the best buy and sell pools and size the trade;
`cost` it (a trade whose cost is unknown because the fee cap is hit,
or above `max_gas_cost`, gets `GasCap`); keep trades with
`profit > cost + min_profit` and set `floor = cost + min_profit`;
`submit` them best first. The engine checks the fee cap again on
accept, because the base fee can move between `cost` and send. Every
evaluated pair yields a `Decision` for the UI, including errors.

Trade economics: borrow `x` base, buy the other token on pool A, sell it
on pool B, repay `x`. Gain is `B.quote_out(A.quote_out(x, side_a), side_b) - x`.

Sizing: a binary search first finds the largest size up to `max_trade`
that both pools quote without error. A ternary search over that range
then finds the best size, scoring a loss or an error as minus infinity
and moving the upper bound down on ties, followed by a scan of the last
small window because floor rounding makes the curve only nearly concave.

### Engine

The engine owns sending. Each trade moves through
`Planned -> Sent -> Executed | Reverted | Dropped`.

- Locks: both pools of an accepted trade are locked. A trade touching a
  locked pool is refused with `Locked(id)`, whatever account would send
  it; in a batch, trades are taken best first and overlapping ones
  refused.
- Release: after the trade leaves `Sent` and its block has been applied
  to state, so a pair is never re-evaluated on reserves from before our
  own trade.
- Timeout: after `trade_timeout` blocks without a receipt, the engine
  sends a zero value self transfer at the same nonce with a higher fee,
  waits for it to be mined, then marks the trade `Dropped` and releases
  its locks. A send error at nonce N cancels and replans the later
  nonces of that batch after a nonce resync.
- Gas: limits per family pair start from adapter config defaults and
  track the highest `gasUsed` seen plus headroom; an out of gas receipt
  raises the default. No gas estimate runs between decision and send.
  Fee per gas is the next base fee (EIP 1559 formula from the latest
  head) plus `priority_fee` from config.
- Gas caps, per chain: gas limits never exceed `max_gas_limit`;
  nothing is sent while the next base fee plus `priority_fee` is above
  `max_fee_per_gas`; a trade whose gas cost (limit times fee per gas) is
  above `max_gas_cost` is skipped. A transaction's fee cap is
  `min(2 * next_base_fee + priority_fee, max_fee_per_gas * 10 / 11)`,
  and nothing is sent when `next_base_fee + priority_fee` is above that
  cap, so a cancel bumped by the required 10% always fits under
  `max_fee_per_gas`. Gas limit headroom is 20% over the highest
  `gasUsed` seen; an out of gas receipt multiplies the limit by 1.5;
  both are capped at `max_gas_limit`.
  Cancels, sweeps and forwards obey the same caps.
- Circuit breakers: a pool with `max_reverts` reverts within
  `revert_window` blocks cools down for `cooldown_blocks`. If gas spent
  on reverts within `revert_window` passes `max_revert_gas`, trading
  pauses (`CircuitOpen`) until resumed from the UI.
- One bot process per config, enforced with a `flock` on
  `.arbitrage-<network>.lock`.

### EVM adapter

```text
src/chain/evm/
  mod.rs        EvmChain: connect, Chain impl
  bindings.rs   sol! interfaces and the executor artifact
  convert.rs    Id <-> Address, error mapping
  config.rs     parses the [adapter] table
  pairs.rs      list and discover pools
  state.rs      pool state and log application
  feed.rs       heads plus logs by block hash
  engine.rs     trade state machine, locks, nonce lanes, gas
  breakers.rs   per pool cooldown and global revert gas breaker
  holdings.rs   sweeps and forwards on the owner lane
  accounts.rs   mnemonic and key signers
  fund.rs       fork funding
  venue/v2.rs   constant product with fee bps
  venue/v4.rs   uniswap-v4-sdk pool over a bounded tick window
```

V4 pools are found by key: for each token the adapter probes hookless
keys at the standard fee and tick spacing pairs (100/1, 500/10, 3000/60,
10000/200) against native ETH and WETH, keeping keys whose StateView
price is non zero. A snapshot holds slot0, liquidity and the initialized
ticks within `tick_words` bitmap words around the current tick. The
tick data provider errors outside that window, so a quote never assumes
empty liquidity beyond it. The SDK's quote is `async` only because its
tick provider trait is; ours answers from memory, so the quote is
polled once with `now_or_never()` inside the synchronous `quote_out`.
The SDK has no protocol fee input, so a pool with a non zero protocol
fee is priced per direction with the fee the PoolManager charges:
`swap_fee = protocol + lp - protocol * lp / 1_000_000` in pips, where
`protocol` is the low 12 bits of `protocolFee` for zero for one and the
high 12 bits for one for zero (Uniswap's `ProtocolFeeLibrary`). Native ETH and WETH are one asset to the
core: the adapter reports native as the WETH id and remembers each
pool's real currency for the executor.

An EVM network needs a V4 PoolManager and StateView, a wrapped native
token (set as `base_token` and passed to the executor), and multicall.
Given those, a network is a config file.

Executor lifecycle: the address is saved to `.arbitrage-<network>.json`
(gitignored). On start, `connect` reuses it if code exists there and its
owner and treasury match the config; otherwise it deploys and writes the
file.

### Executor contract

`contracts/FlashArbitrage.sol`, with per family leg libraries in
`contracts/legs/`.

- Constructor: PoolManager, wrapped native token, treasury (immutable),
  operators. The deployer is the owner.
- `execute(Leg buy, Leg sell, uint256 amount, uint256 minProfit)`, only
  operators, `minProfit > 0`.
- `Leg { uint8 family; bytes data; }`: family 0 is V2 (`pair`,
  `zeroForOne`, `feeBps`), family 1 is V4 (`PoolKey`, `zeroForOne`).
  Each family is a library with `swap(data, amountIn) returns (amountOut)`.
- `execute` encodes legs, amount and `minProfit` into the `unlock`
  payload; nothing is written to storage. `unlockCallback` accepts calls
  only from the PoolManager (v4-periphery SafeCallback pattern).
- In the callback: take `amount` from the PoolManager (WETH directly when
  neither leg trades native ETH, otherwise native ETH wrapped once), run
  both legs, settle `amount` back, and require the executor's WETH
  balance to have grown by at least `minProfit` during the call.
- Profit stays in the contract. `sweep(token, amount)` sends only to the
  immutable treasury and only the owner calls it; `setOperator` is owner
  only.

V2 legs swap at the pair (transfer in, `swap` out): no router, no
approvals. V4 legs swap inside the same unlock and settle their deltas
immediately.

### Accounts

`[[accounts]]` entries name env vars: `source = "mnemonic"` with
`indices` (path `m/44'/60'/0'/0/i`), or `source = "key"`. `owner` picks
the account that deploys and sweeps; it is not an operator, so sweeps
use their own nonce lane and cannot delay trades. Every other account
is an operator and gets trades by fewest in flight; an account below
`account_min_gas` gets none. A missing env var or a duplicate address
fails startup.

### Holding limits

| Key | Meaning |
|---|---|
| `executor_max_hold` | executor WETH above this triggers a sweep |
| `executor_keep` | the sweep sends `balance - executor_keep` |
| `account_max_hold` | account ETH above this is forwarded to the treasury |
| `account_keep` | the forward sends `balance - account_keep` |
| `account_min_gas` | below this an account gets no trades and is flagged |
| `min_transfer` | sweeps and forwards smaller than this are not sent |

Startup rejects a config unless `executor_keep < executor_max_hold`,
`account_keep < account_max_hold` and `account_keep >= account_min_gas`.

### Trade journal

An append only JSON Lines file, `logs/trades-<network>.jsonl` by
default (`journal` key), gitignored. One line per event, written and
flushed as it happens, never rewritten:

- `kind`: `trade`, `cancel`, `sweep` or `forward`.
- `state`: `planned`, `sent`, `executed`, `reverted`, `dropped`.
- Timestamps (wall clock and block), trade id, account, nonce, tx hash.
- For trades: pair, buy and sell pools (with `describe` text), amount,
  quoted profit, floor, gas limit, fee cap and tip.
- On receipt: block, gas used, effective gas price, actual profit,
  decoded revert reason.
- Latency: log received, decided, signed, sent.

The engine sends journal entries to a writer task over their own
bounded channel and waits on a full channel instead of dropping, so the
journal never loses an event; with a handful of trades per block the
wait does not occur in practice. Skipped opportunities are not
journaled; they live in the UI and the `tracing` log. `make journal`
pretty prints the last entries.

### Cache invalidation

| Cache | Updated by | Reloaded when |
|---|---|---|
| V2 reserves | `Sync` log | removed log, feed gap, refork |
| V4 price, liquidity, tick | `Swap` log | removed log, feed gap, refork |
| V4 tick map | `ModifyLiquidity` applied locally | price within one word of the window edge, removed log, feed gap, refork |
| V4 protocol fee | read at snapshot load | feed gap, refork |
| Next base fee | each head | recomputed on every head |
| Gas limit per family pair | receipts (highest seen plus headroom) | out of gas receipt |
| Nonce per account | local increment | send error, timeout, restart |
| Pool locks | engine | trade finished and its block applied |
| Pool list | startup | restart |

Each pool records the block it is current at. After a feed reconnect, a
skipped block number, a removed log or a refork, every pool reloads by
multicall before trading resumes.

### Speed

- Event driven state, no RPC between decision and send (above).
- Only pairs with a touched pool are evaluated.
- `find_all` starts sequential. A criterion bench records a constant
  product baseline right after the strategy is built, and is rerun with
  V4 snapshots once the V4 venue exists; only that second run decides
  whether `rayon` and `release-max` are worth it.
- `release` keeps default settings for quick builds; a `release-max`
  profile adds `lto = "fat"` and `codegen-units = 1`, and `make run`
  uses it once the bench shows it helps.
- Latency span per trade: log received, decided, signed, sent.

### Reload and restart

`r` in the UI, `SIGHUP`, or an `Update` with `restart` set:

1. Reread `.env` and the TOML; on error keep running and show it.
2. `Live` settings apply at once through `Chain::apply_live`.
3. Any other change, including a changed value of an env var the config
   names (compared by hash), restarts: stop submitting, wait for trades
   in flight to finish or time out (`Chain::in_flight`), drop the chain,
   rebuild it with the builder `main` picked by `kind`, resume. The
   supervisor is generic over `C: Chain`; there is one `kind` today, so
   no trait objects are needed.
4. The executor follows its lifecycle rule above; added or removed
   operators get `setOperator`.

The lock file is held across restarts.

### Terminal UI

`ratatui` over `crossterm`; `--headless` (`make run-headless`) logs
only.

Main screen: header (network, block, feed lag, base fee, run state),
totals (net, gross, gas, executed, reverted, dropped), holdings
(executor against its cap, treasury, accounts ready), and panels:

- Opportunities: pair, buy and sell venue (from `describe`), size,
  profit, gas, net, verdict.
- Trades: id, pair, account, state, tx, expected and actual profit, gas
  used, latency.
- Accounts: address, role, ETH, nonce, in flight, low gas flag.
- Log; Pairs tab (venues, spread, last update); Config tab (read only).

Detail view: both pools' `describe` text at the quoted block; the leg
amounts (buy output and sell output at the chosen size, quoted from the
decision's snapshots); a profit curve over sizes recomputed from the
same snapshots; send details (account, nonce, gas limit, fee per gas,
floor); result (gas used, actual profit, decoded revert); timing.

Keys: tab, arrows, enter, `p` pause, `s` sweep now, `r` reload, `f`
filter, `q` quit. Decisions and trade events reach the UI over bounded
channels with `try_send`, so the UI cannot block the trading path.

### Fork

`make fork` runs `anvil --fork-url $FORK_URL --block-time 2`: a static
fork of mainnet at the latest block when started, mining every 2
seconds so several transactions can share a block. Nothing from mainnet
arrives after that; only our transactions and test dumps move prices.
`make refork` calls `anvil_reset` to the latest mainnet block. The reset
erases the executor, so on any feed gap the adapter checks that the
executor still has code; if not, `changes()` returns an `Update` with
`restart` set and the supervisor rebuilds the chain, which redeploys the
executor and resyncs nonces. Trades are never sent to a codeless
executor.

The feed needs WebSocket; anvil always serves it, so there is no HTTP
polling fallback.

Funding (`make fund`): native ETH with `anvil_setBalance` for every
account; each token in `[fork.tokens]` with `anvil_dealERC20` (Foundry
1.3 or later), falling back to impersonating that token's configured
whale. Funding first checks `anvil_nodeInfo` and refuses on anything
that is not anvil.

## Config reference

Core keys (`config/<network>.toml`):

| Key | Type | Default (ethereum) |
|---|---|---|
| `kind` | `"evm"` | `"evm"` |
| `base_token` | address string | WETH |
| `min_profit` | wei | 0.001 WETH |
| `max_trade` | wei | 100 WETH |
| `trade_timeout` | blocks | 3 |
| `max_gas_cost` | wei, most gas one trade may cost | 0.02 ETH |
| `journal` | path | `logs/trades-ethereum.jsonl` |
| `max_reverts`, `revert_window`, `cooldown_blocks` | count, blocks, blocks | 3, 20, 50 |
| `max_revert_gas` | wei | 0.05 ETH |
| `[limits]` | see Holding limits | 1, 0.1, 2, 0.5, 0.1, 0.05 |
| `[[accounts]]`, `owner` | see Accounts | `MNEMONIC_1` (anvil test mnemonic) indices `[0, 1, 2]`; owner 0 |
| `treasury` | address string | anvil account 9 (stand in for a cold wallet) |
| `[pairs]` | `mode` (`list`, `discover`), `tokens`, `max_scan`, `min_base_reserve` | list; USDC, USDT, DAI, WBTC; 5000; 10 WETH |
| `[adapter]` | adapter specific table | see below |

EVM `[adapter]` keys: `chain_id`, `pool_manager`, `state_view`,
`priority_fee` (1 gwei), `max_fee_per_gas`
(50 gwei), `gas_v2_v2`, `gas_v2_v4`, `gas_v4_v4` (starting limits),
`max_gas_limit` (1,000,000), `[[adapter.venues]]` (`name`, `family`, `address`,
`fee_bps` for V2, `tick_words` for V4), `[adapter.fork]` (`fund_eth`,
and `tokens` with `amount` and `whale`).

For V4 pools in discovery, `min_base_reserve` compares against the
active range's virtual base reserve (`L / sqrtP` or `L * sqrtP`, by the
base token's side).

Env vars: `RPC_URL`, `WS_URL`, `FORK_URL` (default
`https://ethereum-rpc.publicnode.com`), `NETWORK`, and the account vars
the config names.

## Developer workflow

| Target | Does |
|---|---|
| `setup` | install Foundry if missing |
| `abi` | build contracts, copy the executor artifact into `abi/` |
| `fork` | start the static fork with 2 second blocks |
| `refork` | reset the fork to the latest mainnet block |
| `fund` | fund every account with ETH and configured tokens |
| `run` | run the bot with the UI |
| `run-headless` | run with logs only |
| `sweep` | sweep executor profit to the treasury now |
| `journal` | print the last 20 journal entries |
| `test` | unit tests, no network |
| `test-fork` | `#[ignore]`d tests against a running fork |
| `bench` | criterion bench of the step hot path |
| `lint` | `cargo fmt --check`, `cargo clippy` |
| `teardown` | remove Foundry |

## Testing

| Goal | Covered by |
|---|---|
| Local quoting | V2 and V4 unit tests; fork tests comparing with router and quoter |
| Best pools and sizing | sizing and strategy unit tests, including errors and ties |
| Execution and profit | fork test: V4 against SushiSwap, executor WETH grows |
| Revert on loss | fork test: stale trade reverts `NoProfit` |
| Gas caps | engine and bot unit tests; fork test: base fee above the cap stops trading |
| Latency | bench; span recorded in fork tests |
| Accounts and locks | engine unit tests; fork test: two pairs from two accounts |
| Holding limits | engine unit tests; fork test: auto sweep |
| Circuit breakers | engine unit tests |
| Trade journal | journal unit tests; fork test: an executed trade appears with its receipt fields |
| UI | render tests on `TestBackend` |
| Reload | supervisor tests with `MockChain`; fork test adds an operator |
| Workflow | `make` targets run in the final plan task |

## Alternatives Considered

| Choice | Picked | Rejected and why |
|---|---|---|
| Flash source | V4 PoolManager | V2 flash swap: pays the 0.3% pool fee and ties the borrow to the buy pool. Balancer Vault: fee free too, but a second protocol to integrate for no gain on this fork. Aave: charges a fee. |
| Sizing | bounded binary then ternary search | V2 closed form: fast, but does not cover V4 or mixed pairs, and one method for all is easier to test. |
| Feed | heads plus `eth_getLogs` by block hash | log subscription: no end of block marker, so a block could be half applied. Polling reserves: an RPC per pool per block. |
| Contract shape | one executor with leg libraries | a contract per family: cross family trades would need two contracts per trade. Router based legs: extra approvals and gas. |
| Pipeline | own tokio tasks | Artemis: same shape, but idle for years and not on alloy 2. |
| Nonces | local lanes per account | alloy's nonce manager: fetches per send, which is an RPC on the hot path. |
| Funding | setBalance plus dealERC20, whale fallback | whale only: fails for tokens without a large holder. |

## Risks and Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Operator key stolen | attacker calls `execute` with crafted legs | `minProfit > 0` and the balance growth check mean a trade cannot lose executor funds; profit only leaves through `sweep` |
| Owner key stolen | sweeps | `sweep` only pays the immutable treasury |
| Malicious pool or token in discovery | quotes look profitable, trades revert | per pool cooldown and global revert gas breaker; gain measured as executor balance growth |
| Fee on transfer or rebasing tokens | local math wrong | same breakers; list mode for known tokens |
| V4 pools outside the four probed keys | missed opportunities | accepted limitation; scan `Initialize` events later |
| Public `FORK_URL` rate limits | slow fork start | configurable `FORK_URL` |
| Fork drift from mainnet | stale prices | `make refork` |
| UI channel full | dropped UI records | intended; trading never waits |
| Stuck transaction | pool locked | timeout with same nonce cancel |

## Decision log

- Venues on Ethereum: Uniswap V4 (PoolManager
  `0x000000000004444c5dc75cB358380D2e3dE08A90`, StateView
  `0x7fFE42C4a5DEeA5b0feC41C94C136Cf115597227`), Uniswap V2 (factory
  `0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f`), SushiSwap (factory
  `0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac`).
- Static fork with 2 second blocks and `make refork` (over a fork reset
  on a timer).
- Funding by setBalance and dealERC20 with whale impersonation fallback.
- Discovery kept, with circuit breakers.
- Mnemonic moves from the README to `.env.example`.
- Parallelism and LTO only after the bench shows a gain.
- Gas caps per chain: `max_fee_per_gas`, `priority_fee`, `max_gas_limit`
  in the adapter table; `max_gas_cost` per trade in the core table.
- Profit accumulates in the executor; sweeps go to a fixed treasury.
- Venue implementations live in the adapter; traits in the core.
- Chain neutral core with an EVM adapter now; Solana later.
- Work lands on a feature branch through a PR; Foundry removed at the end.
- V4 pools with a protocol fee are priced with the combined swap fee
  per direction instead of skipped: by November 2026 every hookless
  WETH pool for the listed tokens carries one, so skipping them left no
  V4 venue. A pricing error costs only a `NoProfit` revert, since the
  executor checks profit on chain.

## Open implementation choices

- Exact default gas limits per family pair (measured on the fork).
- Default token list adjustments if a pool is too shallow at fork time.
