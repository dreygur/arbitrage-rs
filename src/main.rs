//! Bot entry point: takes the instance lock, connects the configured network and runs the bot.

use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::Arc;

use arbitrage::bot::{Bot, Decision, Exit};
use arbitrage::chain::evm::EvmChain;
use arbitrage::config::{ChainKind, NetworkConfig};
use arbitrage::error::{Error, Result};
use arbitrage::types::{TradeEvent, Verdict};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

/// Network run when `NETWORK` is unset.
const DEFAULT_NETWORK: &str = "ethereum";
/// Log filter used when `RUST_LOG` is unset; alloy's per request spans stay quiet, since
/// span close events are on for the bot's step span.
const DEFAULT_LOG: &str = "info,alloy_transport_http=warn";
/// Decisions buffered before the bot starts dropping them.
const DECISIONS: usize = 1024;
/// Flag that runs with logs only.
const HEADLESS: &str = "--headless";

/// Opens `path` and locks it for this process, failing with `Error::Lock` when another
/// instance holds it. The lock lasts as long as the returned file.
fn take_lock(path: &Path) -> Result<File> {
  let file = File::options()
    .create(true)
    .write(true)
    .truncate(false)
    .open(path)?;
  match file.try_lock() {
    Ok(()) => Ok(file),
    Err(TryLockError::WouldBlock) => Err(Error::Lock(format!(
      "{} is held by another instance",
      path.display()
    ))),
    Err(TryLockError::Error(e)) => Err(e.into()),
  }
}

/// Logs every decision: a sent trade at `info`, anything else at `debug`.
async fn log_decisions(mut decisions: mpsc::Receiver<Decision>) {
  while let Some(decision) = decisions.recv().await {
    let Some(op) = &decision.op else {
      debug!(block = decision.block, buy = %decision.buy.describe(), sell = %decision.sell.describe(), "quote failed");
      continue;
    };
    if decision.verdict == Verdict::Sent {
      info!(block = decision.block, token = %op.token, amount = %op.amount, profit = %op.profit, cost = %decision.cost, buy = %decision.buy.describe(), sell = %decision.sell.describe(), "trade sent");
    } else {
      debug!(block = decision.block, token = %op.token, profit = %op.profit, cost = %decision.cost, verdict = ?decision.verdict, "trade skipped");
    }
  }
}

/// Logs every trade event at `info`.
async fn log_trades(mut trades: mpsc::Receiver<TradeEvent>) {
  while let Some(event) = trades.recv().await {
    info!(id = event.id, kind = ?event.kind, state = ?event.state, block = event.block, account = %event.account, tx = ?event.tx, "trade event");
  }
}

/// Connects to `network` and runs the bot once until it ends.
async fn run(network: &str, headless: bool) -> Result<()> {
  let cfg = NetworkConfig::load(network)?;
  if !headless {
    warn!("the terminal UI is not built yet, logging only");
  }
  let chain = match cfg.kind {
    ChainKind::Evm => {
      let (chain, trades) = EvmChain::connect(&cfg, network).await?;
      tokio::spawn(log_trades(trades));
      chain
    }
  };
  let (decisions, decided) = mpsc::channel(DECISIONS);
  tokio::spawn(log_decisions(decided));
  let bot = Bot::new(Arc::new(chain), cfg.live.clone(), decisions);
  match bot.run(None).await? {
    Exit::Restart => warn!("the chain asked for a restart, stopping"),
    Exit::Done => info!("done"),
  }
  Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
  dotenvy::dotenv().ok();
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
  tracing_subscriber::fmt()
    .with_env_filter(filter)
    .with_span_events(FmtSpan::CLOSE)
    .init();
  let headless = std::env::args().any(|arg| arg == HEADLESS);
  let network = std::env::var("NETWORK").unwrap_or_else(|_| DEFAULT_NETWORK.into());
  let _lock = take_lock(Path::new(&format!(".arbitrage-{network}.lock")))?;
  run(&network, headless).await?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A second lock on the same file is refused until the first is released.
  #[test]
  fn second_instance_refused() {
    let dir = std::env::temp_dir().join(format!("arbitrage-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".arbitrage-test.lock");
    let first = take_lock(&path).unwrap();
    assert!(matches!(take_lock(&path), Err(Error::Lock(_))));
    drop(first);
    assert!(take_lock(&path).is_ok());
  }
}
