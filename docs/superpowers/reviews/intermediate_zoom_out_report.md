# Stage 8: Intermediate Zoom Out Review

Status: resolved. All seven majors applied; nits 8, 10, 11 applied; nit 9
kept as author decisions (holding limits, E2).

Reviewer: egm-zoom-out (fresh context), with stage 5 to 7 reports.
Verdict: pass with Major findings, no blockers. Most stage 5 items are
resolved; several fixes added mechanism without updating the interfaces
they depend on.

## Major

1. **Timeout cancel cannot be sent under the gas cap.** Trades already
   use the configured cap as their fee cap, and a same nonce replacement
   must raise fee cap and tip by about 10%, so the cancel would exceed
   the cap it must obey and a stuck trade would hold its pools forever.
   Fix: send trades with fee cap `min(2 * next_base_fee + priority_fee,
   max_fee_per_gas)` so a replacement has room, or drop same nonce cancel
   on the static fork and wait for the base fee to fall.
2. **`Chain` trait does not fit the engine task.** Sync `submit` cannot
   await the engine's oneshot reply; sync `cost` cannot call a struct
   owned by another task; UI `Resume` and `SweepNow` and the supervisor's
   in flight wait have no path through `Chain`; Task 17's `RunningChain`
   and `BoxFuture` builder are new, unrecorded abstractions. Fix: async
   `submit`; engine publishes fee per gas and gas limits on a `watch` so
   `cost` stays sync; `control(&self, Control)` for pause, resume, sweep
   and in flight count; supervisor generic over `C: Chain` with a builder
   closure.
3. **`make refork` is not a restart.** A gap only reloads pool state;
   after `anvil_reset` the executor has no code, calls to it succeed and
   spend gas, trades are recorded `Executed`, and breakers never fire.
   Fix: `make refork` also restarts the bot, or the gap path checks
   executor code and requests a supervisor restart.
4. **UI asks for data the core types lack.** `Decision` has no
   snapshots and `Venue` has no label, price, fee or state. Fix: put the
   two `Arc<dyn Venue>` in `Decision`, add `Venue::describe() -> String`,
   cut the detail view to describe text, curve, send details and result.
5. **`TickWindow` skip duplicates the bounded tick guard.** Quotes near
   the edge are exact and quotes past it already error; skipping throws
   away valid trades, and no task produces the verdict. Fix: keep
   `near_edge` only as a refetch trigger through the channel `changes()`
   reads; drop the verdict and the skip.
6. **Bench decision uses the wrong workload.** Rayon and `release-max`
   are decided on constant product mocks before V4 exists. Fix: Task 5
   is the baseline only; rerun with V4 snapshots after Task 10 and decide
   there.
7. **Task 12 is too large.** Split into lanes, locks, state machine and
   fees; breakers; holdings on the owner lane. Separate files and
   commits, no new abstraction.

## Nit

8. Static fork leftovers: HTTP polling fallback, reorg nonce trigger,
   `ProtocolFeeUpdated` cannot trigger on a local anvil with WS.
9. Account forwarding and the whale fallback have almost no trigger
   (author decisions, noted only as risk).
10. Plan text: Task 1 "plus `GasCap`" is stale; Task 7 `fund_eth` is
    missing from the spec's `[fork]` keys; the fee cap check runs in both
    `cost` and `accept` (fine as a race guard, say so once).
11. Spec Constraints list the teardown profile files; that belongs in
    the plan only.

## Resolved since stage 5

Neutral config with `[adapter]`, `apply_live`, pull pipeline, per op
verdicts, executor persistence, unlock payload, direct WETH borrow,
mnemonic move, `rescan_blocks` dropped, `curve` out of `Opportunity`,
`Outcome` dropped, exact line teardown, fork test count.
