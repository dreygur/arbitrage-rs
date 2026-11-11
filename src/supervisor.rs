//! Keeps the bot running across config reloads and owns the way it stops.
//!
//! A reload from the screen's `r` or a `SIGHUP` rereads `.env` and the TOML. Live settings
//! go to the running chain in place; any other change, or a restart the chain asks for,
//! stops submitting, waits for trades in flight, drops the chain and builds a new one with
//! the builder `main` picked by `kind`. A quit waits for trades in flight the same way.

use std::future::Future;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::bot::{Bot, Decision, Exit};
use crate::chain::Chain;
use crate::config::{Change, NetworkConfig};
use crate::error::{Error, Result};
use crate::types::{Control, ReloadEvent, Status};
use crate::ui::UiCommand;

/// How often the chain's status is copied for the screen.
const STATUS_EVERY: Duration = Duration::from_millis(200);
/// How often a drain checks whether the trades in flight have settled.
const DRAIN_POLL: Duration = Duration::from_millis(200);

/// Waits until no trade is in flight or `timeout` blocks have passed, so neither a quit nor
/// a restart abandons a trade the engine could still settle. Copies the status for the
/// screen at each check, so its block keeps moving while the bot stands still.
async fn drain<C: Chain>(chain: &C, timeout: u64, status: &watch::Sender<Status>) {
  let start = chain.status().block;
  loop {
    let now = chain.status();
    let passed = now.block.saturating_sub(start);
    status.send_replace(now);
    if chain.in_flight() == 0 || passed >= timeout {
      return;
    }
    tokio::time::sleep(DRAIN_POLL).await;
  }
}

/// Returns at a `q` (or Ctrl+C) or when the screen goes away, the operator's way out of a
/// drain that waits on blocks that may never come. Other commands are ignored meanwhile.
async fn forced(commands: &mut mpsc::Receiver<UiCommand>) {
  while let Some(command) = commands.recv().await {
    if command == UiCommand::Quit {
      return;
    }
  }
}

/// The name of every setting that differs between `old` and `new`, for the screen. Env
/// vars show only as `env`, so neither their names nor their values, which may be keys,
/// leave the process.
fn changed(old: &NetworkConfig, new: &NetworkConfig, env: bool) -> Vec<String> {
  let fields = [
    ("min_profit", old.live.min_profit != new.live.min_profit),
    ("max_trade", old.live.max_trade != new.live.max_trade),
    (
      "max_gas_cost",
      old.live.max_gas_cost != new.live.max_gas_cost,
    ),
    (
      "trade_timeout",
      old.live.trade_timeout != new.live.trade_timeout,
    ),
    ("limits", old.live.limits != new.live.limits),
    ("kind", old.kind != new.kind),
    ("base_token", old.base_token != new.base_token),
    ("breakers", old.breakers != new.breakers),
    ("accounts", old.accounts != new.accounts),
    ("owner", old.owner != new.owner),
    ("treasury", old.treasury != new.treasury),
    ("journal", old.journal != new.journal),
    ("pairs", old.pairs != new.pairs),
    ("adapter", old.adapter != new.adapter),
    ("env", env),
  ];
  fields
    .into_iter()
    .filter(|(_, differs)| *differs)
    .map(|(name, _)| name.to_string())
    .collect()
}

/// Reads the config of `network` at `path`, returning it with the text it came from.
fn read_config(path: &Path, network: &str) -> Result<(NetworkConfig, String)> {
  let text = std::fs::read_to_string(path)?;
  let cfg = NetworkConfig::parse(&text, network)?;
  Ok((cfg, text))
}

/// Loads `env_file` over the process env; a missing file is no error. A parse error is
/// reported without its detail, since that quotes the line, which may hold a key.
fn reread_env(env_file: &Path) -> Result<()> {
  match dotenvy::from_path_override(env_file) {
    Ok(()) => Ok(()),
    Err(e) if e.not_found() => Ok(()),
    Err(_) => Err(Error::Env(format!(
      "{} cannot be parsed",
      env_file.display()
    ))),
  }
}

/// The channels a supervisor shares with the screen. They outlive every chain it builds,
/// so the screen never notices a restart.
pub struct Links {
  /// Where every bot reports its decisions.
  pub decisions: mpsc::Sender<Decision>,
  /// Commands from the screen; a closed channel means the screen is gone.
  pub commands: mpsc::Receiver<UiCommand>,
  /// Where reload outcomes go.
  pub reloads: mpsc::Sender<ReloadEvent>,
  /// The latest chain status, for the screen.
  pub status: watch::Sender<Status>,
}

/// A reread config that needs a restart, held until the restart applies it.
struct Pending {
  /// The new config.
  cfg: NetworkConfig,
  /// Its env fingerprint.
  fingerprint: u64,
  /// The outcome to report once the new chain runs.
  event: ReloadEvent,
}

/// Why one chain's run ended.
#[derive(Debug, PartialEq, Eq)]
enum Next {
  /// The bot is done.
  Stop,
  /// Build a new chain and keep going.
  Restart,
}

/// What woke the supervisor while a chain ran.
enum Wake {
  /// The bot's run ended on its own.
  Exited(Result<Exit>),
  /// Time to copy the status for the screen.
  Tick,
  /// A `SIGHUP` asked for a reload.
  Hangup,
  /// The screen sent a command, or went away.
  Command(Option<UiCommand>),
}

/// How a drain ended.
enum Drained {
  /// No trade is in flight, or the timeout passed.
  Settled,
  /// The operator quit, or the screen went away.
  Forced,
  /// The bot's run ended meanwhile.
  Exited(Result<Exit>),
}

/// Runs the bot over a chain built by `build`, rebuilding it when the config or the chain
/// needs a restart.
///
/// Generic over the chain so `main` alone names the concrete one; with a single `kind`
/// there is no need for trait objects.
pub struct Supervisor<C: Chain, F, Fut> {
  /// Network name, for parsing the config.
  network: String,
  /// The config file a reload rereads.
  path: PathBuf,
  /// The env file a reload rereads.
  env_file: PathBuf,
  /// Builds a chain from a config.
  build: F,
  /// The config the running chain was built from, live changes applied.
  cfg: NetworkConfig,
  /// Env fingerprint of `cfg`.
  fingerprint: u64,
  /// A reread config waiting for its restart.
  pending: Option<Pending>,
  /// The running chain.
  chain: Arc<C>,
  /// The bot over `chain`, shared with its run so a reload can change it meanwhile.
  bot: Arc<Bot<C>>,
  /// Whether the operator paused trading, so a restart keeps it paused.
  paused: bool,
  /// Channels to the screen.
  links: Links,
  /// Ties the builder's future type to the struct.
  future: PhantomData<fn() -> Fut>,
}

impl<C, F, Fut> Supervisor<C, F, Fut>
where
  C: Chain,
  F: Fn(NetworkConfig) -> Fut,
  Fut: Future<Output = Result<C>>,
{
  /// Reads the config at `path` and builds the first chain and its bot.
  pub async fn new(
    network: String,
    path: PathBuf,
    env_file: PathBuf,
    build: F,
    links: Links,
  ) -> Result<Self> {
    let (cfg, _) = read_config(&path, &network)?;
    let fingerprint = cfg.env_fingerprint();
    let chain = Arc::new(build(cfg.clone()).await?);
    let bot = Arc::new(Bot::new(
      chain.clone(),
      cfg.live.clone(),
      links.decisions.clone(),
    ));
    links.status.send_replace(chain.status());
    Ok(Self {
      network,
      path,
      env_file,
      build,
      cfg,
      fingerprint,
      pending: None,
      chain,
      bot,
      paused: false,
      links,
      future: PhantomData,
    })
  }

  /// Sends a reload outcome to the screen. A full or closed channel loses it, since it is
  /// also logged and a headless run has no screen to read it.
  fn report(&self, event: ReloadEvent) {
    if self.links.reloads.try_send(event).is_err() {
      debug!("no screen took the reload outcome");
    }
  }

  /// Rereads the env file and the config and applies what changed.
  ///
  /// A config that fails to load or validate keeps the running one and is reported with
  /// its error. A live change goes to the bot and the chain at once. Any other change is
  /// held for the restart that `run` carries out, which reports it once the new chain runs.
  pub fn reload(&mut self) -> Result<Change> {
    let loaded = reread_env(&self.env_file).and_then(|()| read_config(&self.path, &self.network));
    let (cfg, text) = match loaded {
      Ok(loaded) => loaded,
      Err(error) => {
        warn!(%error, "reload refused, keeping the running config");
        self.report(ReloadEvent {
          error: Some(error.to_string()),
          ..ReloadEvent::default()
        });
        return Err(error);
      }
    };
    let fingerprint = cfg.env_fingerprint();
    let change = self.cfg.diff(self.fingerprint, &cfg, fingerprint);
    let event = ReloadEvent {
      changes: changed(&self.cfg, &cfg, fingerprint != self.fingerprint),
      error: None,
      config: Some(text),
    };
    info!(?change, changes = ?event.changes, "config reread");
    match change {
      Change::None => self.report(event),
      Change::Live => {
        self.bot.apply_live(cfg.live.clone());
        self.cfg = cfg;
        self.report(event);
      }
      Change::Restart => {
        self.pending = Some(Pending {
          cfg,
          fingerprint,
          event,
        })
      }
    }
    Ok(change)
  }

  /// Decides what a run that ended on its own means: a restart the chain asked for rereads
  /// the config and rebuilds, without a drain, since the chain that asked can no longer
  /// settle anything; any other exit ends.
  fn exited(&mut self, exit: Result<Exit>) -> Result<Next> {
    match exit? {
      Exit::Restart => {
        warn!("the chain asked for a restart");
        // A refused config is reported by `reload` and the restart uses the running one.
        let _ = self.reload();
        Ok(Next::Restart)
      }
      Exit::Done => Ok(Next::Stop),
    }
  }

  /// Waits for trades in flight to settle while `run` keeps the bot stepping, since the
  /// chain only learns of new blocks through it. A `q` cuts the wait short when
  /// `forceable`; a closed command channel would end it at once, so it is not forceable.
  async fn wait<R>(&mut self, run: Pin<&mut R>, forceable: bool) -> Drained
  where
    R: Future<Output = Result<Exit>>,
  {
    let timeout = self.cfg.live.trade_timeout;
    tokio::select! {
      exit = run => Drained::Exited(exit),
      () = drain(&*self.chain, timeout, &self.links.status) => Drained::Settled,
      () = forced(&mut self.links.commands), if forceable => Drained::Forced,
    }
  }

  /// Pauses the bot and lets its trades settle before stopping. After a `q` the wait can be
  /// cut short by a second one; after the screen went away it cannot. A run that ends
  /// meanwhile is logged, and its error returned, but the quit still wins.
  async fn quit<R>(&mut self, run: Pin<&mut R>, asked: bool) -> Result<Next>
  where
    R: Future<Output = Result<Exit>>,
  {
    info!(
      in_flight = self.chain.in_flight(),
      "stopping, waiting for trades in flight"
    );
    self.bot.set_paused(true);
    match self.wait(run, asked).await {
      Drained::Settled => {}
      Drained::Forced => warn!(
        in_flight = self.chain.in_flight(),
        "quitting without waiting for trades in flight"
      ),
      Drained::Exited(exit) => info!(exit = ?exit?, "the bot ended while stopping"),
    }
    Ok(Next::Stop)
  }

  /// Pauses the bot and lets its trades settle before a restart. A `q` meanwhile turns the
  /// restart into a quit, which keeps waiting until a second `q`. A run that ends meanwhile
  /// decides: a restart goes ahead, any other exit ends.
  async fn drain_for_restart<R>(&mut self, mut run: Pin<&mut R>) -> Result<Next>
  where
    R: Future<Output = Result<Exit>>,
  {
    info!(
      in_flight = self.chain.in_flight(),
      "restarting, waiting for trades in flight"
    );
    self.bot.set_paused(true);
    match self.wait(run.as_mut(), true).await {
      Drained::Settled => Ok(Next::Restart),
      Drained::Forced => {
        let asked = !self.links.commands.is_closed();
        self.quit(run, asked).await
      }
      Drained::Exited(exit) => self.exited(exit),
    }
  }

  /// Serves one chain: carries out screen commands, reloads on `r` or `SIGHUP` and copies
  /// the status for the screen, until the bot must stop or restart.
  async fn serve<R>(&mut self, mut run: Pin<&mut R>, hangup: &mut Signal) -> Result<Next>
  where
    R: Future<Output = Result<Exit>>,
  {
    let mut tick = tokio::time::interval(STATUS_EVERY);
    loop {
      let wake = tokio::select! {
        exit = run.as_mut() => Wake::Exited(exit),
        _ = tick.tick() => Wake::Tick,
        Some(()) = hangup.recv() => Wake::Hangup,
        command = self.links.commands.recv() => Wake::Command(command),
      };
      match wake {
        Wake::Exited(exit) => return self.exited(exit),
        Wake::Tick => {
          self.links.status.send_replace(self.chain.status());
        }
        Wake::Hangup | Wake::Command(Some(UiCommand::Reload)) => {
          if let Ok(Change::Restart) = self.reload() {
            return self.drain_for_restart(run).await;
          }
        }
        Wake::Command(Some(UiCommand::Pause)) => {
          self.paused = true;
          self.bot.set_paused(true);
        }
        Wake::Command(Some(UiCommand::Resume)) => {
          self.paused = false;
          self.bot.set_paused(false);
        }
        Wake::Command(Some(UiCommand::Sweep)) => self.chain.control(Control::Sweep),
        Wake::Command(Some(UiCommand::Quit)) => return self.quit(run, true).await,
        Wake::Command(None) => return self.quit(run, false).await,
      }
    }
  }

  /// Drops the bot and the chain, then builds new ones from the pending config, or from
  /// the running one when the chain asked for the restart, and reports the pending reload.
  /// The new bot starts trading unless the operator had paused it.
  async fn restart(mut self) -> Result<Self> {
    let pending = self.pending.take();
    let (cfg, fingerprint) = match &pending {
      Some(pending) => (pending.cfg.clone(), pending.fingerprint),
      None => (self.cfg.clone(), self.fingerprint),
    };
    drop(self.bot);
    drop(self.chain);
    info!("rebuilding the chain");
    let chain = Arc::new((self.build)(cfg.clone()).await?);
    let bot = Arc::new(Bot::new(
      chain.clone(),
      cfg.live.clone(),
      self.links.decisions.clone(),
    ));
    if self.paused {
      bot.set_paused(true);
    }
    info!("restarted");
    let restarted = Self {
      cfg,
      fingerprint,
      chain,
      bot,
      ..self
    };
    if let Some(pending) = pending {
      restarted.report(pending.event);
    }
    Ok(restarted)
  }

  /// Runs the bot until a quit, the screen going away, or an exit that is not a restart,
  /// rebuilding the chain whenever a restart is due.
  pub async fn run(mut self) -> Result<()> {
    let mut hangup = signal(SignalKind::hangup())?;
    loop {
      let next = {
        let bot = self.bot.clone();
        let run = async move { bot.run(None).await };
        tokio::pin!(run);
        self.serve(run, &mut hangup).await?
      };
      match next {
        Next::Stop => return Ok(()),
        Next::Restart => self = self.restart().await?,
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::future::{Ready, ready};
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicUsize, Ordering};

  use super::*;
  use crate::testkit::MockChain;
  use crate::types::{Amount, Update};

  /// The shipped config the tests start from.
  const ETHEREUM: &str = include_str!("../config/ethereum.toml");
  /// Network name the tests parse with.
  const NETWORK: &str = "ethereum";
  /// Paused clock time long enough for every pending step to have run.
  const SETTLE: Duration = Duration::from_secs(5);
  /// Paused clock time between checks of a condition a test waits for.
  const TICK: Duration = Duration::from_millis(50);
  /// Env var a test names as its mnemonic, so changing it touches no other test.
  const TEST_ENV: &str = "ARB_SUPERVISOR_TEST_MNEMONIC";

  /// Distinguishes the file sets of tests run in one process.
  static SEQ: AtomicUsize = AtomicUsize::new(0);

  /// What a test keeps of the chains the supervisor builds.
  struct Shared {
    /// Chains built so far.
    builds: AtomicUsize,
    /// In flight count every built chain reports.
    in_flight: Arc<AtomicUsize>,
    /// Status every built chain reports.
    status: Arc<Mutex<Status>>,
    /// Whether the first chain's feed asks for a restart.
    restart_first: bool,
  }

  impl Shared {
    /// Chains built so far.
    fn builds(&self) -> usize {
      self.builds.load(Ordering::SeqCst)
    }
  }

  /// Fresh shared handles; the first chain asks for a restart when `restart_first` is set.
  fn mock_shared(restart_first: bool) -> Arc<Shared> {
    let chain = MockChain::new("base", Vec::new());
    Arc::new(Shared {
      builds: AtomicUsize::new(0),
      in_flight: chain.in_flight.clone(),
      status: chain.status.clone(),
      restart_first,
    })
  }

  /// A builder of mock chains that all report `shared`'s in flight count and status.
  fn mock_builder(shared: Arc<Shared>) -> impl Fn(NetworkConfig) -> Ready<Result<MockChain>> {
    move |_cfg| {
      let mut chain = MockChain::new("base", Vec::new());
      chain.in_flight = shared.in_flight.clone();
      chain.status = shared.status.clone();
      if shared.builds.fetch_add(1, Ordering::SeqCst) == 0 && shared.restart_first {
        chain.push_update(Ok(Update {
          restart: true,
          ..Default::default()
        }));
      }
      ready(Ok(chain))
    }
  }

  /// The config and env file a test's supervisor reads.
  struct Files {
    /// The config file.
    config: PathBuf,
    /// The env file, absent until a test writes it.
    env: PathBuf,
  }

  impl Files {
    /// Replaces the config with the shipped text after replacing `from` with `to`.
    fn edit(&self, from: &str, to: &str) {
      assert!(ETHEREUM.contains(from), "fixture lacks {from}");
      std::fs::write(&self.config, ETHEREUM.replace(from, to)).unwrap();
    }
  }

  /// A fresh directory holding `text` as the config, and no env file.
  fn mock_files(text: &str) -> Files {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir =
      std::env::temp_dir().join(format!("arbitrage-supervisor-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let files = Files {
      config: dir.join("config.toml"),
      env: dir.join(".env"),
    };
    std::fs::write(&files.config, text).unwrap();
    let _ = std::fs::remove_file(&files.env);
    files
  }

  /// The test's ends of a supervisor's channels.
  struct Ends {
    /// Sends screen commands.
    commands: mpsc::Sender<UiCommand>,
    /// Receives reload outcomes.
    reloads: mpsc::Receiver<ReloadEvent>,
    /// Keeps the decision channel open.
    _decisions: mpsc::Receiver<Decision>,
  }

  /// The supervisor type the tests build.
  type MockSupervisor<B> = Supervisor<MockChain, B, Ready<Result<MockChain>>>;

  /// A supervisor over mock chains sharing `shared`, reading `files`.
  async fn mock_supervisor(
    files: &Files,
    shared: Arc<Shared>,
  ) -> (
    MockSupervisor<impl Fn(NetworkConfig) -> Ready<Result<MockChain>>>,
    Ends,
  ) {
    let (decisions, decided) = mpsc::channel(64);
    let (commands_tx, commands) = mpsc::channel(8);
    let (reloads, reload_rx) = mpsc::channel(8);
    let (status, _) = watch::channel(shared.status.lock().unwrap().clone());
    let links = Links {
      decisions,
      commands,
      reloads,
      status,
    };
    let supervisor = Supervisor::new(
      NETWORK.into(),
      files.config.clone(),
      files.env.clone(),
      mock_builder(shared),
      links,
    )
    .await
    .unwrap();
    let ends = Ends {
      commands: commands_tx,
      reloads: reload_rx,
      _decisions: decided,
    };
    (supervisor, ends)
  }

  /// A config that fails to parse or validate keeps the running one and reports why.
  #[tokio::test]
  async fn invalid_config_keeps_old() {
    let files = mock_files(ETHEREUM);
    let shared = mock_shared(false);
    let (mut supervisor, mut ends) = mock_supervisor(&files, shared.clone()).await;
    files.edit("owner = 0", "owner = 9");
    assert!(supervisor.reload().is_err());
    let event = ends.reloads.try_recv().unwrap();
    assert!(
      event.error.as_deref().unwrap().contains("owner"),
      "{event:?}"
    );
    assert!(event.config.is_none());
    std::fs::write(&files.config, "kind = [").unwrap();
    assert!(supervisor.reload().is_err());
    assert!(ends.reloads.try_recv().unwrap().error.is_some());
    assert_eq!(supervisor.cfg.owner, 0);
    assert_eq!(shared.builds(), 1);
    assert!(supervisor.chain.lives.lock().unwrap().is_empty());
  }

  /// A new minimum profit reaches the chain in place, without a rebuild.
  #[tokio::test]
  async fn threshold_change_applies_live() {
    let files = mock_files(ETHEREUM);
    let shared = mock_shared(false);
    let (mut supervisor, mut ends) = mock_supervisor(&files, shared.clone()).await;
    files.edit(
      "min_profit = \"1000000000000000\"",
      "min_profit = \"2000000000000000\"",
    );
    assert_eq!(supervisor.reload().unwrap(), Change::Live);
    let want = Amount::from(2_000_000_000_000_000u64);
    assert_eq!(supervisor.chain.lives.lock().unwrap()[0].min_profit, want);
    assert_eq!(supervisor.cfg.live.min_profit, want);
    let event = ends.reloads.try_recv().unwrap();
    assert_eq!(event.changes, vec!["min_profit"]);
    assert!(event.config.unwrap().contains("2000000000000000"));
    assert_eq!(shared.builds(), 1);
  }

  /// A new gas cost cap reaches the chain in place, without a rebuild.
  #[tokio::test]
  async fn gas_cap_change_applies_live() {
    let files = mock_files(ETHEREUM);
    let shared = mock_shared(false);
    let (mut supervisor, mut ends) = mock_supervisor(&files, shared.clone()).await;
    files.edit(
      "max_gas_cost = \"20000000000000000\"",
      "max_gas_cost = \"30000000000000000\"",
    );
    assert_eq!(supervisor.reload().unwrap(), Change::Live);
    let want = Amount::from(30_000_000_000_000_000u64);
    assert_eq!(supervisor.chain.lives.lock().unwrap()[0].max_gas_cost, want);
    assert_eq!(
      ends.reloads.try_recv().unwrap().changes,
      vec!["max_gas_cost"]
    );
    assert_eq!(shared.builds(), 1);
  }

  /// A changed value of an env var the config names, read from the env file, rebuilds the
  /// chain, and the outcome is reported once the new one runs.
  #[tokio::test(start_paused = true)]
  async fn env_change_restarts() {
    let files = mock_files(&ETHEREUM.replace("MNEMONIC_1", TEST_ENV));
    let shared = mock_shared(false);
    let (supervisor, mut ends) = mock_supervisor(&files, shared.clone()).await;
    let driver = async {
      std::fs::write(&files.env, format!("{TEST_ENV}=changed\n")).unwrap();
      ends.commands.send(UiCommand::Reload).await.unwrap();
      let event = ends.reloads.recv().await.unwrap();
      assert_eq!(event.changes, vec!["env"]);
      assert_eq!(shared.builds(), 2);
      ends.commands.send(UiCommand::Quit).await.unwrap();
    };
    let (result, ()) = tokio::join!(supervisor.run(), driver);
    result.unwrap();
  }

  /// A restart waits for the trade in flight to settle before rebuilding.
  #[tokio::test(start_paused = true)]
  async fn restart_waits_for_in_flight() {
    let files = mock_files(ETHEREUM);
    let shared = mock_shared(false);
    shared.in_flight.store(1, Ordering::SeqCst);
    let (supervisor, mut ends) = mock_supervisor(&files, shared.clone()).await;
    let driver = async {
      files.edit("gas_v2_v2 = 350000", "gas_v2_v2 = 360000");
      ends.commands.send(UiCommand::Reload).await.unwrap();
      tokio::time::sleep(SETTLE).await;
      assert_eq!(shared.builds(), 1);
      shared.in_flight.store(0, Ordering::SeqCst);
      let event = ends.reloads.recv().await.unwrap();
      assert_eq!(event.changes, vec!["adapter"]);
      assert_eq!(shared.builds(), 2);
      ends.commands.send(UiCommand::Quit).await.unwrap();
    };
    let (result, ()) = tokio::join!(supervisor.run(), driver);
    result.unwrap();
  }

  /// A restart stops waiting once `trade_timeout` blocks pass with the trade still in
  /// flight; afterwards a second quit ends a drain that would never settle.
  #[tokio::test(start_paused = true)]
  async fn restart_waits_at_most_timeout() {
    let files = mock_files(ETHEREUM);
    let shared = mock_shared(false);
    shared.in_flight.store(1, Ordering::SeqCst);
    let (supervisor, mut ends) = mock_supervisor(&files, shared.clone()).await;
    let driver = async {
      files.edit("gas_v2_v2 = 350000", "gas_v2_v2 = 360000");
      ends.commands.send(UiCommand::Reload).await.unwrap();
      tokio::time::sleep(SETTLE).await;
      assert_eq!(shared.builds(), 1);
      shared.status.lock().unwrap().block = 3;
      ends.reloads.recv().await.unwrap();
      assert_eq!(shared.builds(), 2);
      ends.commands.send(UiCommand::Quit).await.unwrap();
      ends.commands.send(UiCommand::Quit).await.unwrap();
    };
    let (result, ()) = tokio::join!(supervisor.run(), driver);
    result.unwrap();
  }

  /// A restart the chain's feed asks for rereads the config, reported as unchanged, and
  /// rebuilds the chain once.
  #[tokio::test(start_paused = true)]
  async fn restart_update_rebuilds_chain() {
    let files = mock_files(ETHEREUM);
    let shared = mock_shared(true);
    let (supervisor, mut ends) = mock_supervisor(&files, shared.clone()).await;
    let driver = async {
      while shared.builds() < 2 {
        tokio::time::sleep(TICK).await;
      }
      tokio::time::sleep(SETTLE).await;
      assert_eq!(shared.builds(), 2);
      assert!(ends.reloads.try_recv().unwrap().changes.is_empty());
      assert!(ends.reloads.try_recv().is_err());
      ends.commands.send(UiCommand::Quit).await.unwrap();
    };
    let (result, ()) = tokio::join!(supervisor.run(), driver);
    result.unwrap();
  }

  /// A quit ends the wait for a forced exit.
  #[tokio::test(start_paused = true)]
  async fn forced_returns_on_quit() {
    let (tx, mut rx) = mpsc::channel(4);
    tx.send(UiCommand::Quit).await.unwrap();
    assert!(tokio::time::timeout(SETTLE, forced(&mut rx)).await.is_ok());
  }

  /// The screen going away ends the wait for a forced exit.
  #[tokio::test(start_paused = true)]
  async fn forced_returns_when_sender_dropped() {
    let (tx, mut rx) = mpsc::channel::<UiCommand>(4);
    drop(tx);
    assert!(tokio::time::timeout(SETTLE, forced(&mut rx)).await.is_ok());
  }

  /// A pause while draining does not force the exit.
  #[tokio::test(start_paused = true)]
  async fn forced_ignores_pause() {
    let (tx, mut rx) = mpsc::channel(4);
    tx.send(UiCommand::Pause).await.unwrap();
    assert!(tokio::time::timeout(SETTLE, forced(&mut rx)).await.is_err());
    drop(tx);
  }
}
