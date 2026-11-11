//! Fork tests of the whole arbitrage loop against the pinned anvil `make test-fork` starts.
//!
//! Each test starts from a reset to the pinned block, so tests never see each other's dumps,
//! trades or executors, while the remote state anvil fetched stays cached. Blocks come only
//! from transactions and `evm_mine`. All are ignored by plain `cargo test`;
//! `make test-fork` runs them one at a time.

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol_types::{SolCall, SolError};

use arbitrage::chain::Chain;
use arbitrage::chain::evm::bindings::{
  FlashArbitrage, IERC20, IStateView, IUniswapV2Router02, IV4Quoter, QuoteExactSingleParams,
};
use arbitrage::chain::evm::config::EvmConfig;
use arbitrage::chain::evm::convert::to_id;
use arbitrage::chain::evm::fund::fund;
use arbitrage::chain::evm::venue::v4::{V4Key, virtual_base_reserve};
use arbitrage::config::PairMode;
use arbitrage::journal::read_last;
use arbitrage::types::{Amount, EntryKind, Id, TradeState, Verdict};
use arbitrage::venue::Venue;

use common::*;

/// Hookless V4 fee (pips) and tick spacing pairs the adapter probes.
const PROBES: [(u32, i32); 4] = [(100, 1), (500, 10), (3000, 60), (10_000, 200)];
/// Tip of the dump that front runs a stale trade, well over the bot's.
const FRONT_RUN_TIP: u128 = 50_000_000_000;
/// Executor WETH the sweep test starts with, under the default `executor_max_hold`.
const SWEEP_SEED: U256 = U256::from_limbs([500_000_000_000_000_000, 0, 0, 0]);
/// USDC sold into a V4 pool to check the quote opposite to WETH in, 2,000 USDC.
const USDC_IN: u64 = 2_000_000_000;
/// Factory pairs discovery scans in its test, small enough to finish inside the public
/// node's state window.
const DISCOVERY_SCAN: u64 = 50;
/// Bytes in a V2 pool id (a pair address).
const V2_ID_LEN: usize = 20;
/// Bytes in a V4 pool id.
const V4_ID_LEN: usize = 32;

/// The snapshot venue of `pool`.
fn venue(ctx: &Ctx, pool: &Id) -> std::sync::Arc<dyn Venue> {
  ctx
    .chain
    .snapshots()
    .into_iter()
    .find(|venue| venue.pool() == pool)
    .unwrap_or_else(|| panic!("pool {pool} not tracked"))
}

/// Every hookless V4 key the adapter probes for `token` against native ETH and WETH.
fn v4_keys(token: Address) -> Vec<V4Key> {
  let mut keys = Vec::new();
  for (fee, tick_spacing) in PROBES {
    for base in [Address::ZERO, WETH] {
      let (currency0, currency1) = if base < token {
        (base, token)
      } else {
        (token, base)
      };
      keys.push(V4Key {
        currency0,
        currency1,
        fee,
        tick_spacing,
        hooks: Address::ZERO,
      });
    }
  }
  keys
}

/// The tracked V4 pool of WETH (or ETH) and `token` with the largest virtual WETH reserve.
async fn deepest_v4(ctx: &Ctx, token: Address) -> V4Key {
  let state_view = EvmConfig::parse(&ctx.cfg).unwrap().state_view;
  let view = IStateView::new(state_view, &ctx.provider);
  let tracked: HashSet<Id> = ctx
    .chain
    .snapshots()
    .iter()
    .map(|venue| venue.pool().clone())
    .collect();
  let mut best: Option<(Amount, V4Key)> = None;
  for key in v4_keys(token) {
    let id = key.pool_id();
    if !tracked.contains(&Id::new(id.to_vec())) {
      continue;
    }
    let slot0 = view.getSlot0(id).call().await.unwrap();
    let liquidity = view.getLiquidity(id).call().await.unwrap();
    let base_first = key.currency0 == Address::ZERO || key.currency0 == WETH;
    let reserve = virtual_base_reserve(liquidity, slot0.sqrtPriceX96, base_first).unwrap();
    if best.is_none_or(|(deepest, _)| reserve > deepest) {
      best = Some((reserve, key));
    }
  }
  best.expect("no tracked v4 pool").1
}

/// Whether a trade's two pools are one V2 pair and one V4 pool.
fn v4_against_v2(buy: &Id, sell: &Id) -> bool {
  let mut lengths = [buy.as_bytes().len(), sell.as_bytes().len()];
  lengths.sort_unstable();
  lengths == [V2_ID_LEN, V4_ID_LEN]
}

/// Funding sets every account's ETH, gives it each configured token, and drops the EIP 7702
/// delegations the accounts and the treasury carry on mainnet.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn fund_credits_eth_and_tokens() {
  forked(|| async {
    let cfg = config();
    let fork = EvmConfig::parse(&cfg).unwrap().fork.unwrap();
    let provider = provider().await;
    let accounts = accounts(&cfg);
    let treasury = treasury(&cfg);
    let funded = fund(&provider, &fork, treasury, &accounts).await.unwrap();
    assert_eq!(funded.len(), accounts.len());
    for (account, held) in accounts.iter().zip(&funded) {
      assert_eq!(held.address, *account);
      assert_eq!(held.eth, fork.fund_eth);
      assert_eq!(provider.get_balance(*account).await.unwrap(), fork.fund_eth);
      assert!(provider.get_code_at(*account).await.unwrap().is_empty());
      assert_eq!(held.tokens.len(), fork.tokens.len());
      for (token, (address, amount)) in fork.tokens.iter().zip(&held.tokens) {
        assert_eq!(*address, token.address);
        assert!(*amount >= token.amount, "{address}: {amount}");
      }
    }
    assert!(provider.get_code_at(treasury).await.unwrap().is_empty());
  })
  .await;
}

/// The local V2 quote of one WETH on SushiSwap's WETH/USDC pair equals the router's.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn v2_quote_matches_router() {
  forked(|| async {
    let ctx = ctx(|_| {}).await;
    let pair = sushi_pair(&ctx.provider, USDC).await;
    let venue = venue(&ctx, &to_id(pair));
    let (reserve_weth, reserve_usdc, _) = reserves(&ctx.provider, pair, WETH).await;
    let router = IUniswapV2Router02::new(ROUTER, &ctx.provider)
      .getAmountOut(ETHER, reserve_weth, reserve_usdc)
      .call()
      .await
      .unwrap();
    assert_eq!(venue.quote_out(ETHER, base_side(&*venue)).unwrap(), router);
  })
  .await;
}

/// On the deepest WETH/USDC V4 pool, which charges a protocol fee, the local quotes of one
/// WETH in and of [`USDC_IN`] in equal the V4Quoter's.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn v4_quote_matches_quoter() {
  forked(|| async {
    let ctx = ctx(|_| {}).await;
    let key = deepest_v4(&ctx, USDC).await;
    let state_view = EvmConfig::parse(&ctx.cfg).unwrap().state_view;
    let slot0 = IStateView::new(state_view, &ctx.provider)
      .getSlot0(key.pool_id())
      .call()
      .await
      .unwrap();
    assert!(!slot0.protocolFee.is_zero(), "pool charges no protocol fee");
    let venue = venue(&ctx, &Id::new(key.pool_id().to_vec()));
    let quoter = IV4Quoter::new(QUOTER, &ctx.provider);
    let base_first = key.currency0 == Address::ZERO || key.currency0 == WETH;
    let side = base_side(&*venue);
    for (amount, zero_for_one, side) in [
      (ETHER, base_first, side),
      (U256::from(USDC_IN), !base_first, side.flip()),
    ] {
      let params = QuoteExactSingleParams {
        poolKey: key.pool_key(),
        zeroForOne: zero_for_one,
        exactAmount: amount.to::<u128>(),
        hookData: Bytes::new(),
      };
      let quoted = quoter
        .quoteExactInputSingle(params)
        .call()
        .await
        .unwrap()
        .amountOut;
      assert_eq!(venue.quote_out(amount, side).unwrap(), quoted, "{side:?}");
    }
  })
  .await;
}

/// A dump on SushiSwap's WETH/USDC pair opens a V4 against V2 trade that executes and grows
/// the executor's WETH by at least its floor.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn arbitrages_v4_against_sushiswap() {
  forked(|| async {
    let mut ctx = ctx(|_| {}).await;
    let pair = sushi_pair(&ctx.provider, USDC).await;
    let executor = executor(&ctx);
    let before = weth_of(&ctx.provider, executor).await;
    let update = imbalance(&ctx, &[pair]).await;
    let ops = sent(&step(&ctx, &update).await);
    assert_eq!(ops.len(), 1, "{ops:?}");
    let op = &ops[0];
    assert!(v4_against_v2(&op.buy_pool, &op.sell_pool), "{op:?}");
    let done = outcome(&mut ctx, op).await;
    assert_eq!(done.state, TradeState::Executed, "{done:?}");
    let after = weth_of(&ctx.provider, executor).await;
    assert!(
      after - before >= op.floor,
      "gained {} under floor {}",
      after - before,
      op.floor
    );
  })
  .await;
}

/// A trade front run by the opposite dump at a higher tip reverts with `NoProfit`.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn stale_trade_reverts_no_profit() {
  forked(|| async {
    let mut ctx = ctx(|_| {}).await;
    let pair = sushi_pair(&ctx.provider, USDC).await;
    let amount = tenth_of_weth(&ctx.provider, pair).await;
    let (block, usdc) = dump(&ctx.provider, pair, amount).await;
    let update = catch_up(&ctx, block).await;
    rpc(&ctx.provider, "evm_setAutomine", (false,)).await;
    let ops = sent(&step(&ctx, &update).await);
    assert_eq!(ops.len(), 1, "{ops:?}");
    let op = ops[0].clone();
    let deadline = Instant::now() + WAIT;
    let trade = loop {
      drain(&mut ctx);
      let found = ctx
        .seen
        .iter()
        .find(|event| carries(event, &op) && event.state == TradeState::Sent)
        .cloned();
      if let Some(event) = found {
        break event;
      }
      assert!(Instant::now() < deadline, "trade never sent");
      tokio::time::sleep(POLL).await;
    };
    let trade_tx = trade.tx.expect("sent trade hash").parse().unwrap();
    let (front, _) = send_swap(&ctx.provider, pair, USDC, usdc, FRONT_RUN_TIP).await;
    rpc(&ctx.provider, "evm_mine", ()).await;
    let receipt = mined(&ctx.provider, trade_tx).await;
    assert!(!receipt.status(), "stale trade succeeded");
    for tx in front {
      let dumped = mined(&ctx.provider, tx).await;
      assert!(dumped.status());
      assert_eq!(dumped.block_number, receipt.block_number);
      assert!(dumped.transaction_index < receipt.transaction_index);
    }
    let data = revert_data(&ctx.provider, trade_tx).await;
    assert!(
      data.starts_with(&FlashArbitrage::NoProfit::SELECTOR),
      "revert data {data}"
    );
  })
  .await;
}

/// Dumps on two pairs give one step two trades, sent from two operators, both executed.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn two_pairs_from_two_accounts() {
  forked(|| async {
    let mut ctx = ctx(|_| {}).await;
    let pairs = [
      sushi_pair(&ctx.provider, USDC).await,
      sushi_pair(&ctx.provider, DAI).await,
    ];
    let update = imbalance(&ctx, &pairs).await;
    let ops = sent(&step(&ctx, &update).await);
    assert_eq!(ops.len(), 2, "{ops:?}");
    let first = outcome(&mut ctx, &ops[0]).await;
    let second = outcome(&mut ctx, &ops[1]).await;
    assert_eq!(first.state, TradeState::Executed, "{first:?}");
    assert_eq!(second.state, TradeState::Executed, "{second:?}");
    assert_ne!(first.account, second.account);
  })
  .await;
}

/// Right after a trade is accepted, stepping the same touched pools again finds its pools
/// locked.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn no_retrade_before_own_block_applied() {
  forked(|| async {
    let ctx = ctx(|_| {}).await;
    let pair = sushi_pair(&ctx.provider, USDC).await;
    let update = imbalance(&ctx, &[pair]).await;
    let ops = sent(&step(&ctx, &update).await);
    assert_eq!(ops.len(), 1, "{ops:?}");
    let again = step(&ctx, &update).await;
    assert!(sent(&again).is_empty(), "{again:?}");
    let locked = again.iter().any(|(op, verdict)| {
      op.buy_pool == ops[0].buy_pool
        && op.sell_pool == ops[0].sell_pool
        && matches!(verdict, Verdict::Locked(_))
    });
    assert!(locked, "{again:?}");
  })
  .await;
}

/// Without a dump, a step over every pair sends nothing.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn idle_without_imbalance() {
  forked(|| async {
    let ctx = ctx(|_| {}).await;
    let update = advance(&ctx).await;
    let results = ctx.bot.step(update.block, None).await;
    assert!(sent(&results).is_empty(), "{results:?}");
    assert_eq!(ctx.chain.in_flight(), 0);
  })
  .await;
}

/// A fee per gas over the cap from `max_fee_per_gas` turns an open arbitrage into `GasCap`
/// and nothing is sent.
///
/// anvil 1.9 on this fork ignores `anvil_setNextBlockBaseFeePerGas`, so the cap is lowered
/// under the fee instead: `max_fee_per_gas` equal to the 1 gwei tip leaves a fee cap of
/// 10/11 gwei, below base fee plus tip. The executor is deployed under the normal cap first
/// and reused by the capped chain, since setup transactions obey the same cap.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn fee_cap_stops_trading() {
  forked(|| async {
    drop(ctx(|_| {}).await);
    let mut ctx = ctx(|cfg| {
      let tip = cfg.adapter["priority_fee"].clone();
      cfg.adapter.insert("max_fee_per_gas".into(), tip);
    })
    .await;
    let tip = EvmConfig::parse(&ctx.cfg).unwrap().priority_fee;
    let pair = sushi_pair(&ctx.provider, USDC).await;
    let update = imbalance(&ctx, &[pair]).await;
    let status = ctx.chain.status();
    assert!(
      status.next_base_fee + tip > status.fee_cap,
      "fee per gas within the cap: {status:?}"
    );
    let results = step(&ctx, &update).await;
    assert!(!results.is_empty(), "the dumped pair was not evaluated");
    assert!(
      results
        .iter()
        .all(|(_, verdict)| *verdict == Verdict::GasCap),
      "{results:?}"
    );
    let decision = ctx.decisions.try_recv().expect("a decision");
    assert_eq!(decision.verdict, Verdict::GasCap);
    advance(&ctx).await;
    advance(&ctx).await;
    drain(&mut ctx);
    assert!(ctx.seen.is_empty(), "{:#?}", ctx.seen);
    assert_eq!(ctx.chain.in_flight(), 0);
  })
  .await;
}

/// With an executor cap of one wei, the profit of a trade is swept to the treasury in the
/// blocks after it.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn auto_sweep_above_cap() {
  forked(|| async {
    let mut ctx = ctx(|cfg| {
      cfg.live.limits.executor_max_hold = Amount::from(1u8);
      cfg.live.limits.executor_keep = Amount::ZERO;
      cfg.live.limits.min_transfer = Amount::from(1u8);
    })
    .await;
    let treasury = treasury(&ctx.cfg);
    let executor = executor(&ctx);
    let before = weth_of(&ctx.provider, treasury).await;
    let pair = sushi_pair(&ctx.provider, USDC).await;
    let update = imbalance(&ctx, &[pair]).await;
    let ops = sent(&step(&ctx, &update).await);
    assert_eq!(ops.len(), 1, "{ops:?}");
    let done = outcome(&mut ctx, &ops[0]).await;
    assert_eq!(done.state, TradeState::Executed, "{done:?}");
    let profit = done.detail.actual_profit.expect("actual profit");
    let sweep = await_event(&mut ctx, BLOCKS, |event| {
      event.kind == EntryKind::Sweep && is_final(event.state)
    })
    .await;
    assert_eq!(sweep.state, TradeState::Executed, "{sweep:?}");
    assert_eq!(weth_of(&ctx.provider, treasury).await - before, profit);
    assert_eq!(weth_of(&ctx.provider, executor).await, U256::ZERO);
  })
  .await;
}

/// An account that is not an operator cannot execute.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn rejects_non_operator() {
  forked(|| async {
    let ctx = ctx(|_| {}).await;
    rpc(&ctx.provider, "anvil_setBalance", (STRANGER, ETHER)).await;
    let leg = FlashArbitrage::Leg {
      family: 0,
      data: Bytes::new(),
    };
    let call = FlashArbitrage::executeCall {
      buy: leg.clone(),
      sell: leg,
      amount: ETHER,
      minProfit: U256::from(1u8),
    };
    let receipt = impersonated(&ctx.provider, STRANGER, executor(&ctx), call.abi_encode()).await;
    assert!(!receipt.status());
    let data = revert_data(&ctx.provider, receipt.transaction_hash).await;
    assert_eq!(
      data,
      Bytes::from(FlashArbitrage::NotOperator {}.abi_encode())
    );
  })
  .await;
}

/// An operator cannot sweep, and the owner's sweep pays the treasury.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn sweep_only_pays_treasury() {
  forked(|| async {
    let ctx = ctx(|_| {}).await;
    let accounts = accounts(&ctx.cfg);
    let (owner, operator) = (accounts[ctx.cfg.owner], accounts[1]);
    let treasury = treasury(&ctx.cfg);
    let executor = executor(&ctx);
    let deposit = wrap(&ctx.provider, SWEEP_SEED).await;
    let transfer = IERC20::transferCall {
      to: executor,
      value: SWEEP_SEED,
    };
    let seeded = send(
      &ctx.provider,
      DUMPER,
      WETH,
      U256::ZERO,
      transfer.abi_encode(),
      TIP,
    )
    .await;
    assert!(mined(&ctx.provider, deposit).await.status());
    assert!(mined(&ctx.provider, seeded).await.status());
    let before = weth_of(&ctx.provider, treasury).await;
    let sweep = FlashArbitrage::sweepCall {
      token: WETH,
      amount: SWEEP_SEED,
    };
    let refused = impersonated(&ctx.provider, operator, executor, sweep.abi_encode()).await;
    assert!(!refused.status());
    let data = revert_data(&ctx.provider, refused.transaction_hash).await;
    assert_eq!(data, Bytes::from(FlashArbitrage::NotOwner {}.abi_encode()));
    let paid = impersonated(&ctx.provider, owner, executor, sweep.abi_encode()).await;
    assert!(paid.status());
    assert_eq!(weth_of(&ctx.provider, treasury).await - before, SWEEP_SEED);
    assert_eq!(weth_of(&ctx.provider, executor).await, U256::ZERO);
  })
  .await;
}

/// Discovery over SushiSwap's first [`DISCOVERY_SCAN`] pairs connects and tracks pairs
/// holding WETH.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn discovers_pairs() {
  forked(|| async {
    let ctx = ctx(|cfg| {
      cfg.pairs.mode = PairMode::Discover;
      cfg.pairs.max_scan = DISCOVERY_SCAN;
    })
    .await;
    let base = to_id(WETH);
    let snapshots = ctx.chain.snapshots();
    assert!(
      snapshots
        .iter()
        .any(|venue| venue.tokens().contains(&&base)),
      "no pool holds WETH"
    );
  })
  .await;
}

/// A chain connected again after the first is dropped reuses the recorded executor without
/// deploying.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn executor_reused_after_restart() {
  forked(|| async {
    let first = ctx(|_| {}).await;
    let executor_before = executor(&first);
    let owner = accounts(&first.cfg)[first.cfg.owner];
    let provider = first.provider.clone();
    drop(first);
    let nonce = provider.get_transaction_count(owner).await.unwrap();
    let second = ctx(|_| {}).await;
    assert_eq!(executor(&second), executor_before);
    assert_eq!(provider.get_transaction_count(owner).await.unwrap(), nonce);
  })
  .await;
}

/// An executed trade leaves planned, sent and executed lines in the journal, the last with
/// its gas used and actual profit.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork"]
async fn journal_records_executed_trade() {
  forked(|| async {
    let mut ctx = ctx(|_| {}).await;
    let pair = sushi_pair(&ctx.provider, USDC).await;
    let update = imbalance(&ctx, &[pair]).await;
    let ops = sent(&step(&ctx, &update).await);
    assert_eq!(ops.len(), 1, "{ops:?}");
    let done = outcome(&mut ctx, &ops[0]).await;
    assert_eq!(done.state, TradeState::Executed, "{done:?}");
    let deadline = Instant::now() + WAIT;
    loop {
      let (entries, _) = read_last(&ctx.cfg.journal, usize::MAX).await.unwrap();
      let trade: Vec<_> = entries
        .iter()
        .filter(|entry| entry.event.id == done.id && entry.event.kind == EntryKind::Trade)
        .collect();
      let executed = trade
        .iter()
        .find(|entry| entry.event.state == TradeState::Executed);
      if let Some(executed) = executed {
        let has = |state| trade.iter().any(|entry| entry.event.state == state);
        assert!(has(TradeState::Planned), "{trade:?}");
        assert!(has(TradeState::Sent), "{trade:?}");
        assert!(executed.event.detail.gas_used.is_some(), "{executed:?}");
        assert!(
          executed.event.detail.actual_profit.is_some(),
          "{executed:?}"
        );
        break;
      }
      assert!(Instant::now() < deadline, "executed line never journaled");
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
  })
  .await;
}
