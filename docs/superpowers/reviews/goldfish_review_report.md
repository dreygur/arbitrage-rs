# Goldfish Annotated Review Report

Status: resolved (E1 a; E2 dealERC20 and setBalance with whale fallback; E3 kept with breakers; E4 moved to .env.example; E5 bench first; all F items applied; gas caps and doc comment rules added on request)

**Target:** `docs/superpowers/specs/2026-11-07-flash-arbitrage-design.md` (full file), with plan fixes where a finding spans both.

Sources: stage 5 zoom out (Z), stage 6 comprehension (C), stage 7 design critic (D), structural (S), style (T). Duplicates across reviewers are merged; ids list every source.

## Escalated (needs your decision)

### E1. Fork mechanics  [Z1, D M2, D N5]  Severity: High
*   **Discrepancy:** Anvil forks at one block and then mines only local transactions. It does not follow mainnet, so reorgs, feed gaps, other searchers and fee updates never happen, and with automine every transaction gets its own block, so "several pairs in one block" cannot happen either.
*   ***AI Suggestion:*** ESCALATE
*   ***Justification:*** Which fork model you want is your call and changes what gets built.
*   ***Options:*** (a) static fork with interval mining (`anvil --block-time 2`) so several trades share a block, reorg and gap code kept small (one "reload everything" path) and covered by hermetic tests; plus `make refork` that does `anvil_reset` to the latest block and is treated as a full restart. (b) followed fork that resets on a timer, designed as a full pipeline restart with executor redeploy each time.
*   ***Proposed default:*** (a).

### E2. Funding by whale impersonation  [Z10]  Severity: Medium
*   **Discrepancy:** `anvil_setBalance` funds any address in one call with no whale lookup or failure mode.
*   ***AI Suggestion:*** ESCALATE
*   ***Justification:*** You asked for impersonation of public addresses. Proposed: keep impersonation as the method, fall back to `anvil_setBalance` when no whale can cover the amount.

### E3. Discovery mode  [Z13, C11, D M6]  Severity: Medium
*   **Discrepancy:** Reviewers call discovery speculative; the design critic adds that discovered tokens (fee on transfer, rebasing, honeypots) will look profitable and always revert.
*   ***AI Suggestion:*** ESCALATE
*   ***Justification:*** You asked for "whatever coin seems profitable". Proposed: keep it, drop `rescan_blocks`, and add the revert circuit breaker from F8.

### E4. Mnemonic in README  [Z12]  Severity: Medium
*   **Discrepancy:** README commits a plaintext mnemonic while the spec says secrets live only in `.env`.
*   ***AI Suggestion:*** ESCALATE
*   ***Justification:*** You said these are test data. Remove it from the README in Task 16 and point to `.env.example`, or leave it.

### E5. Parallelism and LTO before measuring  [Z6, T M2]  Severity: Medium
*   **Discrepancy:** rayon and fat LTO are chosen before the bench exists; four tokens over a few pools may not repay rayon's overhead, and fat LTO slows every release build.
*   ***AI Suggestion:*** ESCALATE
*   ***Justification:*** You asked for the fastest possible bot. Proposed: write `find_all` sequentially, move the bench right after strategy (Task 4), keep rayon and LTO only where the bench shows a gain. LTO on in a separate `release-max` profile so `make run` stays quick to build.

## Proposed fixes

### F1. Executor lifecycle  [Z9, C9, D M3]  Severity: High
*   Persist the executor address in `.arbitrage-<network>.json` (gitignored). `connect` reuses it when code exists and owner matches, else deploys and writes the file. `make sweep` reads it; no `EXECUTOR` env var. `connect_reusing` folds into `connect`.

### F2. Contract shape and security  [Z11, D M1, D M5, T, Z16]  Severity: High
*   Legs, amount and `minProfit` travel in the `unlock` payload; no storage writes; only `msg.sender == poolManager` is checked (v4-periphery SafeCallback pattern).
*   "Gain" is the executor's WETH balance after minus before, inside the call.
*   `minProfit` must be greater than zero; the bot sends `cost + min_profit`.
*   `treasury` is immutable in the constructor and `sweep(token, amount)` can only send there, so a stolen owner key cannot redirect funds.
*   Take WETH from the PoolManager directly when neither leg is a native ETH pool; wrap only when needed.

### F3. One pipeline model  [Z4, C4, D N4]  Severity: High
*   Pull model: `changes()` applies the block. Spawned tasks: the feed and the engine only. V4 window refetch runs in the background; a pool with a stale window is skipped (verdict `TickWindow`) until refreshed, so no RPC sits between event and decision. The "four tasks" text goes.

### F4. Block atomicity  [D M8]  Severity: High
*   Feed subscribes to new heads only. On head N it calls `eth_getLogs` by block hash for the tracked addresses and applies them as one block. One local RPC per block, stated as the cost. Logs and heads can no longer interleave.

### F5. Verdicts reach the UI  [Z5, C5]  Severity: High
*   `submit(ops) -> Vec<(Opportunity, Verdict)>`. `Verdict` gains `QuoteError`, `TickWindow`, `LowGas`, `Locked(trade id)`, `CircuitOpen`. `find_all` returns errored pairs as decisions instead of dropping them.

### F6. Chain neutral boundary  [Z2, Z3, C10]  Severity: High
*   Core config keeps neutral keys (kind, base token, min profit, max trade, trade timeout, limits, accounts as env names, treasury as an opaque string). Everything EVM goes in an `[adapter]` table parsed by `chain/evm`. `Chain` gains `apply_live(&Live)`; restart is "drop and rebuild through the adapter factory picked by `kind`", so the supervisor stays in core with no EVM calls. `diff` also hashes the env values the config names (RPC URLs, mnemonics, keys), so `.env` changes restart.

### F7. Sizing correctness  [C16, D M4, T M5]  Severity: High
*   A quote error scores as minus infinity, not zero. Before the ternary search, a binary search finds the largest size that quotes without error and caps the range there. Ties move the upper bound down. The final window scan stays.

### F8. Circuit breakers  [D M6]  Severity: High
*   Per pool: after `max_reverts` reverts within `revert_window` blocks, cool the pool down for `cooldown_blocks`. Global: if gas spent on reverts in the last `revert_window` blocks passes `max_revert_gas`, pause trading and flag it in the TUI.

### F9. Stuck and failed transactions  [D M7]  Severity: High
*   On timeout, send a zero value self transfer at the same nonce with a higher fee, wait for it to be mined, then release the pool lock. If a send at nonce N fails, the later nonces in that batch are cancelled and replanned after the resync.

### F10. Gas without RPC on the hot path  [C6, D N1]  Severity: Medium
*   Default gas limits per family pair come from adapter config (`gas_v2_v2`, `gas_v2_v4`, `gas_v4_v4`) and are updated from receipts (highest seen plus headroom). An out of gas receipt raises the default for that pair. No `estimate_gas` between decision and send.

### F11. Reorg and gap reload covers the tick map  [D M9, Z14]  Severity: Medium
*   Add removed log and feed gap as reload triggers for the V4 tick map row.

### F12. Holding limit details  [Z8, C8, T M7, T M8, D N5]  Severity: Medium
*   Defaults: `fund_eth` 1 ETH, under `account_max_hold` 2 ETH, so funding does not trigger a forward.
*   Drop "never in the same block as a trade"; the owner account is not an operator, so sweeps have their own nonce lane and cannot delay trades. "Low priority" goes.
*   Spell out both keep rules with full key names.

### F13. V4 liquidity filter  [C7]  Severity: Medium
*   For V4, `base_reserve` is the virtual base reserve of the active range: `L / sqrtP` or `L * sqrtP` depending on which side the base token is.

### F14. Spec structure  [S1, S2, S3, S4, S5, S7, S8, S9, D B1, D M1, T M12]  Severity: High
*   Add "Problem Statement" (why replace the demo, who it is for).
*   Add "Alternatives Considered": flash source (V4 vs Balancer vs Aave vs V2 flash swap), sizing (ternary vs closed form), feed (ws logs vs getLogs by hash, chosen per F4), contract shape (shared executor with leg libraries vs per family contracts vs routers), pipeline (own tasks vs Artemis), nonce handling.
*   Add "Risks and Mitigations" with a threat model (owner, operators, PoolManager, arbitrary pool and token contracts), malicious tokens, non standard V4 pools missed by probing, public RPC rate limits, fork drift, channel drops under load.
*   One "Decision log" replaces "Decided" and "Calls made along the way".
*   Config becomes the single key reference (key, type, default).
*   One block lists every core type with fields (`Update`, `Decision`, `Verdict`, `Opportunity`, `TradeState`).
*   Reorder: Problem, Goals, Non goals, Anti goals, Constraints, Architecture (Numbers, Core, Pipeline, Bot loop, Engine, EVM adapter, Executor, Accounts, Holding limits, Cache, Speed, Reload, TUI, Config), Developer workflow, Testing (goals to tests table), Alternatives, Risks, Decision log, Open implementation choices.

### F15. Contradictions and terms  [C1, C2, C3, C12, C15, C18, C19, S6, T M6, T M10, T M11]  Severity: Medium
*   Fork test asserts the executor's WETH, not the owner's.
*   One refetch trigger for V4 ticks: price within one bitmap word of the window edge.
*   "Step" for a bot cycle; "tick" only for V4 price ticks.
*   "Executor balance" vs `treasury` address used distinctly.
*   `Opportunity.floor` (cost plus threshold, sent as `minProfit`) replaces the overloaded `min_profit` field.
*   Subscription list includes `ProtocolFeeUpdated` (now via getLogs).
*   `make fund` funds every account; `run-headless` added to the plan and listed after `run`.

### F16. Wording  [T M1, T M2, T M3, T M9, D N3, D N6]  Severity: Medium
*   "Exercises" instead of "proves"; speed goal stated as "minimise event to send latency, measured per trade and per step".
*   "Any EVM network" lists preconditions: V4 PoolManager and StateView, a wrapped native token set as `base_token` and passed to the executor, multicall.
*   "The UI cannot block the trading path."
*   `WS_URL` listed with the env vars; teardown names the profile files and matches only the exact line foundryup writes.

### F17. Plan fixes  [C13, C20, C21, Z17, Z18, S17]  Severity: Medium
*   Task 12 expected count matches its tests; two account overlap noted as covered by engine tests.
*   Drop `Outcome` (replaced by `TradeState`); `profit_curve` signature spelled out or removed with F3/F5 (UI recomputes curves from snapshots, per Z7).
*   Replace the 50 ms timing assertion with a structural check.
*   Task 16 README note fixed (first account is owner; treasury is an address); README says `make setup` is needed again after teardown.
*   Links to the dreygur style repo, the plan, and the external crates.

## Contested

### X1. Hyphenated compounds  [T N2, S10]  Severity: Low
*   ***AI Suggestion:*** CONTEST
*   ***Justification:*** The dreygur style guide (`/Users/rakib/Code/time_pass/dreygur-coding-style/skills/dreygur-coding-style/SKILL.md`, "No Dashes") bans hyphenated compounds in prose. Kept as is.

### X2. Committing `.claude/settings.json`  [Z18, C22]  Severity: Low
*   ***AI Suggestion:*** CONTEST
*   ***Justification:*** The same style guide ("Commits & Attribution") asks for `attribution.commit` and `attribution.pr` set to `""` in the repo's `.claude/settings.json`.

## Declined nits

Informal words (T N1, N3, N6, N7, N11, N13), code fence tags and wrap width (S11, S13, T N15), goal bullet grammar (S12), heading with code (S18), metadata block (S19), Artemis date (T N12). Cheap ones get fixed in passing with F14.
