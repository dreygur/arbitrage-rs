# Stage 6: Comprehension Test

Reviewer: egm-comprehension (fresh context). No blockers; 13 major, 9 nits.

## What this proposes

The spec and plan replace a small ethers-rs demo with a Rust arbitrage
bot. It tracks WETH pairs on Uniswap V4, Uniswap V2 and SushiSwap,
prices every pool locally from in memory state fed by WebSocket logs,
and sizes each trade with an integer ternary search. When the profit
beats gas plus a threshold, it sends a transaction to a custom Solidity
executor. The executor takes a fee free flash loan of ETH from the V4
PoolManager, runs a buy leg and a sell leg (each in a per family
library), repays the loan, and reverts unless the WETH it gained is at
least `minProfit`.

The core (types, Venue and Chain traits, sizing, strategy, discovery,
config, bot loop) knows nothing about any chain or DEX. An EVM adapter
provides the concrete venues, a log feed, pool state, accounts, and an
engine task that owns all sending: per account nonce lanes, cached gas
limits, pool locks and a per trade state machine.

Around this: many signing accounts; holding caps that sweep executor
profit to a keyless treasury address; a ratatui terminal UI; live
config reload with in process restart; a criterion bench; and a
Makefile workflow that installs Foundry, uses it to build the contract
ABI and run an anvil fork funded by impersonating a whale, then removes
Foundry so plain `cargo build` and `cargo test` still work.

## How the existing system works, per this document

The documents say little about the current code. They name
`uniswap.rs`, `arbitrage.rs`, `eth_wallet.rs` and an `approve` demo in
`main.rs` as things to replace, and say `Cargo.toml` holds an invalid
`[build]` table plus `ethers`, `anyhow` and `dotenv`. From the code:
`main.rs` builds one ethers signer from `PRIVATE_KEY` and `RPC_URL`,
wraps V2 router and factory bindings and an `IArbitrage` contract in an
`EthWallet`, prints one V2 pair's reserves, and sends an `approve` on
that pair's LP token to the router. That matches the documents; nothing
is carried forward, so the thin description does not block
understanding.

## Findings

### Major

1. Task 16 README note says "the first is the treasury unless
   `treasury` says otherwise"; everywhere else the treasury is a keyless
   address and the first account is the owner.
2. Spec fork test says "raises the owner's WETH"; the executor keeps the
   gain. Should say executor.
3. Speed says V4 ticks are refetched after `ModifyLiquidity`; the cache
   table says `ModifyLiquidity` is applied locally and refetch happens
   near the window edge. Plan follows the table.
4. Spec's four task pipeline and single writer state task do not match
   the plan, where `changes()` applies blocks on the bot's call and only
   the engine is its own task.
5. Named UI skip reasons cannot be produced: `submit` returns only
   accepted ops, `find_all` drops errored pairs, and `Verdict` has no
   variants for quote error, tick window miss or low gas.
6. `estimate_gas` is an RPC call; when the first estimate (and the
   post out of gas estimate) happens is unstated, and if it is at trade
   time it breaks "no RPC between decide and send".
7. `min_base_reserve` is undefined for V4 pools, which have no per pool
   reserve.
8. "Sweep never in the same block as our trade" has no mechanism; the
   bot cannot pick blocks. State one (for example sweep only when no
   trade is `Sent`, hold trades while a sweep is pending).
9. Executor is deployed on every start; previous profit is stranded;
   `make sweep` needs a hand copied `EXECUTOR` that is not in
   `.env.example` or the README.
10. `diff` compares only `NetworkConfig`, so changes to `RPC_URL`,
    `WS_URL` or mnemonic values in `.env` return `Change::None` and never
    restart.
11. `rescan_blocks` contradicts "discovery runs once" and no task
    implements it.
12. "Tick" means both an evaluation cycle and a V4 price tick. Use
    "step" or "block" for the cycle.
13. Task 12 expects 14 passing fork tests but lists 13; the 14th comes
    in Task 14.

### Nit

14. "dreygur" style is referenced without a link or a statement that
    the plan's list is complete.
15. `min_profit` means three things (config threshold, the field `find`
    sets to 0, the contract's gas plus threshold). Define or rename.
16. "Quote errors count as no profit, which keeps the curve unimodal"
    does not obviously hold; flat regions hurt ternary search. The plan
    gives floor rounding as the reason for the final scan. Explain once.
17. "Tip" in `cost` has no source or config key.
18. Spec's subscription list omits `ProtocolFeeUpdated`.
19. `run-headless` is in the spec's Makefile table but not the plan;
    spec funding text says "the bot" while elsewhere every account is
    funded.
20. "Two accounts never send overlapping trades" has no fork test; only
    hermetic engine tests cover it, and the plan does not say so.
21. `profit_curve` signature elided; curve stored twice; `Outcome`
    unused once `TradeState` exists.
22. Unexplained external references: Artemis, the superpowers skill
    names, `.claude/settings.json` attribution keys (and why that file
    is committed).
