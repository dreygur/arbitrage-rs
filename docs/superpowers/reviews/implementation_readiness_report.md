# Stage 9: Implementation Readiness

Status: resolved. Reviewers: egm-implementation-readiness, egm-codebase-details, egm-style (fresh context). Initial verdict NOT READY (3 blockers, about 20 majors). All blockers and majors are addressed in the rewritten plan; resolutions below.

## Blockers

- **B1 task order:** breakers (Task 12), holdings (13) and journal (14) now come before the engine (15); signatures match.
- **B2 UI wiring:** `UiCommand`, `Status` (published by the engine, read through `Chain::status`), `ReloadEvent` receiver, and `main.rs` spawning `run_ui` are defined in Tasks 1, 15, 19, 20.
- **B3 engine calldata:** `LegSpec` map from `pairs::leg_specs`, `build_legs`, `EngineCmd`, provider type parameter, and the `Block` path from `changes()` to the engine are defined in Tasks 11, 15, 16.

## Majors (resolution)

- Teardown files listed explicitly, fish included (Task 2).
- `make setup` recipe and Foundry version stated; the installer is a user command, not a tool call.
- Accounts: anvil mnemonic in `.env.example`, indices `[0, 1, 2]`, no `PRIVATE_KEY` account; README mnemonic kept as a commented `MNEMONIC_2`; `cp .env.example .env` in Task 16; unquoted values.
- `.env` was tracked by git: Task 1 runs `git rm --cached .env`.
- Gas numbers fixed: 20% headroom, 1.5x on out of gas, fee cap `min(2*base+tip, max_fee_per_gas*10/11)` so a bumped cancel fits.
- `Evaluation::Failed` carries the venue snapshots and has a definition.
- `BelowMin` vs `GasExceedsProfit` rule stated; `Decision.cost` is 0 for `GasCap`.
- Bot ownership (`Arc<C>`, atomics, `RwLock`), `Exit { Restart, Done }`, `step(0, None)` fixed.
- `Chain::base()` and `Chain::status()` added (spec updated).
- Instance lock moved to `main` with `std::fs::File::try_lock`; `fs4` dropped.
- Executor record schema and the pure `executor_action` decision defined; `owner`, `treasury` public immutables.
- `TradeEvent`, `TradeDetail` moved to core `types.rs` with explicit fields.
- Feed returns a `FeedHandle`; window fetch lives in `ticks.rs`, shared by state and feed.
- Discovery steps written out.
- Venue names in `V2Pool`, `V4Pool`, `Tracked`; `describe` formats fixed.
- V4: own `V4Key`; decimals read at load; protocol fee pools skipped; SDK quote polled with `now_or_never()` (verified: SDK `TickDataProvider` methods are async in uniswap-v3-sdk 7.0.0).
- Engine test seam: pure functions plus an `Asserter` backed `mock_provider`.
- Router and Quoter addresses added as test constants; Multicall3 address added.
- Fork test harness (`tests/common`), per scenario setup, refork in its own test file.
- Config serde: decimal string deserializer, flattened breakers, fork token array with amounts.
- Bench outcome edits listed; shared doubles in `src/testkit.rs` behind the `testkit` feature.
- Supervisor: `dotenv_override`, block based timeout from `status().block`, `MockChain` in testkit.
- Contract tested in its own task with forge tests; leg libraries `internal`; swap limits and delta based amounts stated; `gained` underflow guarded.
- `Error::Env(String)`, `Io`, `Toml`, `Json` with `#[from]`, `Lock`.
- alloy features pinned (`full`, `json`, `signer-mnemonic`); ratatui 0.30 with its crossterm re-export.
- Each task lists `src/lib.rs` changes; Global Constraint added.

## Style findings

Applied: Review Focus item 2 states a requirement; "lane" defined in Task 15; the engine bullet split; headroom value; "smallest" factory defined (lower `allPairsLength`); Task 16 and 19 interfaces as bullets; doc comment and abstraction rules reworded as concrete rules; bench imbalance range and median statistic; "too shallow" tied to `min_base_reserve`; README "leave all other sections unchanged"; "all pass".

Declined: "(recommended)" in the skill header (fixed template text).
