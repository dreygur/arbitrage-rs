[![ReadMeSupportPalestine](https://raw.githubusercontent.com/Safouene1/support-palestine-banner/master/banner-support.svg)](https://techforpalestine.org/learn-more)

# Arbitrage Bot

A flash-loan arbitrage bot for Uniswap V4, Uniswap V2 and SushiSwap. It watches pools for price gaps and trades them through one executor contract, on a local anvil fork of mainnet. A terminal UI shows each decision and trade.

## Screenshots

Captured from a live fork run, with 16 swaps pushed through SushiSwap and Uniswap V2 on the USDC, USDT, DAI and WBTC pairs.

![Main screen](docs/images/main.png)

The main screen: 15 trades executed and 1 reverted, for 3.3128 ETH net. Two sweeps moved 2.3864 ETH to the treasury. Each opportunity shows its verdict: sent, skipped for gas or for the minimum profit, or locked behind a trade in flight. Log times are in the local zone.

![Decision detail](docs/images/detail.png)

A sent decision: the legs, the profit by size curve, the send parameters, and the result. It was decided 16ms after the block was read and sent at 17.6ms, and it earned 0.0643 ETH.

![Pairs tab](docs/images/pairs.png)

The Pairs tab: each venue pair with its last spread and verdict.

![Config tab](docs/images/config.png)

The Config tab shows the running config, with every key commented.

![Connecting](docs/images/connecting.png)

Startup: the screen shows the connect while pools load, before any trading.

## Credentials

Mnemonic: `impose maximum pencil dragon steel kick unusual firm steak jeans snow maid`

## Development

Copy the env file first. It ships the anvil test mnemonic as `MNEMONIC_1`.

```
cp .env.example .env
```

Run it on a fork, each in its own terminal:

```
make fork
make fund
make run
```

To see a trade, push one pool off price, for example a large swap on SushiSwap's WETH/USDC pair from another anvil account.

Make targets:

- `make setup` installs Foundry and forge-std.
- `make abi` builds the contract and copies its ABI to `abi/`.
- `make test-contracts` runs the forge tests against `FORK_URL`.
- `make fork` starts anvil as a mainnet fork, one block every 2 seconds, pinned at the block in `target/fork-block` so a restart reads state from anvil's cache. Stop it with Ctrl-C so the cache is written.
- `make fork-fresh` drops that pin and forks the current mainnet head.
- `make refork` resets the running anvil to the current mainnet head.
- None of them echoes `FORK_URL`, which usually holds a provider key, but anvil prints its fork URL in its startup banner, so keep that terminal's output private.
- `make fund` funds the configured accounts with ETH and tokens on the fork.
- `make sweep` sends the executor's base token profit to the treasury.
- `make journal` prints the last 20 trade journal entries.
- `make run` starts the bot with the terminal UI.
- `make run-headless` starts the bot without the UI.
- `make test` runs the unit tests. It needs no network.
- `make test-fork` runs the fork tests.
- `make bench` runs the benchmarks.
- `make lint` runs `cargo fmt --check` and clippy.
- `make teardown` removes the Foundry install that `make setup` added.

After `make teardown`, run `make setup` again before any fork target.

The first `make run` on a newly pinned block connects slowly, about ten minutes, because anvil fetches pool state from `FORK_URL` one storage slot at a time, about ten thousand requests. That costs credits on a paid key. Later runs on the same block read anvil's disk cache and connect in seconds. A free node such as publicnode keeps old state for only a few minutes, so an old pin needs a node that serves historical state, or `make fork-fresh`.

### Fork tests

`make test-fork` starts its own anvil on port 8546, pinned to a block. It stops it when the tests end.

The first run fetches state from the public RPC and takes about 16 minutes. A repeat run on the cached block takes about 72 seconds. Plain `make test` stays hermetic.

### Config

Each network has a file in `config/`. Keys below are from `config/ethereum.toml`.

- Accounts: each `[[accounts]]` entry names an env var, such as `MNEMONIC_1`, and the `indices` to derive. `owner` is the index that deploys the executor. `treasury` is a plain address.
- Gas caps: `max_gas_cost` at the top level, `max_fee_per_gas`, `priority_fee` and the gas limits (`gas_v2_v2`, `gas_v2_v4`, `gas_v4_v4`, `max_gas_limit`) under `[adapter]`.
- Journal: trades are written to `logs/trades-<network>.jsonl`. Read it with `make journal`.

### Adding a network

Copy `config/ethereum.toml` and edit it. Run with `NETWORK=<name>`.

### Adding a DEX family

1. Add a venue in `src/chain/evm/venue`.
2. Add a library in `contracts/legs`.
3. Add a `Family` variant.
