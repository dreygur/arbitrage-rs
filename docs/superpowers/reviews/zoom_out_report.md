# Stage 5: Initial Zoom Out Review

Reviewer: egm-zoom-out (fresh context). Targets: the design doc and the
implementation plan. Context: `src/`, `Cargo.toml`, `README.md`.

Codebase reuse: the existing code (about 150 lines of ethers demo) has
nothing reusable beyond the V2 pair and factory ABI text in
`src/uniswap.rs`, which can be pasted into `sol!`. Replacing it is the
right call.

## Blocker

1. **The fork does not follow the latest block.** `anvil --fork-url`
   pins state at the fork block and then mines only our own
   transactions. No mainnet blocks arrive, nobody else swaps, and no
   reorgs or feed gaps happen. Reorg and gap reloads, the HTTP
   fallback feed, `ProtocolFeeUpdated` tracking, searcher races and
   most base fee work can never trigger outside synthetic tests.
   Following mainnet needs periodic `anvil_reset`, which wipes the
   executor, nonces and cached state, and the design does not handle
   that. Choose and document one of: (a) a static fork, with these
   paths cut to one "reload everything on reconnect" path and speed
   measured by bench and fork tests; or (b) a followed fork via
   `anvil_reset`, designed as a full pipeline restart including
   executor redeploy.

## Major

2. **Core config is EVM and DEX shaped.** `Family { V2, V4 }`,
   `state_view`, `fee_bps`, `tick_words`, `pool_manager`, `chain_id`
   and `whales` sit in core `config.rs`, so a Solana adapter would have
   to edit core. Keep neutral fields in core and put the rest in an
   `[adapter]` table parsed by `chain/evm`; any adapter table change
   means `Restart`.
3. **`supervisor.rs` is EVM specific but top level.** It calls
   `EvmChain::connect_reusing`, `Engine::set_limits` and `setOperator`.
   Move it under `chain/evm`, or give `Chain` neutral hooks
   (`apply_live`, rebuild).
4. **Two contradictory data flow models.** The spec describes four
   tasks (feed, state, bot, engine); the plan applies state inside
   `changes()` on the bot's task and refetches V4 windows there, which
   puts RPC reads between event and decision. Pick one. Suggested: keep
   the pull model, spawn only feed and engine, and move window refetch
   off the hot path.
5. **`submit` cannot carry skip reasons.** Locks and gas checks live in
   the engine but `submit` returns only accepted ops, so `Locked(N)` and
   low gas verdicts cannot reach the UI. Return a verdict per op, or let
   the engine emit decisions. Settle it in the trait now.
6. **Rayon and fat LTO before measurement.** Four tokens over a handful
   of pools will not repay rayon's overhead, and fat LTO slows every
   release build in development. Write `find_all` sequentially, move the
   bench to right after Task 4, add parallelism and LTO only if the
   bench shows a gain.
7. **UI needs leak into hot path types.** `Opportunity.curve` allocates
   a Vec per opportunity and `Decision.curve` duplicates it; "pool state
   at inclusion" needs an extra RPC per receipt plus history. Drop
   `curve` from `Opportunity`, recompute it in the UI from the snapshot
   `Arc`s in the `Decision`, and defer the inclusion comparison.
8. **Account forwarding undoes funding.** Operators never receive
   profit, so only `make fund` (100 ETH) ever pushes an account over
   `account_max_hold` (2 ETH); the first block then forwards 98 ETH
   away. Keep the executor limits and `account_min_gas`, drop account
   forwarding or make `fund_eth` fit under the cap.
9. **Executor redeployed every start.** Profit in the previous run's
   executor is orphaned; `make sweep` needs a hand copied `EXECUTOR`;
   there are three sweep paths. Sweep on shutdown, or deploy at a
   deterministic address (CREATE2 or an `executor` config address reused
   when code exists), which also folds `connect_reusing` into `connect`.
10. **Whale impersonation reinvents anvil.** Anvil's default accounts
    start with 10,000 ETH and `anvil_setBalance` funds any address in
    one call. Suggested `fund`: check anvil, `anvil_setBalance` each
    account.
11. **Executor storage ignores the V4 pattern.** `unlock(data)` passes
    `data` to `unlockCallback` and only calls back the `unlock` caller.
    Storing legs costs about 20k gas per slot, more than the transfer
    saving claimed; the `unlocking` flag adds nothing past the
    PoolManager check. Encode legs, amount and minProfit in the unlock
    payload (v4-periphery `SafeCallback` style); use transient storage
    if a reentrancy flag is wanted.
12. **README publishes a mnemonic** and Task 16 keeps it. Remove it and
    point to `.env.example`.
13. **Discovery is speculative for four tokens.** It adds a module,
    multicall enumeration, `max_scan`, `min_base_reserve` and a fork
    test, while defaults and tests use list mode. `rescan_blocks`
    contradicts "discovery runs once" and no task implements it. Ship
    list mode only, or at least drop `rescan_blocks`.

## Nit

14. Spec drops into implementation detail (the `liquidityNet` rule, gas
    numbers, headroom percentage, loop body and callback pseudocode in
    the plan). State invariants in the spec, arithmetic in the plan.
15. "Sweeps never in a trade block" has no stated hazard. Drop it or
    state why.
16. Wrapping the flash loan costs a deposit and withdraw per trade; take
    WETH directly from the PoolManager when no leg is a native ETH pool.
17. Dead or inconsistent pieces: `Outcome` is replaced by `TradeState`;
    the Task 16 README note contradicts the treasury design; the 50 ms
    debug timing assertion will be flaky.
18. Repo scope: committing `.claude/settings.json`; `teardown.sh`
    deletes any profile line containing `.foundry/bin`, which may remove
    user lines; after teardown `make fork` and `make test-fork` need
    `make setup` again, which the README should say.
