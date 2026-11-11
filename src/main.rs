//! Bot entry point: takes the instance lock, connects the configured network and runs the bot,
//! under the terminal screen unless `--headless` is given.

use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arbitrage::bot::{Bot, Decision, Exit};
use arbitrage::chain::Chain;
use arbitrage::chain::evm::EvmChain;
use arbitrage::config::{ChainKind, NetworkConfig};
use arbitrage::error::{Error, Result};
use arbitrage::types::{Control, ReloadEvent, Status, TradeEvent, Verdict};
use arbitrage::ui::app::App;
use arbitrage::ui::{UiCommand, UiLog, run_ui};
use tokio::sync::{mpsc, watch};
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
/// Log lines buffered for the screen before new ones are dropped.
const LOG_BUFFER: usize = 1024;
/// Reload outcomes buffered for the screen.
const RELOADS: usize = 16;
/// Screen commands buffered for the runner.
const COMMANDS: usize = 16;
/// How often the chain's status is copied for the screen.
const STATUS_EVERY: Duration = Duration::from_millis(200);
/// How often a quitting run checks whether its trades have settled.
const DRAIN_POLL: Duration = Duration::from_millis(200);
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

/// Logs how the bot's run ended.
fn report_exit(exit: Exit) {
  match exit {
    Exit::Restart => warn!("the chain asked for a restart, stopping"),
    Exit::Done => info!("done"),
  }
}

/// Copies the chain's status for the screen every [`STATUS_EVERY`] until the screen is gone.
async fn publish_status<C: Chain>(chain: Arc<C>, status: watch::Sender<Status>) {
  let mut tick = tokio::time::interval(STATUS_EVERY);
  loop {
    tick.tick().await;
    if status.send(chain.status()).is_err() {
      return;
    }
  }
}

/// Waits until no trade is in flight or `timeout` blocks have passed, so quitting never
/// abandons a trade the engine could still settle.
async fn drain<C: Chain>(chain: &C, timeout: u64) {
  let start = chain.status().block;
  while chain.in_flight() > 0 && chain.status().block.saturating_sub(start) < timeout {
    tokio::time::sleep(DRAIN_POLL).await;
  }
}

/// Carries out screen commands until `q`, or until the screen goes away, then pauses the bot
/// and lets its trades settle.
async fn serve<C: Chain>(
  commands: &mut mpsc::Receiver<UiCommand>,
  bot: &Bot<C>,
  chain: &C,
  timeout: u64,
) {
  loop {
    match commands.recv().await {
      Some(UiCommand::Pause) => bot.set_paused(true),
      Some(UiCommand::Resume) => bot.set_paused(false),
      Some(UiCommand::Sweep) => chain.control(Control::Sweep),
      Some(UiCommand::Reload) => warn!("reload arrives with the supervisor"),
      Some(UiCommand::Quit) | None => break,
    }
  }
  info!(
    in_flight = chain.in_flight(),
    "stopping, waiting for trades in flight"
  );
  bot.set_paused(true);
  drain(chain, timeout).await;
}

/// Runs the bot under the screen until `q` or the bot ends, then closes the screen.
async fn run_with_screen(
  network: &str,
  cfg: &NetworkConfig,
  chain: Arc<EvmChain>,
  trades: mpsc::Receiver<TradeEvent>,
  logs: mpsc::Receiver<String>,
) -> Result<()> {
  let config = std::fs::read_to_string(format!("config/{network}.toml"))?;
  let app = App::new(
    network.to_string(),
    config,
    cfg.live.limits.executor_max_hold,
  );
  let (decisions, decided) = mpsc::channel(DECISIONS);
  let bot = Bot::new(chain.clone(), cfg.live.clone(), decisions);
  let (status_tx, status_rx) = watch::channel(chain.status());
  tokio::spawn(publish_status(chain.clone(), status_tx));
  // Task 20's supervisor sends reload outcomes; the sender is held so the screen's
  // receiver stays open until then.
  let (_reloads, reload_rx) = mpsc::channel::<ReloadEvent>(RELOADS);
  let (commands_tx, mut commands) = mpsc::channel(COMMANDS);
  let screen = tokio::task::spawn_blocking(move || {
    run_ui(
      app,
      decided,
      trades,
      status_rx,
      reload_rx,
      logs,
      commands_tx,
    )
  });
  let ended = tokio::select! {
    exit = bot.run(None) => Some(exit),
    () = serve(&mut commands, &bot, &chain, cfg.live.trade_timeout) => None,
  };
  drop(commands);
  let shown = screen
    .await
    .map_err(|e| Error::Io(std::io::Error::other(e)));
  match ended {
    Some(exit) => report_exit(exit?),
    None => info!("stopped"),
  }
  shown?
}

/// Connects to `network` and runs the bot once until it ends: with logs only when `logs`
/// is absent, under the screen fed by `logs` otherwise.
async fn run(network: &str, logs: Option<mpsc::Receiver<String>>) -> Result<()> {
  let cfg = NetworkConfig::load(network)?;
  let (chain, trades) = match cfg.kind {
    ChainKind::Evm => EvmChain::connect(&cfg, network).await?,
  };
  let chain = Arc::new(chain);
  if let Some(logs) = logs {
    return run_with_screen(network, &cfg, chain, trades, logs).await;
  }
  tokio::spawn(log_trades(trades));
  let (decisions, decided) = mpsc::channel(DECISIONS);
  tokio::spawn(log_decisions(decided));
  let bot = Bot::new(chain, cfg.live.clone(), decisions);
  report_exit(bot.run(None).await?);
  Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
  dotenvy::dotenv().ok();
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
  let headless = std::env::args().any(|arg| arg == HEADLESS);
  let logs = if headless {
    tracing_subscriber::fmt()
      .with_env_filter(filter)
      .with_span_events(FmtSpan::CLOSE)
      .init();
    None
  } else {
    let (writer, lines) = UiLog::channel(LOG_BUFFER);
    tracing_subscriber::fmt()
      .with_env_filter(filter)
      .with_span_events(FmtSpan::CLOSE)
      .with_ansi(false)
      .with_writer(move || writer.clone())
      .init();
    Some(lines)
  };
  let network = std::env::var("NETWORK").unwrap_or_else(|_| DEFAULT_NETWORK.into());
  let _lock = take_lock(Path::new(&format!(".arbitrage-{network}.lock")))?;
  run(&network, logs).await?;
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
