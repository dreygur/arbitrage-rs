//! Bot entry point: takes the instance lock and supervises the bot on the configured
//! network, under the terminal screen unless `--headless` is given. The lock is held for
//! the whole process, so it holds across every restart the supervisor makes.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arbitrage::bot::Decision;
use arbitrage::chain::evm::EvmChain;
use arbitrage::config::{ChainKind, NetworkConfig};
use arbitrage::error::{Error, Result};
use arbitrage::redact::{self, Redacting};
use arbitrage::supervisor::{Links, Supervisor};
use arbitrage::types::{Phase, Status, TradeEvent, Verdict};
use arbitrage::ui::app::App;
use arbitrage::ui::{Inputs, UiLog, run_ui};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
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
/// Trade events buffered for the screen.
const TRADES: usize = 1024;
/// The env file a reload rereads, relative to the working directory.
const ENV_FILE: &str = ".env";
/// Longest a restart waits for the old engine to finish its queued commands.
const ENGINE_STOP: Duration = Duration::from_secs(30);
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

/// Passes one chain's trade events on to the screen's channel, which outlives the chain, so
/// the screen keeps its feed across restarts. Ends when the chain's engine stops.
async fn forward_trades(mut trades: mpsc::Receiver<TradeEvent>, screen: mpsc::Sender<TradeEvent>) {
  while let Some(event) = trades.recv().await {
    if screen.send(event).await.is_err() {
      return;
    }
  }
}

/// Waits for the engine of the chain the supervisor just dropped, so its queued commands
/// and their ledger ids are done before the next connect reads the journal. A wait past
/// [`ENGINE_STOP`] is logged and given up, so a stuck engine cannot block the restart.
async fn stop_engine(engine: &Mutex<Option<JoinHandle<()>>>) {
  let task = engine.lock().unwrap_or_else(|e| e.into_inner()).take();
  if let Some(task) = task
    && tokio::time::timeout(ENGINE_STOP, task).await.is_err()
  {
    warn!("the old engine did not stop in time, connecting anyway");
  }
}

/// Builds the first chain of `cfg`'s kind and runs the supervisor over it until it stops,
/// or until the operator quits during that first connect. Trade events go to
/// `screen_trades` under the screen, and to the log otherwise.
async fn supervise(
  network: &str,
  path: PathBuf,
  cfg: NetworkConfig,
  hangup: Signal,
  links: Links,
  screen_trades: Option<mpsc::Sender<TradeEvent>>,
) -> Result<()> {
  match cfg.kind {
    ChainKind::Evm => {
      let name = network.to_string();
      let engine = Arc::new(Mutex::new(None));
      let build = move |cfg: NetworkConfig| {
        let name = name.clone();
        let screen_trades = screen_trades.clone();
        let engine = engine.clone();
        async move {
          stop_engine(&engine).await;
          let (chain, events) = EvmChain::connect(&cfg, &name).await?;
          *engine.lock().unwrap_or_else(|e| e.into_inner()) = chain.take_engine();
          match screen_trades {
            Some(screen) => tokio::spawn(forward_trades(events, screen)),
            None => tokio::spawn(log_trades(events)),
          };
          Ok(chain)
        }
      };
      let env_file = PathBuf::from(ENV_FILE);
      let supervisor =
        Supervisor::new(network.to_string(), path, env_file, hangup, build, links).await?;
      match supervisor {
        Some(supervisor) => supervisor.run().await,
        None => Ok(()),
      }
    }
  }
}

/// Supervises the bot on `network` until it stops: with logs only when `logs` is absent,
/// under the screen fed by `logs` otherwise.
///
/// The screen opens before the first connect, which can take minutes, so the operator sees
/// it progress in the log panel and can quit it. It closes once the supervisor is done,
/// however that ends, since dropping the command receiver is what tells it to.
async fn run(network: &str, logs: Option<mpsc::Receiver<String>>) -> Result<()> {
  let hangup = signal(SignalKind::hangup())?;
  let path = PathBuf::from(format!("config/{network}.toml"));
  let cfg = NetworkConfig::load(network)?;
  let (decisions, decided) = mpsc::channel(DECISIONS);
  let (reloads, reload_rx) = mpsc::channel(RELOADS);
  let (commands_tx, commands) = mpsc::channel(COMMANDS);
  let (status, status_rx) = watch::channel(Status::default());
  let (phase, phase_rx) = watch::channel(Phase::Connecting);
  let (trades, trade_rx) = mpsc::channel(TRADES);
  let screen_trades = logs.is_some().then_some(trades);
  let links = Links {
    decisions,
    commands,
    reloads,
    status,
    phase,
  };
  // Held until the run ends in a headless run, since a closed command channel reads as
  // the screen leaving.
  let mut headless_commands = None;
  let screen = match logs {
    Some(logs) => {
      let app = App::new(
        network.to_string(),
        std::fs::read_to_string(&path)?,
        cfg.live.limits.executor_max_hold,
      );
      let inputs = Inputs {
        decisions: decided,
        events: trade_rx,
        status: status_rx,
        phase: phase_rx,
        reloads: reload_rx,
        logs,
      };
      Some(tokio::task::spawn_blocking(move || {
        run_ui(app, inputs, commands_tx)
      }))
    }
    None => {
      tokio::spawn(log_decisions(decided));
      headless_commands = Some(commands_tx);
      None
    }
  };
  let supervised = supervise(network, path, cfg, hangup, links, screen_trades).await;
  drop(headless_commands);
  info!("stopped");
  let Some(screen) = screen else {
    return supervised;
  };
  let shown = screen
    .await
    .map_err(|e| Error::Io(std::io::Error::other(e)))
    .and_then(|shown| shown);
  supervised.and(shown)
}

/// Loads `.env`, registers the node URLs for redaction, routes logs to the screen or, with
/// `--headless`, to stderr, takes the instance lock and runs the bot. A failure is redacted
/// before eyre prints it.
#[tokio::main]
async fn main() -> eyre::Result<()> {
  dotenvy::dotenv().ok();
  redact::register_env();
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
  let headless = std::env::args().any(|arg| arg == HEADLESS);
  let logs = if headless {
    tracing_subscriber::fmt()
      .with_env_filter(filter)
      .with_span_events(FmtSpan::CLOSE)
      .with_writer(|| Redacting(std::io::stderr()))
      .init();
    None
  } else {
    let (writer, lines) = UiLog::channel(LOG_BUFFER);
    tracing_subscriber::fmt()
      .with_env_filter(filter)
      .with_span_events(FmtSpan::CLOSE)
      .with_ansi(false)
      .with_writer(move || Redacting(writer.clone()))
      .init();
    Some(lines)
  };
  let network = std::env::var("NETWORK").unwrap_or_else(|_| DEFAULT_NETWORK.into());
  let _lock = take_lock(Path::new(&format!(".arbitrage-{network}.lock")))?;
  run(&network, logs)
    .await
    .map_err(|e| eyre::eyre!(redact::redact(&e.to_string())))?;
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
