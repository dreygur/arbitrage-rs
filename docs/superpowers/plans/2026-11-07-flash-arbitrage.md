# Flash Arbitrage Bot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust bot that tracks WETH pairs across Uniswap V4, Uniswap V2 and SushiSwap and closes cross venue price gaps atomically with a fee free V4 flash loan, on an anvil fork of Ethereum.

**Operational scope:** configured accounts with capped hot balances and a fixed treasury, per chain gas caps, a terminal UI showing every decision, an append only trade journal, and config reload with in place restart.

**Architecture:** A chain and DEX neutral core (`types`, `venue`, `sizing`, `strategy`, `discovery`, `config`, `journal`, `chain`, `bot`, `supervisor`, `ui`) on `ruint` `U256`. The `chain/evm` adapter implements `Chain` with alloy: V2 and V4 venues, a block feed (heads plus logs by block hash), and an engine task that owns sending. One Solidity executor borrows from the V4 PoolManager and runs two legs through per family libraries.

**Tech Stack:** Rust 2024, alloy 2.x, uniswap-v4-sdk 2.0, uniswap-v3-sdk 7.0, ruint 1.x, tokio, futures, thiserror, tracing, serde, toml 1, dotenvy, eyre (binaries), ratatui 0.30 (its re-exported crossterm), criterion 0.8. Solidity 0.8.28, Foundry `forge`, `anvil`, forge-std, make.

**Spec:** `docs/superpowers/specs/2026-11-07-flash-arbitrage-design.md` (read it; this plan does not repeat its reasoning).

## Global Constraints

- dreygur coding style: helpers above callers, constants at the top, no section comments, `thiserror` errors, `tracing` logs, `mock_*` doubles, braces always, no dashes or hyphenated compounds in prose.
- Doc comments on every item: modules (`//!`), types, fields, functions, private ones included when the name alone does not state the purpose. NatSpec on every contract function, error and struct. A comment states what the item is for and why; it never labels sections of a file.
- No speculative abstractions: no trait, option or config key without a caller in this plan or a decision in the spec. If a task seems to need one, stop and ask the controller.
- 2 space indent; `cargo fmt` before each commit. Commit subjects imperative, capitalized, under 50 chars, no period, no agent attribution.
- No floats in amount math; `checked_*` arithmetic, overflow is `Error::Overflow`.
- Core modules never import `alloy` or the Uniswap SDKs; only `src/chain/evm/` does.
- `cargo build` and `cargo test` work with no Foundry installed.
- No RPC call between deciding a trade and sending it.
- Gas is capped per chain: nothing is sent when its fee per gas would exceed `max_fee_per_gas`, or a trade's gas cost would exceed `max_gas_cost`.
- Each task that adds a module also adds its `mod` line to `src/lib.rs` (or the parent module) and lists that file.
- Shared test doubles live in `src/testkit.rs`, compiled under `#[cfg(any(test, feature = "testkit"))]`; benches and integration tests enable the `testkit` feature.
- Native tools first: Read, Edit, Write, Grep, Glob for files and search. Bash only for `git`, `cargo`, `make`, `forge`, `anvil`, `gh`, `curl` (JSON-RPC in the Makefile), as plain commands. Never pipe code into an interpreter yourself. (The Makefile's `setup` recipe running the official Foundry installer is a user command, not a tool call, and is allowed.)
- `.env` values are unquoted (Make's `-include .env` keeps quotes literally).
- Addresses: V4 PoolManager `0x000000000004444c5dc75cB358380D2e3dE08A90`, StateView `0x7fFE42C4a5DEeA5b0feC41C94C136Cf115597227`, V4Quoter `0x52F0E24D1c21C8A0cB1e5a5dD6198556BD9E1203`, Uniswap V2 factory `0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f`, UniswapV2Router02 `0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D`, SushiSwap factory `0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac`, Multicall3 `0xcA11bde05977b3631167028862bE2a173976CA11`, WETH `0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2`, USDC `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48`, USDT `0xdAC17F958D2ee523a2206206994597C13D831ec7`, DAI `0x6B175474E89094C44Da98b954EedeAC495271d0F`, WBTC `0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599`, treasury stand in (anvil account 9) `0xa0Ee7A142d267C1f36714E4a8F75612F20a79720`, default whale `0x28C6c06298d514Db089934071355E5743bf21d60`. Router and Quoter are test constants only.
- V4: probe keys fee/tick spacing 100/1, 500/10, 3000/60, 10000/200, hooks zero, base side native ETH and WETH; swap price limits `MIN_SQRT_PRICE + 1 = 4295128740` and `MAX_SQRT_PRICE - 1 = 1461446703485210103287273052203988822378723970341`.
- Anvil test mnemonic `test test test test test test test test test test test junk`: index 0 `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` (owner), 1 `0x70997970C51812dc3A010C7d01b50e0d17dc79C8`, 2 `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` (operators).
- Gas numbers: limit headroom 20% over the highest `gasUsed` seen; an out of gas receipt multiplies that pair's limit by 1.5; both capped at `max_gas_limit`. A trade's fee cap is `min(2 * next_base_fee + priority_fee, max_fee_per_gas * 10 / 11)` and nothing is sent when `next_base_fee + priority_fee` exceeds that cap, so a 10% bumped cancel always fits under `max_fee_per_gas`.
- Branch `feature/flash-arbitrage`, merged through a PR.

## Review Focus

1. A V4 quote crossing past the fetched tick window must error, never assume empty liquidity. Task 10.
2. No two trades may be in flight on one pool across all accounts, and a stuck trade keeps its locks until its cancel or the original is mined. Task 15.
3. A discovered token that always reverts must trip the pool cooldown and, if widespread, the global breaker. Task 12.
4. A base fee spike must stop sending (`GasCap`), and a stuck trade's cancel must still fit under `max_fee_per_gas`. Tasks 8 and 15.
5. A changed `.env` value or an invalid config on reload: restart on the first, keep running on the second. Task 20.

---

## File Structure

```text
Cargo.toml  Makefile  foundry.toml  .env.example  config/ethereum.toml
scripts/teardown.sh
contracts/FlashArbitrage.sol  contracts/legs/{V2Leg,V4Leg}.sol
contracts/test/FlashArbitrage.t.sol
abi/FlashArbitrage.json
src/lib.rs  src/main.rs  src/testkit.rs
src/bin/{fund,sweep,journal}.rs
src/{error,types,venue,sizing,strategy,discovery,config,journal,chain,bot,supervisor}.rs
src/ui/{mod,app,views,detail}.rs
src/chain/evm/{mod,bindings,convert,config,ticks,pairs,state,feed,breakers,holdings,engine,accounts,executor,fund}.rs
src/chain/evm/venue/{mod,v2,v4}.rs
tests/common/mod.rs  tests/fork.rs  tests/refork.rs
benches/step.rs
docs/superpowers/reviews/bench.md
```

Deleted: `src/uniswap.rs`, `src/arbitrage.rs`, `src/eth_wallet.rs`.

---

### Task 1: Branch, deps, core types

**Files:** Modify `Cargo.toml`, `.gitignore`, `src/main.rs`. Create `.claude/settings.json`, `src/lib.rs`, `src/error.rs`, `src/types.rs`. Delete the three ethers modules.

**Interfaces (produces):**
- `Amount = ruint::aliases::U256`; `Id(Vec<u8>)` (`new`, `as_bytes`, `Display` as `0x` hex, derives `Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord`); `Side { ZeroForOne, OneForZero }` (`flip`).
- `Opportunity { buy_pool, sell_pool, token, amount, profit, floor }` (`token` is the non base token).
- `Verdict { Sent, BelowMin, GasExceedsProfit, GasCap, QuoteError, Locked(u64), LowGas, CircuitOpen, Paused }`.
- `Update { touched: HashSet<Id>, block: u64, reload: bool, restart: bool }`; `TradeState { Planned, Sent, Executed, Reverted, Dropped }`; `Control { Pause, Resume, Sweep }`.
- `EntryKind { Trade, Cancel, Sweep, Forward }`; `TradeEvent { id: u64, kind: EntryKind, state: TradeState, block: u64, account: String, nonce: u64, tx: Option<String>, detail: TradeDetail }`; `TradeDetail` with optional chain neutral fields `pair: Option<(Id, Id)>`, `buy_pool`, `sell_pool`, `buy_describe: Option<String>`, `sell_describe`, `amount`, `quoted_profit`, `floor`, `gas_limit: Option<u64>`, `fee_cap: Option<u128>`, `tip: Option<u128>`, `mined_block: Option<u64>`, `gas_used: Option<u64>`, `effective_gas_price: Option<u128>`, `actual_profit: Option<Amount>`, `revert: Option<String>`, `latency_us: Option<[u64; 4]>`.
- `Status { block: u64, next_base_fee: u128, fee_cap: u128, max_fee_per_gas: u128, feed_lag_ms: u64, executor: String, executor_balance: Amount, treasury: String, treasury_balance: Amount, accounts: Vec<AccountStatus> }`, `AccountStatus { address: String, role: Role { Owner, Operator }, eth: Amount, nonce: u64, in_flight: usize, low_gas: bool }`.
- `Error { Overflow, InsufficientLiquidity, TokenNotInPool, Config(String), Env(String), Io(#[from] std::io::Error), Toml(#[from] toml::de::Error), Json(#[from] serde_json::Error), Rpc(String), Contract(String), Lock(String) }`, `Result<T>`.

- [ ] **Step 1:** `git checkout -b feature/flash-arbitrage`; `git rm --cached .env` (keeps the local file, stops tracking it); `git add docs && git commit -m "Add flash arbitrage design, plan and reviews"`. Write `.claude/settings.json` as `{"attribution": {"commit": "", "pr": ""}}`.
- [ ] **Step 2:** `Cargo.toml`: edition 2024, `default-run = "arbitrage"`; remove `[build]`, `ethers`, `anyhow`, `dotenv`. Dependencies: `alloy = { version = "2", features = ["full", "json", "signer-mnemonic"] }`, `uniswap-v4-sdk = { version = "2", features = ["std"] }`, `uniswap-v3-sdk = { version = "7", features = ["std"] }`, `ruint = "1"`, `tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync", "signal", "fs", "io-util"] }`, `futures = "0.3"`, `thiserror = "2"`, `tracing = "0.1"`, `tracing-subscriber = { version = "0.3", features = ["env-filter"] }`, `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`, `toml = "1"`, `dotenvy = "0.15"`, `eyre = "0.6"`. Dev: `criterion = "0.8"`. `[features] testkit = []`. `[profile.release-max] inherits = "release"`, `lto = "fat"`, `codegen-units = 1`. `.gitignore` adds `out/`, `cache/`, `lib/`, `logs/`, `.env`, `.arbitrage-*`.
- [ ] **Step 3: Failing tests** in `types.rs`: `id_displays_as_hex` (`[0xab, 0x01]` is `"0xab01"`), `side_flips`.
- [ ] **Step 4:** delete the ethers modules; `main.rs` initializes `tracing_subscriber` and logs `starting`; `lib.rs` declares `error` and `types` only.
- [ ] **Step 5:** implement; `cargo test` shows 2 passed.
- [ ] **Step 6: Commit** `Replace ethers scaffold with core types`.

---

### Task 2: Makefile, Foundry, executor contract

**Files:** Create `Makefile`, `foundry.toml`, `scripts/teardown.sh`, `.env.example`, `contracts/FlashArbitrage.sol`, `contracts/legs/V2Leg.sol`, `contracts/legs/V4Leg.sol`, `contracts/test/FlashArbitrage.t.sol`, `abi/FlashArbitrage.json`.

**Interfaces (produces):**
- `constructor(address poolManager, address wrappedNative, address treasury, address[] operators)`; `address public immutable owner` (deployer); `address public immutable treasury`.
- `execute(Leg buy, Leg sell, uint256 amount, uint256 minProfit)` operators only, `minProfit > 0`.
- `struct Leg { uint8 family; bytes data; }`: 0 V2 `abi.encode(address pair, bool zeroForOne, uint16 feeBps)`; 1 V4 `abi.encode(PoolKey key, bool zeroForOne)`.
- `unlockCallback(bytes) returns (bytes)` PoolManager only.
- `sweep(address token, uint256 amount)` owner only, pays `treasury`; `setOperator(address, bool)` owner only; `mapping(address => bool) public operators`.
- Errors `NotOwner()`, `NotOperator()`, `NotPoolManager()`, `ZeroMinProfit()`, `UnknownFamily(uint8)`, `NoProfit(uint256 gained, uint256 want)`.
- `V2Leg.swap(bytes data, uint256 amountIn) internal returns (uint256)`, `V4Leg.swap(IPoolManager pm, bytes data, uint256 amountIn, IWETH weth) internal returns (uint256)`. Both `internal`, so the executor bytecode has no link placeholders. Legs take and return the base side as WETH.

- [ ] **Step 1: Makefile.** Top: `FOUNDRY_BIN ?= $(HOME)/.foundry/bin`, `FORGE := $(FOUNDRY_BIN)/forge`, `ANVIL := $(FOUNDRY_BIN)/anvil`, `NETWORK ?= ethereum`, `-include .env`, `export`. Targets (all `.PHONY`):
  - `setup`: if `$(FORGE)` is missing, `curl -L https://foundry.paradigm.xyz | bash` then `$(FOUNDRY_BIN)/foundryup --install stable`; then `$(FORGE) install foundry-rs/forge-std --no-git`; print `$(FORGE) --version` (must be 1.3 or later for `anvil_dealERC20`).
  - `abi`: `$(FORGE) build` then `cp out/FlashArbitrage.sol/FlashArbitrage.json abi/`.
  - `test-contracts`: `$(FORGE) test --fork-url $(FORK_URL)`.
  - `fork`: `$(ANVIL) --fork-url $(FORK_URL) --block-time 2`.
  - `refork`: `curl -s -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"anvil_reset","params":[{"forking":{"jsonRpcUrl":"$(FORK_URL)"}}]}' $(RPC_URL)`.
  - `fund`: `cargo run --release --bin fund`; `sweep`: `cargo run --release --bin sweep`; `journal`: `cargo run --release --bin journal`.
  - `run`: `NETWORK=$(NETWORK) cargo run --release --bin arbitrage`; `run-headless`: same with `-- --headless`.
  - `test`: `cargo test`; `test-fork`: `cargo test --features testkit --test fork -- --ignored --test-threads=1` then the same for `--test refork`.
  - `bench`: `cargo bench --features testkit`; `lint`: `cargo fmt --check && cargo clippy --all-targets --features testkit -- -D warnings`; `teardown`: `scripts/teardown.sh`.
- [ ] **Step 2: `scripts/teardown.sh`.** `set -euo pipefail`; remove `$HOME/.foundry`; for each existing file of `${ZDOTDIR:-$HOME}/.zshenv`, `~/.zshrc`, `~/.bashrc`, `~/.bash_profile`, `~/.profile`, delete lines exactly equal to `export PATH="$PATH:$HOME/.foundry/bin"` (home expanded); for `~/.config/fish/config.fish`, delete lines exactly equal to `fish_add_path -a $HOME/.foundry/bin` (home expanded); edit through a temp file and `mv`; prefix each log line with `[+]`. Mark executable.
- [ ] **Step 3:** `foundry.toml`: `[profile.default]` with `src = "contracts"`, `out = "out"`, `test = "contracts/test"`, `libs = ["lib"]`, `solc = "0.8.28"`, `optimizer = true`, `via_ir = true`. `.env.example` (unquoted values): `RPC_URL=http://127.0.0.1:8545`, `WS_URL=ws://127.0.0.1:8545`, `FORK_URL=https://ethereum-rpc.publicnode.com`, `NETWORK=ethereum`, `MNEMONIC_1=` the anvil test mnemonic, and a commented `# MNEMONIC_2=` line holding the mnemonic currently in `README.md` (labelled as an extra test wallet).
- [ ] **Step 4: Contracts.** Inline minimal interfaces (`IPoolManager` with `unlock`, `take`, `settle`, `sync`, `swap`, `PoolKey`, `SwapParams`, `BalanceDelta` as `int256` with amount0 in the high 128 bits; `IUniswapV2Pair`; `IERC20`; `IWETH`). `execute`: operator check, `minProfit > 0`, `pm.unlock(abi.encode(buy, sell, amount, minProfit))`; nothing written to storage. `unlockCallback`:

```solidity
if (msg.sender != address(pm)) revert NotPoolManager();
uint256 before = weth.balanceOf(address(this));
// borrow WETH: pm.take(WETH, this, amount)
// run buy then sell through V2Leg / V4Leg by family, else UnknownFamily
// repay WETH: pm.sync(WETH); weth.transfer(pm, amount); pm.settle()
uint256 afterBal = weth.balanceOf(address(this));
uint256 gained = afterBal > before ? afterBal - before : 0;
if (gained < minProfit) revert NoProfit(gained, minProfit);
```

`V2Leg`: transfer `amountIn` of the input token to the pair, output from fresh reserves `amountIn*(10000-fee)*rOut / (rIn*10000 + amountIn*(10000-fee))`, `pair.swap` to self. `V4Leg`: `pm.swap(key, SwapParams(zeroForOne, -int256(amountIn), limit), "")` with the limits from Global Constraints; read the input owed and output due from the returned `BalanceDelta`; pay input (native: `weth.withdraw` then `pm.settle{value}`; ERC20: `pm.sync`, transfer, `pm.settle`); `pm.take` output (wrap with `weth.deposit` when native). `receive()` accepts ETH only from WETH and the PoolManager. NatSpec on everything.
- [ ] **Step 5: Contract tests** in `contracts/test/FlashArbitrage.t.sol` (forge-std, mainnet fork): `test_rejects_foreign_callback`, `test_rejects_non_operator`, `test_rejects_zero_min_profit`, `test_sweep_pays_treasury_only`, `test_v2_to_v2_arbitrage_profits` (deal WETH to a test account, dump into the SushiSwap WETH/USDC pair, execute buy on SushiSwap and sell on Uniswap V2 or the reverse, assert executor WETH grew), `test_v4_leg_round_trip` (buy on the WETH/USDC 0.05% V4 pool, sell on SushiSwap after a dump), `test_no_profit_reverts`.
- [ ] **Step 6:** `make setup && make abi && make test-contracts`: all pass; `forge inspect FlashArbitrage bytecode` is non empty with no `__$` placeholders.
- [ ] **Step 7: Commit** `Add V4 flash executor with V2 and V4 legs`.

---

### Task 3: Venue trait, sizing, testkit

**Files:** Create `src/venue.rs`, `src/sizing.rs`, `src/testkit.rs`; modify `src/lib.rs`.

**Interfaces (produces):**
- `Venue: Send + Sync { pool(&self) -> &Id; tokens(&self) -> [&Id; 2]; quote_out(&self, amount_in: Amount, side: Side) -> Result<Amount>; describe(&self) -> String }`.
- `side_for(venue: &dyn Venue, token_in: &Id) -> Result<Side>` (`TokenNotInPool` otherwise).
- `gain(buy: &dyn Venue, sell: &dyn Venue, base: &Id, x: Amount) -> Result<Option<Amount>>`: `Err` on quote error, `Ok(None)` on a loss.
- `best_amount(buy, sell, base, max_trade) -> Option<(Amount, Amount)>`: `(amount, profit)` with `profit > 0`.
- `testkit::MockVenue::new(pool: &str, token0: &str, token1: &str, r0: u64, r1: u64, fee_bps: u16)` constant product, optional `fail_above: Option<Amount>`; `describe` returns `"mock {pool}"`.

- [ ] **Step 1: Failing tests**: `side_for_finds_token_and_rejects_strangers`; `finds_near_optimal_size` (reserves 10_000/20_000 vs 10_000/25_000; profit is at least the brute force maximum profit minus 1 wei); `balanced_pools_have_no_profit`; `error_region_does_not_capture_search` (sell venue with `fail_above` at 3_000 while the true peak is below 1_000; result equals the brute force peak); `ties_do_not_skip_peak` (reserves 1_000_000/1_000_000 vs 1_000_000/1_002_000, integer flats around the peak; result within 1 wei of brute force); `respects_max_trade`.
- [ ] **Step 2:** run `cargo test`, expect compile failure.
- [ ] **Step 3:** implement: binary search for the largest `x` in `[1, max_trade]` where `gain` is not `Err` (none means no trade); ternary search on `[1, that]` scoring `Err` and `None` as minus infinity, `hi = m2 - 1` on ties; scan the last window of at most 3 sizes.
- [ ] **Step 4:** `cargo test`: all pass.
- [ ] **Step 5: Commit** `Add venue trait and bounded trade sizing`.

---

### Task 4: Strategy

**Files:** Create `src/strategy.rs`; modify `src/lib.rs`.

**Interfaces (produces):**
- `find(a: &dyn Venue, b: &dyn Venue, base: &Id, max_trade) -> Result<Option<Opportunity>>` (both directions, better one, `floor = 0`; `TokenNotInPool` on mismatch).
- `Evaluation { Found { op: Opportunity, buy: Arc<dyn Venue>, sell: Arc<dyn Venue> }, Failed { buy: Arc<dyn Venue>, sell: Arc<dyn Venue>, error: Error } }`. `Failed` means no venue pair of the group produced a trade and at least one errored at size 1 (that pair is reported). Groups with no trade and no error produce nothing.
- `find_all(venues: &[Arc<dyn Venue>], base: &Id, max_trade, touched: Option<&HashSet<Id>>) -> Vec<Evaluation>`: groups by unordered token pair, only groups with a touched pool when `Some`, best per group over every venue pair, sequential, `Found` sorted by profit descending, then `Failed`.

- [ ] **Step 1: Failing tests** (testkit `MockVenue`): `picks_cheap_pool_to_buy_on` (ids, `token`, and `profit` recomputed in the test), `reverse_imbalance_flips_direction`, `balanced_returns_nothing`, `mismatched_pairs_error`, `find_all_one_per_pair_sorted`, `find_all_best_of_three_venues`, `find_all_skips_single_venue_pairs`, `find_all_untouched_pairs_skipped`, `find_all_reports_failures` (one venue with `fail_above = 0`).
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add strategy across pairs and venues`.

---

### Task 5: Step bench baseline

**Files:** Create `benches/step.rs`, `docs/superpowers/reviews/bench.md`; modify `Cargo.toml` (`[[bench]] name = "step"`, `harness = false`, `required-features = ["testkit"]`).

- [ ] **Step 1:** bench `find_all` over 200 `MockVenue` pairs on two venues; pair `i` has the second venue's quote reserve raised by `i * 0.05%` (0% to 10%); cases `touched = None` and one touched pool.
- [ ] **Step 2:** `make bench`; write the medians to `bench.md` as the constant product baseline. No decision here.
- [ ] **Step 3: Commit** `Add step benchmark baseline`.

---

### Task 6: Discovery filter

**Files:** Create `src/discovery.rs`; modify `src/lib.rs`.

**Interfaces (produces):** `PoolInfo { venue: usize, pool: Id, token0: Id, token1: Id, base_reserve: Amount }`; `select(pools: &[PoolInfo], base: &Id, min_base_reserve) -> Vec<Vec<PoolInfo>>` (drop pools without base or under the minimum, group by the other token, keep groups on two or more distinct venues, several pools per venue allowed, sorted by token id).

- [ ] **Step 1: Failing tests**: `keeps_pair_on_two_venues`, `drops_pair_on_one_venue`, `drops_shallow_pool`, `drops_pairs_without_base`, `base_can_be_either_token`, `keeps_several_v4_tiers`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add liquidity filter for pair discovery`.

---

### Task 7: Core config

**Files:** Create `src/config.rs`, `config/ethereum.toml`; modify `src/lib.rs`.

**Interfaces (produces):**
- Amount fields use `#[serde(deserialize_with = "amount")]`, a parser for decimal strings only (`U256::from_str_radix(s, 10)`), error naming the key.
- `ChainKind { Evm }`, `PairMode { List, Discover }`, `AccountSource { Mnemonic { env: String, indices: Vec<u32> }, Key { env: String } }` (serde tag `source`, lowercase).
- `Limits { executor_max_hold, executor_keep, account_max_hold, account_keep, account_min_gas, min_transfer }`.
- `Breakers { max_reverts: u32, revert_window: u64, cooldown_blocks: u64, max_revert_gas: Amount }`, flattened (top level keys).
- `Live { min_profit, max_trade, max_gas_cost, trade_timeout: u64, limits: Limits }`, flattened except `limits` (a `[limits]` table).
- `NetworkConfig { kind, base_token: String, live, breakers, accounts: Vec<AccountSource>, owner: usize, treasury: String, journal: PathBuf, pairs: PairsConfig { mode, tokens: Vec<String>, max_scan: u64, min_base_reserve: Amount }, adapter: toml::Table }`; `journal` defaults to `logs/trades-<network>.jsonl`.
- `NetworkConfig::parse(text, network)`, `load(network)` (reads `config/<network>.toml`), `env_fingerprint(&self) -> u64` (hash of `RPC_URL`, `WS_URL` and every env var named in `accounts`), `diff(&self, old_fp, new: &NetworkConfig, new_fp) -> Change { None, Live, Restart }` (only `live` differs gives `Live`; anything else, or a different fingerprint, gives `Restart`).
- Validation: at least one account, `owner` in range of the flattened account list, `executor_keep < executor_max_hold`, `account_keep < account_max_hold`, `account_keep >= account_min_gas`, `max_gas_cost > 0`; errors name the key.

`config/ethereum.toml`: `kind = "evm"`, `base_token` WETH, `min_profit = "1000000000000000"`, `max_trade = "100000000000000000000"`, `max_gas_cost = "20000000000000000"`, `trade_timeout = 3`, `max_reverts = 3`, `revert_window = 20`, `cooldown_blocks = 50`, `max_revert_gas = "50000000000000000"`, `[limits]` `1000000000000000000`, `100000000000000000`, `2000000000000000000`, `500000000000000000`, `100000000000000000`, `50000000000000000`, `[[accounts]] source = "mnemonic", env = "MNEMONIC_1", indices = [0, 1, 2]`, `owner = 0`, `treasury` the anvil account 9 address, `[pairs] mode = "list"`, `tokens` USDC, USDT, DAI, WBTC, `max_scan = 5000`, `min_base_reserve = "10000000000000000000"`, and `[adapter]`: `chain_id = 1`, `pool_manager`, `state_view`, `multicall`, `priority_fee = "1000000000"`, `max_fee_per_gas = "50000000000"`, `gas_v2_v2 = 250000`, `gas_v2_v4 = 300000`, `gas_v4_v4 = 350000`, `max_gas_limit = 1000000`, `[[adapter.venues]]` `uniswap_v4` (`family = "v4"`, `address` PoolManager, `tick_words = 4`), `uniswap_v2` and `sushiswap` (`family = "v2"`, factory `address`, `fee_bps = 30`), `[adapter.fork] fund_eth = "1000000000000000000"`, `[[adapter.fork.tokens]]` entries `{ address, amount, whale }`: USDC `100000000000`, USDT `100000000000`, DAI `100000000000000000000000`, WBTC `100000000`, whale the default whale.

- [ ] **Step 1: Failing tests**: `parses_ethereum_config` (via `include_str!`), `rejects_bad_amount`, `list_mode_needs_tokens`, `needs_an_account`, `owner_index_in_range`, `keep_must_be_below_cap`, `account_keep_covers_min_gas`, `max_gas_cost_must_be_positive`, `diff_live_for_thresholds`, `diff_live_for_max_gas_cost`, `diff_restart_for_adapter_change`, `diff_restart_for_env_change`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add network config`.

---

### Task 8: Chain trait and bot

**Files:** Create `src/chain.rs`, `src/bot.rs`; modify `src/lib.rs`, `src/testkit.rs`.

**Interfaces (produces):**
- `Chain` (`#[allow(async_fn_in_trait)]`): `async changes(&self) -> Result<Update>`; `snapshots(&self) -> Vec<Arc<dyn Venue>>`; `cost(&self, op) -> Option<Amount>` (`None` when the fee cap is exceeded); `async submit(&self, ops) -> Vec<(Opportunity, Verdict)>`; `apply_live(&self, &Live)`; `control(&self, Control)`; `in_flight(&self) -> usize`; `base(&self) -> Id`; `status(&self) -> Status`.
- `Decision { block, op: Option<Opportunity>, cost: Amount, verdict: Verdict, buy: Arc<dyn Venue>, sell: Arc<dyn Venue> }` (`cost` is 0 when `cost()` returned `None`).
- `Bot<C: Chain>` holds `Arc<C>`, `paused: AtomicBool`, `live: RwLock<Live>`, `decisions: mpsc::Sender<Decision>`. `Bot::new(chain: Arc<C>, live, decisions)` (base from `chain.base()`); `set_paused(&self, bool)` (also `Control::Pause`/`Resume`); `apply_live(&self, Live)`.
- `async step(&self, block: u64, touched: Option<&HashSet<Id>>) -> Vec<(Opportunity, Verdict)>`, per evaluation: paused gives `Paused` without submit; `Failed` gives `QuoteError` (`op = None`); `cost()` `None` or above `max_gas_cost` gives `GasCap`; `profit <= cost` gives `GasExceedsProfit`; `profit <= cost + min_profit` gives `BelowMin`; the rest are submitted together best first with `floor = cost + min_profit`, carrying the engine's verdicts. Every evaluation becomes one `Decision`, sent with `try_send`.
- `Exit { Restart, Done }`. `async run(&self, max_steps: Option<u64>) -> Result<Exit>`: `step(0, None)` first, then loop: `changes()`; on error `warn!` and sleep 500 ms; `restart` returns `Exit::Restart`; `reload` steps with `None`; else steps with the touched set. `max_steps` caps loop iterations for tests and returns `Exit::Done` when reached; `None` loops until a restart. A `tracing` span per step records elapsed time.
- `testkit::MockChain`: scripted `changes` results, snapshots, per pool cost, scripted submit verdicts, recorded submits and controls, settable `in_flight` and `status`.

- [ ] **Step 1: Failing tests**: `submits_when_profit_clears_cost` (floor equals cost plus min), `gas_exceeds_profit_verdict`, `below_min_verdict`, `gas_cap_verdict_when_cost_above_max`, `gas_cap_verdict_when_fee_cap_hit`, `paused_submits_nothing`, `quote_error_becomes_decision`, `engine_verdicts_pass_through` (mock returns `Locked(7)`), `keeps_running_after_rpc_error`, `only_touched_pairs_evaluated`, `full_decision_channel_does_not_block`, `restart_update_exits_run`, `pause_sends_control`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add chain trait and bot step loop`.

---

### Task 9: EVM bindings, adapter config, V2 venue

**Files:** Create `src/chain/evm/{mod,bindings,convert,config}.rs`, `src/chain/evm/venue/{mod,v2}.rs`; modify `src/chain.rs` (`pub mod evm;`), `src/lib.rs` if needed.

**Interfaces (produces):**
- `bindings.rs`: `sol!` with `#[sol(rpc)]` for `IUniswapV2Factory` (`getPair`, `allPairs`, `allPairsLength`), `IUniswapV2Pair` (`token0`, `token1`, `getReserves`, `swap`, `event Sync`), `IUniswapV2Router02` (`getAmountOut`), `IPoolManager` (`event Swap`, `event ModifyLiquidity`), `IStateView` (`getSlot0`, `getLiquidity`, `getTickBitmap`, `getTickLiquidity`), `IV4Quoter` (`quoteExactInputSingle((PoolKey,bool,uint128,bytes))` returning `(uint256 amountOut, uint256 gasEstimate)`, called with `eth_call`), `IMulticall3` (`aggregate3`, `getEthBalance`), `IERC20` (`balanceOf`, `transfer`, `decimals`), `IWETH` (`deposit`), and `sol!(#[sol(rpc)] FlashArbitrage, "abi/FlashArbitrage.json");`.
- `convert.rs`: `to_address(&Id) -> Result<Address>`, `to_id(Address) -> Id`, `From<alloy::transports::TransportError> for Error` (`Rpc`), `From<alloy::contract::Error> for Error` (`Contract`).
- `config.rs`: `Family { V2, V4 }`; `EvmVenue { name, family, address, fee_bps: Option<u16>, tick_words: Option<u16> }`; `ForkToken { address, amount: Amount, whale }`; `ForkSection { fund_eth: Amount, tokens: Vec<ForkToken> }`; `EvmConfig { chain_id, pool_manager, state_view, multicall, priority_fee: u128, max_fee_per_gas: u128, gas_v2_v2, gas_v2_v4, gas_v4_v4, max_gas_limit: u64, venues, fork: Option<ForkSection> }` parsed from `NetworkConfig.adapter`; rejects `priority_fee > max_fee_per_gas`, a V2 venue without `fee_bps`, a V4 venue without `tick_words`.
- `V2Pool::new(venue: String, pool: Id, token0: Id, token1: Id, reserve0, reserve1, fee_bps)`, `set_reserves`, `Venue` impl; `describe()` is `"{venue} {fee_bps}bps r0={reserve0} r1={reserve1}"`.

- [ ] **Step 1: Failing tests**: `quote_out_matches_uniswap` (1000 in, reserves 1000/2000, 30 bps gives 998), `fee_is_not_hardcoded`, `zero_reserves_are_insufficient`, `uint112_max_reserves_do_not_overflow`, `describe_names_venue`, `address_roundtrip`, `short_id_rejected`, `evm_config_parses_ethereum`, `v2_venue_needs_fee`, `priority_fee_above_max_fee_rejected`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add EVM bindings and V2 venue`.

---

### Task 10: V4 venue and tick windows

**Files:** Create `src/chain/evm/ticks.rs`, `src/chain/evm/venue/v4.rs`; modify `src/testkit.rs`, `Cargo.toml` (bench), `benches/step.rs`, `docs/superpowers/reviews/bench.md`, and if the bench decides so `src/strategy.rs`, `Makefile`.

**Interfaces (produces):**
- `V4Key { currency0: Address, currency1: Address, fee: u32, tick_spacing: i32, hooks: Address }`, `V4Key::pool_id(&self) -> B256`.
- `ticks.rs`: `BoundedTicks { ticks: Arc<Vec<Tick<i32>>>, lower: i32, upper: i32 }` implementing `uniswap_v3_sdk::TickDataProvider` (its methods are `async`; ours return immediately), erroring on any lookup or next tick search outside `[lower, upper]`. `fetch_window(provider, state_view, pool_id, tick_spacing, tick, words) -> Result<BoundedTicks>`: reads `getTickBitmap` for the `words` words each side of the current word, then `getTickLiquidity` for every set bit, in one multicall each.
- `V4Pool::new(venue: String, pool: Id, base_alias: Id, key: V4Key, decimals: (u8, u8), sqrt_price_x96: U160, liquidity: u128, tick: i32, ticks: BoundedTicks)` builds an SDK `Pool` via `Pool::new_with_tick_data_provider` with `Currency` values from the key and decimals (native for `Address::ZERO`). `quote_out` calls `pool.get_output_amount(..).now_or_never()` (`futures::FutureExt`); `None` or an SDK error maps to `InsufficientLiquidity`. Pools with a non zero protocol fee are not built (the SDK cannot price them); `pairs.rs` skips them with a warning.
- `apply_swap(sqrt_price, liquidity, tick)`, `apply_modify(tick_lower, tick_upper, delta: i128)`, `replace_window(BoundedTicks)`, `near_edge(&self) -> bool` (within one bitmap word of `lower` or `upper`; only a refetch trigger), `base_reserve(&self) -> Amount` (`L * 2^96 / sqrtP` when the base is currency0, `L * sqrtP / 2^96` otherwise), `describe()` as `"{venue} {fee/100}bps p={sqrt_price} L={liquidity} t={tick}"`. `tokens()` reports native currency as `base_alias`.
- `testkit::v4_fixture()` returns a hand built `V4Pool` (one position across ticks -600..600, spacing 60) for tests and benches.

- [ ] **Step 1: Failing tests**: `quote_matches_unbounded_reference` (same ticks in the SDK's `TickListDataProvider`), `quote_past_window_errors`, `native_reports_weth`, `apply_swap_moves_price`, `apply_modify_matches_fresh_pool`, `replace_window_extends_quotes`, `near_edge_detected`, `base_reserve_from_liquidity`, `describe_names_venue`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add V4 venue over a bounded tick window`.
- [ ] **Step 6: Bench decision.** Add 50 `v4_fixture` pools to the bench mix. Run with `release` and `--profile release-max`, sequential and with a `rayon` variant of `find_all` (`rayon = "1"` added to `Cargo.toml`). Keep `rayon` in `find_all` only if its median beats sequential by more than 20%, else remove the variant and the dependency. Point `make run` at `--profile release-max` only if its median beats `release` by more than 10%. Record numbers and both decisions in `bench.md`. **Commit** `Decide parallelism from V4 bench`.

---

### Task 11: Pools, state and feed

**Files:** Create `src/chain/evm/{pairs,state,feed}.rs`.

**Interfaces (produces):**
- `pairs.rs`: `Tracked { V2 { venue: usize, pair, token0, token1, fee_bps }, V4 { venue: usize, key: V4Key } }`; `LegSpec { family: Family, pair: Option<Address>, key: Option<V4Key>, token0: Address, fee_bps: u16 }`; `leg_specs(&[Tracked]) -> HashMap<Id, LegSpec>`.
  - `listed(provider, cfg, evm)`: per token, `getPair` on every V2 venue and the probe keys on every V4 venue (kept when `getSlot0` price is non zero); then `discovery::select` with base reserves read in one multicall.
  - `discover(provider, cfg, evm)`: the V2 venue with the lower `allPairsLength` is scanned over indices `0..min(len, max_scan)`; its pairs holding the base token give candidate tokens; each candidate is looked up like `listed`; then `select`.
- `state.rs`: `State::load(provider, tracked, evm)` (multicall reserves, slot0, liquidity, decimals, protocol fee, `fetch_window`); `apply(&mut self, block, logs) -> Update` (log index order; `Sync`, `Swap`, `ModifyLiquidity`; a removed log or a block number skipping ahead sets `reload`); `near_edge(&self) -> Vec<(Id, B256, i32, i32)>` (id, pool id, spacing, tick); `apply_window(&mut self, id, BoundedTicks)`; `reload_all(provider)`; `snapshots() -> Vec<Arc<dyn Venue>>` (each update replaces that pool's `Arc`).
- `feed.rs`: `Feed::spawn(ws_url, provider, addresses, v4_ids, state_view, tick_words) -> (FeedHandle, mpsc::Receiver<FeedEvent>)`; `FeedEvent::Block(FeedBlock { number, hash, base_fee, gas_used, gas_limit, logs, gap })`, `FeedEvent::Window(Id, BoundedTicks)`; heads by WebSocket `subscribe_blocks`, logs by `get_logs` with `Filter::at_block_hash`; a reconnect or skipped number sets `gap`. `FeedHandle::refetch(&self, id, pool_id, tick_spacing, tick)` runs `fetch_window` in the background and delivers `FeedEvent::Window`.

- [ ] **Step 1: Failing tests**: `sync_updates_reserves`, `swap_updates_v4`, `modify_liquidity_applied_locally`, `removed_log_requests_reload`, `skipped_block_requests_reload`, `foreign_log_ignored`, `old_arc_unchanged_after_apply`, `near_edge_listed_for_refetch`, `window_event_replaces_ticks`, `leg_specs_cover_every_pool`. Logs are hand built; RPC paths use an alloy `Asserter` backed provider named `mock_provider`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add pool state and block feed`.

---

### Task 12: Circuit breakers

**Files:** Create `src/chain/evm/breakers.rs`.

**Interfaces (produces):** `Breakers::new(cfg: config::Breakers)`; `record(&mut self, pools: [&Id; 2], block: u64, reverted: bool, gas_cost: Amount)`; `allows(&self, pool: &Id, block: u64) -> bool` (false in a pool's cooldown or while the global breaker is open); `is_open(&self) -> bool`; `resume(&mut self)`.

- [ ] **Step 1: Failing tests**: `pool_cooldown_after_reverts`, `cooldown_expires`, `success_does_not_count`, `global_breaker_opens_on_revert_gas`, `revert_gas_outside_window_ignored`, `resume_closes_global`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add revert circuit breakers`.

---

### Task 13: Holdings

**Files:** Create `src/chain/evm/holdings.rs`.

**Interfaces (produces):** `Balances { executor_weth: Amount, accounts: Vec<(Address, Amount)> }`; `TxRequest { from: usize, to: Address, value: Amount, data: Bytes, kind: EntryKind }`; `Holdings::new(limits, owner: usize, treasury: Address, executor: Address, weth: Address)`; `on_block(&self, balances: &Balances, fee_ok: bool) -> Vec<TxRequest>` returning the owner's `sweep(weth, executor_weth - executor_keep)` when `executor_weth > executor_max_hold`, and an operator's `balance - account_keep` transfer to the treasury when above `account_max_hold`; nothing when `!fee_ok` or below `min_transfer`; `set_limits(Limits)`.

- [ ] **Step 1: Failing tests**: `sweep_drops_to_keep`, `no_sweep_below_cap`, `transfer_below_minimum_skipped`, `account_forward_drops_to_keep`, `nothing_sent_over_fee_cap`, `live_limits_apply`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add holding limits with sweeps and forwards`.

---

### Task 14: Trade journal

**Files:** Create `src/journal.rs`, `src/bin/journal.rs`; modify `src/lib.rs`.

**Interfaces (produces):** `JournalEntry { ts: String (RFC 3339), event: TradeEvent }` serialized as one JSON line (amounts as decimal strings); `Journal::spawn(path) -> Result<JournalTx>` (creates parent dirs, appends, flushes after every line); `JournalTx::record(&self, TradeEvent)` async, bounded channel of 1024, `send().await` so nothing is dropped; `read_last(path, n) -> Result<Vec<JournalEntry>>`. `src/bin/journal.rs` prints the last 20 entries as a table (time, kind, state, pair, account, tx, profit, gas used).

- [ ] **Step 1: Failing tests**: `entry_roundtrips_as_json_line`, `appends_across_reopen` (write two, drop, reopen, write one, three lines in order), `creates_missing_dir`, `read_last_returns_tail_in_order`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** `Add append only trade journal`.

---

### Task 15: Accounts and trade engine

**Files:** Create `src/chain/evm/{accounts,engine}.rs`.

**Interfaces (produces):**
- `accounts.rs`: `signers(sources) -> Result<Vec<PrivateKeySigner>>` (`MnemonicBuilder::<English>` with `.index(i)`; a missing or empty var gives `Env(name)`; duplicate addresses give `Config("duplicate account <addr>")`).
- `EngineCmd { Submit { ops: Vec<Opportunity>, reply: oneshot::Sender<Vec<(Opportunity, Verdict)>> }, Block { block: FeedBlock, applied: u64 }, Control(Control), SetLive(Live) }`.
- `Engine<P: Provider>::new(provider: P, signers, owner: usize, evm: EvmConfig, live: Live, breakers: Breakers, holdings: Holdings, executor: Address, legs: HashMap<Id, LegSpec>, journal: JournalTx, events: mpsc::Sender<TradeEvent>)`; `Engine::spawn(self) -> (mpsc::Sender<EngineCmd>, watch::Receiver<Published>)` where `Published { fee_per_gas: Option<u128>, gas_limits: [u64; 3], in_flight: usize, status: Status }`.
- Pure parts, tested without a network:
  - `fee_per_gas(next_base_fee, evm) -> Option<u128>` and `fee_cap(next_base_fee, evm) -> u128` per Global Constraints;
  - `next_base_fee(parent_base_fee, gas_used, gas_limit) -> u128` (EIP 1559);
  - `build_legs(op, &legs) -> Result<(Leg, Leg)>`: buy leg swaps base for `op.token` on `buy_pool`, sell leg swaps `op.token` for base on `sell_pool`; `zeroForOne` is `input token == token0`; V2 `data = abi.encode(pair, zeroForOne, fee_bps)`; V4 `data = abi.encode(PoolKey, zeroForOne)`;
  - `accept(&mut self, ops, block) -> Vec<(Opportunity, Verdict)>`: best first; `GasCap` when `fee_per_gas` is `None`; overlapping or locked pools give `Locked(id)`; `Breakers::allows` false gives `CircuitOpen`; operator lane (an operator account with its own nonce) with fewest trades in flight among those at or above `account_min_gas`, none gives `LowGas`; accepted trades become `Planned`.
- Effectful parts on the task: sends `Planned` trades (fee cap and tip per Global Constraints, gas limit from the cache), each lane's nonces consecutive; a send error at nonce N marks that trade `Dropped`, resyncs the lane nonce with `get_transaction_count` (pending), and re-signs the lane's later unsent trades with fresh nonces. On `Block`: `eth_getTransactionReceipt` for each `Sent` trade; gas cache update (Global Constraints numbers); `Breakers::record`; locks released for finished trades once `applied >= mined_block`; trades `Sent` for more than `trade_timeout` blocks get a cancel (zero value self transfer, same nonce, fee cap and tip bumped 10%, still under `max_fee_per_gas` by construction), and their locks stay until the cancel or the original is mined; balances read with one Multicall3 call; `Holdings::on_block` requests sent from their lanes. Every state change is sent to the journal (awaited) and to `events` (`try_send`). `Control::Resume` also calls `Breakers::resume`; `Control::Sweep` sweeps the full executor balance now.

- [ ] **Step 1: Failing tests** (pure parts directly; effectful parts with `mock_provider`): `overlapping_ops_keep_the_better`, `locked_pool_refused_for_every_account`, `lock_held_until_block_applied`, `timeout_sends_bumped_cancel`, `cancel_fits_under_cap`, `locks_held_until_cancel_or_original_mined`, `send_error_resigns_later_nonces`, `least_busy_operator_chosen`, `low_gas_account_skipped`, `owner_is_not_an_operator_lane`, `gas_cache_tracks_highest_used`, `gas_limit_never_above_max`, `out_of_gas_raises_limit`, `next_base_fee_follows_eip1559` (full block +12.5%, empty block -12.5%), `fee_cap_blocks_sending`, `legs_built_for_v2_and_v4`, `published_values_update`, `every_state_change_journaled`, `full_event_channel_does_not_block`, `derives_known_mnemonic_addresses`, `duplicate_account_rejected`, `empty_env_var_is_missing`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Commit** in two parts: `Add mnemonic account loading`, then `Add trade engine with pool locks`.

---

### Task 16: Executor lifecycle, EVM chain, entry point

**Files:** Create `src/chain/evm/executor.rs`; modify `src/chain/evm/mod.rs`, `src/main.rs`.

**Interfaces (produces):**
- `executor.rs`: `ExecutorRecord { address, owner, treasury, chain_id }` stored as JSON in `.arbitrage-<network>.json`; `executor_action(record: Option<&ExecutorRecord>, code_present: bool, owner, treasury, chain_id) -> ExecutorAction { Reuse(Address), Deploy }` (pure); `ensure_executor(provider, record_path, ...) -> Result<Address>` (reads `get_code_at`, deploys `FlashArbitrage(pool_manager, weth, treasury, operators)` from the owner when needed, writes the record, sends `setOperator` for configured operators the contract lacks).
- `EvmChain::connect(cfg: &NetworkConfig, network: &str) -> Result<(EvmChain, mpsc::Receiver<TradeEvent>)>`: checks `eth_chainId` against `chain_id` (error names both); signers; pools via `listed` or `discover` (`Config("no tradable pairs")` when empty); `ensure_executor`; `State::load`; `Feed::spawn`; `Journal::spawn(cfg.journal)`; engine spawn.
- `Chain for EvmChain`: `changes` reads the next `FeedEvent`: `Window` is applied and returns an empty update; `Block` is applied, `near_edge` pools go to `FeedHandle::refetch`, then `EngineCmd::Block` is sent; on `gap` it reloads state and checks `get_code_at(executor)`, returning `restart: true` when the code is gone. `cost` is gas limit for the pair's families times fee per gas from `Published`; `submit` sends `EngineCmd::Submit` and awaits the reply; `control`, `apply_live` send commands; `in_flight` and `status` read `Published`; `base` returns the WETH id.
- `main.rs`: `dotenvy::dotenv()`; `NETWORK` default `ethereum`; `--headless`; takes the instance lock with `std::fs::File::try_lock` on `.arbitrage-<network>.lock` (held for the process lifetime; `Error::Lock` when taken); loads config, matches `kind`, connects, runs `Bot::run(None)` (Task 20 replaces this with the supervisor); logs pair, pool and account counts, executor, treasury and gas caps.

- [ ] **Step 1: Failing tests**: `executor_record_roundtrip`, `reuse_when_code_and_owner_match`, `deploy_when_owner_differs`, `deploy_when_code_missing`, `missing_code_after_gap_requests_restart` (`mock_provider` returns empty code), `second_instance_refused` (take the lock twice).
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Smoke:** `cp .env.example .env`; `make fork`; `make fund` is not ready yet, so for this smoke run use anvil's prefunded mnemonic accounts (the anvil mnemonic is the default and its accounts start with 10,000 ETH); `make run-headless` logs pairs, pools, executor, gas caps and one step per block with no trades.
- [ ] **Step 6: Commit** `Add EVM chain and entry point`.

---

### Task 17: Fork funding and sweep

**Files:** Create `src/chain/evm/fund.rs`, `src/bin/fund.rs`, `src/bin/sweep.rs`.

**Interfaces (produces):** `Funded { address, eth: Amount, tokens: Vec<(Address, Amount)> }`; `fund(provider, fork: &ForkSection, accounts: &[Address]) -> Result<Vec<Funded>>`: `anvil_nodeInfo` first (`Config("funding needs an anvil fork")` on error, nothing else called); `anvil_setBalance` to `fund_eth`; per token `anvil_dealERC20(account, token, amount)`, on error `anvil_impersonateAccount(whale)`, `transfer`, `anvil_stopImpersonatingAccount`. `src/bin/fund.rs` funds every configured account and logs balances. `src/bin/sweep.rs` reads `.arbitrage-<network>.json` and has the owner call `sweep(weth, balance)`, logging `nothing to sweep` when the balance is zero.

- [ ] **Step 1: Failing test**: `refuses_non_anvil` (`mock_provider` with `anvil_nodeInfo` erroring; asserts no other request).
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Smoke:** `make fork`, `make fund` shows ETH and four token balances per account; `make sweep` logs `nothing to sweep`.
- [ ] **Step 6: Commit** `Add fork funding and profit sweep`.

---

### Task 18: Fork tests

**Files:** Create `tests/common/mod.rs`, `tests/fork.rs`, `tests/refork.rs`.

**Interfaces (produces):** `common::setup()` runs once (`std::sync::Once`): `unsafe { std::env::set_var("MNEMONIC_1", ANVIL_MNEMONIC) }` and the RPC vars from `.env.example` defaults. `common::Ctx { cfg, chain: Arc<EvmChain>, bot: Bot<EvmChain>, provider, events }` built by `common::ctx(overrides: impl Fn(&mut NetworkConfig))` with a fresh journal path under `target/test-journals/`, after `fund`. `common::snapshot()` / `revert(id)`. `common::dump(provider, pair, weth_amount)` wraps ETH from anvil account 5, sends WETH to a V2 pair and calls `swap` for the matching output. Tests in `fork.rs` wrap each body in snapshot and revert.

- [ ] **Step 1: Tests in `fork.rs`**:
  - `fund_credits_eth_and_tokens`.
  - `v2_quote_matches_router`; `v4_quote_matches_quoter` (deepest WETH/USDC V4 pool, 1 WETH in).
  - `arbitrages_v4_against_sushiswap`: `dump` 10% of the SushiSwap WETH/USDC WETH reserve; `bot.step` accepts one trade; after `changes()` returns the next blocks, the trade is `Executed`; the executor's WETH grew by at least `floor`; one leg is V4 and one V2.
  - `stale_trade_reverts_no_profit`: dump, `step` accepts a trade, then `anvil_setAutomine(false)`, dump the opposite way first in the mempool with a higher tip, mine; the trade's receipt reverts with `NoProfit`.
  - `two_pairs_from_two_accounts`: dump on WETH/USDC and WETH/DAI SushiSwap pairs; one step submits two trades; their `account` fields differ; both `Executed`.
  - `no_retrade_before_own_block_applied`: right after a trade is accepted, `step` again on the same touched set gives `Locked`.
  - `idle_without_imbalance`: a step submits nothing.
  - `fee_cap_stops_trading`: `anvil_setNextBlockBaseFeePerGas` above `max_fee_per_gas`, dump; the decision is `GasCap` and nothing is sent.
  - `auto_sweep_above_cap`: config override with `executor_max_hold = "1"`, `executor_keep = "0"`, `min_transfer = "1"`; after a trade, the next blocks sweep to the treasury.
  - `rejects_non_operator` and `sweep_only_pays_treasury` (impersonated stranger and operator).
  - `discovers_pairs`: override `mode = "discover"`, `max_scan = 500`; `connect` succeeds and tracks at least one pair holding WETH.
  - `executor_reused_after_restart`: drop the chain, connect again; same executor address.
  - `journal_records_executed_trade`: after the arbitrage flow, `read_last` holds planned, sent and executed entries for the trade with `gas_used` and `actual_profit` set.
- [ ] **Step 2: `tests/refork.rs`**: `refork_requests_restart`: connect, call `anvil_reset` with `forking.jsonRpcUrl = FORK_URL`, the next `changes()` returns `restart: true`. No snapshot (the reset drops them); it runs after `fork.rs` through `make test-fork`.
- [ ] **Step 3:** `make fork`, `make test-fork`: 15 plus 1 passed. A pair too shallow for the dump to beat gas (SushiSwap WETH reserve under `min_base_reserve`) is replaced by another listed token, with the reason in the commit body.
- [ ] **Step 4: Commit** `Add fork tests for the arbitrage loop`.

---

### Task 19: Terminal UI

**Files:** Create `src/ui/{mod,app,views,detail}.rs`; modify `src/main.rs`, `src/lib.rs`, `Cargo.toml` (`ratatui = "0.30"`, using `ratatui::crossterm`).

**Interfaces (produces):**
- `UiCommand { Pause, Resume, Sweep, Reload, Quit }`, sent by the UI to the runner in `main.rs` (and in Task 20 to the supervisor) over `mpsc`.
- `App`: last step's decisions, last 500 trade events, latest `Status`, totals (net, gross, gas, executed, reverted, dropped), log lines, last `ReloadEvent`, selection, tab. `on_decision`, `on_trade`, `on_status`, `on_reload`, `on_key(KeyCode) -> Option<UiCommand>` (`p` toggles Pause and Resume, `s` Sweep, `r` Reload, `q` Quit; `f` cycles a verdict filter inside `App`).
- `views::draw(frame, &App)` and `detail::draw_detail(frame, &App)` per the spec's Terminal UI section: venue text from `describe`; leg amounts and the profit curve recomputed with `quote_out` and `sizing::gain` over the decision's snapshots; header shows block, feed lag, next base fee and fee cap against `max_fee_per_gas`. Verdict labels: `SENT`, `skip: min`, `skip: gas`, `skip: gas cap`, `skip: quote`, `locked #N`, `skip: low gas`, `breaker`, `paused`.
- `run_ui(decisions_rx, events_rx, status_rx: watch::Receiver<Status>, reload_rx, commands_tx) -> Result<()>` at 10 fps, restoring the terminal on exit and on panic.
- `main.rs`: unless `--headless`, spawns `run_ui`; `UiCommand::Pause`/`Resume`/`Sweep` go to `Chain::control`; `Quit` stops submitting, waits until `in_flight() == 0` or `trade_timeout` blocks pass (by `status().block`), then exits. A `status` publisher task copies `chain.status()` into the `watch` every 200 ms.
- Amount formatting: wei to decimals of the token (18 for WETH and ETH) with 4 places.

- [ ] **Step 1: Failing tests** with `ratatui::backend::TestBackend`: `opportunities_show_verdicts` (all nine labels), `header_shows_fee_against_cap`, `trade_row_shows_expected_and_actual`, `detail_shows_legs_and_revert`, `keys_map_to_commands`, `amount_formatting` (`1234500000000000000` at 18 decimals is `1.2345`).
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Smoke:** `make run` shows the screen; running `arbitrages_v4_against_sushiswap` in another terminal shows a `SENT` row turning `Executed`.
- [ ] **Step 6: Commit** `Add terminal UI for decisions and trades`.

---

### Task 20: Reload and restart

**Files:** Create `src/supervisor.rs`; modify `src/main.rs`, `src/lib.rs`.

**Interfaces (produces):**
- `ReloadEvent { changes: Vec<String>, error: Option<String> }`.
- `Supervisor<C: Chain, F, Fut>` with `F: Fn(NetworkConfig) -> Fut`, `Fut: Future<Output = Result<C>>`; `main.rs` passes the `EvmChain::connect` closure after matching `kind` (the only place that names `EvmChain`).
- `run(self)`: runs `Bot::run`; listens for `UiCommand` (control, reload, quit), `SIGHUP` (`tokio::signal::unix`), and `Exit::Restart`.
- `reload(&mut self) -> Result<Change>`: `dotenvy::dotenv_override()` then reload the TOML; a parse or validation error keeps the old pipeline and is sent as a `ReloadEvent` with `error`; `Live` calls `Chain::apply_live` and `Bot::apply_live`; `Restart` pauses, waits until `in_flight() == 0` or `trade_timeout` blocks pass (by `status().block`), drops bot and chain, builds new ones, resumes.

- [ ] **Step 1: Failing tests** with testkit `MockChain`: `invalid_config_keeps_old`, `threshold_change_applies_live`, `gas_cap_change_applies_live`, `env_change_restarts`, `restart_waits_for_in_flight`, `restart_waits_at_most_timeout`, `restart_update_rebuilds_chain`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass.
- [ ] **Step 5: Fork test** `reload_adds_operator` in `tests/fork.rs`: write a temp config adding index 3 to the mnemonic account, point the supervisor at it, reload; the executor address is unchanged and `operators(index 3)` is true.
- [ ] **Step 6: Commit** `Add config reload with in place restart`.

---

### Task 21: README

**Files:** Modify `README.md`.

- [ ] **Step 1:** remove the mnemonic line (now commented in `.env.example`); name Uniswap V4, Uniswap V2 and SushiSwap in the intro; add a Development section: `cp .env.example .env`; each `make` target in one line; fork targets need `make setup` again after `make teardown`; accounts (env var names in `[[accounts]]`, `owner` picks the deployer, `treasury` is a plain address); gas caps (`max_gas_cost` in the core table, `max_fee_per_gas`, `priority_fee`, gas limits in `[adapter]`); the journal file and `make journal`; adding a network (copy `config/ethereum.toml`) and a DEX family (venue in `chain/evm/venue`, library in `contracts/legs`, `Family` variant). Leave all other sections unchanged.
- [ ] **Step 2: Commit** `Document the development workflow`.

---

### Task 22: Verify, remove Foundry, open the PR

- [ ] **Step 1:** `make lint && make test && make test-contracts`; `make fork`, `make fund`, `make test-fork`. All pass.
- [ ] **Step 2:** stop anvil; `make teardown`; `~/.foundry` is gone, no foundry line remains in the profile files, `command -v forge anvil` prints nothing.
- [ ] **Step 3:** `cargo clean && cargo build && cargo test`: all pass.
- [ ] **Step 4:** `git push -u origin feature/flash-arbitrage`; `gh pr create --base main --title "Flash arbitrage bot on V4 flash loans" --body-file <scratch file>` with what it does, the make workflow, results from Steps 1 and 3, the bench numbers and decisions from `bench.md`, and follow ups (Solana, more venue families, hooked and non standard V4 pools, pools with a protocol fee). No agent attribution.
