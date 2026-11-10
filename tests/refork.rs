//! The refork test: an `anvil_reset` erases the executor and the chain asks for a restart.
//!
//! The reset drops every snapshot, so it runs alone and after `fork.rs` (`make test-fork`).

mod common;

use std::time::Duration;

use serde_json::json;

use arbitrage::chain::Chain;

use common::*;

/// Updates pulled after the reset before giving up on a restart.
const UPDATES: usize = 10;
/// Longest wait for one update after the reset. The reset is a feed gap, so the chain
/// rereads every pool, V4 tick windows included, against the new fork before it checks the
/// executor's code; through a public node that takes minutes, not a block time.
const RELOAD_WAIT: Duration = Duration::from_secs(20 * 60);

/// After the fork is reset to the latest mainnet block, the next changes ask for a restart.
#[tokio::test]
#[ignore = "needs a running fork: make fork; resets it"]
async fn refork_requests_restart() {
  let ctx = ctx(|_| {}).await;
  catch_up(&ctx, 0).await;
  let fork_url = std::env::var("FORK_URL").expect("FORK_URL");
  rpc(
    &ctx.provider,
    "anvil_reset",
    (json!({ "forking": { "jsonRpcUrl": fork_url } }),),
  )
  .await;
  for _ in 0..UPDATES {
    let update = tokio::time::timeout(RELOAD_WAIT, ctx.chain.changes())
      .await
      .expect("no block after the reset")
      .expect("changes");
    if update.restart {
      return;
    }
  }
  panic!("no restart within {UPDATES} updates of the reset");
}
