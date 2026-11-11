//! The refork test: an `anvil_reset` erases the executor and the chain asks for a restart.
//!
//! It resets the fork under a connected chain, so it runs alone and after `fork.rs`
//! (`make test-fork`).

mod common;

use arbitrage::chain::Chain;

use common::*;

/// Updates pulled after the reset before giving up on a restart.
const UPDATES: usize = 10;

/// After the fork is reset under a connected chain and a block is mined, the next changes ask
/// for a restart. The reset goes to the pinned block rather than the latest one: it erases the
/// executor and rewinds the head just the same, while every read stays in anvil's cache
/// instead of on a public node that throttles by the end of a run. The connect applies one
/// block first, so the feed has a head to find the rewind against, and the first block after
/// the reset repeats that block's number and is a gap. The executor's code is checked before
/// any pool reload, so the restart comes without one.
#[tokio::test]
#[ignore = "needs the test fork: make test-fork; resets it"]
async fn refork_requests_restart() {
  setup();
  reset(&provider().await).await;
  let ctx = ctx(|_| {}).await;
  reset(&ctx.provider).await;
  for _ in 0..UPDATES {
    rpc(&ctx.provider, "evm_mine", ()).await;
    let update = tokio::time::timeout(WAIT, ctx.chain.changes())
      .await
      .expect("no block after the reset")
      .expect("changes");
    if update.restart {
      return;
    }
  }
  panic!("no restart within {UPDATES} updates of the reset");
}
