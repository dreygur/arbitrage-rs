//! Screen state fed by the bot's channels and the keyboard.

use std::collections::{BTreeMap, VecDeque};

use ratatui::crossterm::event::KeyCode;

use super::UiCommand;
use crate::bot::Decision;
use crate::types::{
  Amount, EntryKind, Id, Phase, ReloadEvent, Status, TradeEvent, TradeState, Verdict,
};

/// Trade events kept for the trades panel and the detail view; older ones are only in the
/// journal.
pub const TRADE_EVENTS: usize = 500;
/// Decisions kept for the opportunities panel across blocks, so a sent trade and the
/// numbers behind it stay open to the detail view after the next block.
pub const DECISIONS: usize = 500;
/// Log lines kept for the log panel.
pub const LOG_LINES: usize = 200;

/// Which page of the main screen is shown; `tab` cycles through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
  /// Opportunities, trades, accounts and the log.
  Main,
  /// Every pair seen, with its venues, spread and last update.
  Pairs,
  /// The config file, read only.
  Config,
}

impl Tab {
  /// The tab after this one, wrapping around.
  fn next(self) -> Self {
    match self {
      Self::Main => Self::Pairs,
      Self::Pairs => Self::Config,
      Self::Config => Self::Main,
    }
  }
}

/// Which decisions the opportunities panel lists; `f` cycles through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
  /// Every decision.
  All,
  /// Only trades that were sent.
  Sent,
  /// Only decisions that sent nothing.
  Skipped,
}

impl Filter {
  /// The filter after this one, wrapping around.
  fn next(self) -> Self {
    match self {
      Self::All => Self::Sent,
      Self::Sent => Self::Skipped,
      Self::Skipped => Self::All,
    }
  }

  /// Whether a decision with `verdict` passes this filter.
  fn admits(self, verdict: Verdict) -> bool {
    match self {
      Self::All => true,
      Self::Sent => verdict == Verdict::Sent,
      Self::Skipped => verdict != Verdict::Sent,
    }
  }
}

/// Running sums over the settled entries seen since the screen started.
///
/// Sums saturate rather than fail: they are display only, and wei sums cannot reach the top of
/// a 256 bit amount in practice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Totals {
  /// Base token gained by executed trades.
  pub gross: Amount,
  /// Native token spent on gas by mined trades and cancels.
  pub gas: Amount,
  /// Trades mined successfully.
  pub executed: u64,
  /// Trades mined and reverted.
  pub reverted: u64,
  /// Trades abandoned without being mined.
  pub dropped: u64,
}

impl Totals {
  /// Adds a settled entry; entries still on their way change nothing.
  fn add(&mut self, event: &TradeEvent) {
    let mined = matches!(event.state, TradeState::Executed | TradeState::Reverted);
    let costs_gas = matches!(event.kind, EntryKind::Trade | EntryKind::Cancel);
    if mined
      && costs_gas
      && let (Some(used), Some(price)) = (event.detail.gas_used, event.detail.effective_gas_price)
    {
      let spent = Amount::from(used).saturating_mul(Amount::from(price));
      self.gas = self.gas.saturating_add(spent);
    }
    if event.kind != EntryKind::Trade {
      return;
    }
    match event.state {
      TradeState::Executed => {
        self.executed += 1;
        let profit = event.detail.actual_profit.unwrap_or(Amount::ZERO);
        self.gross = self.gross.saturating_add(profit);
      }
      TradeState::Reverted => self.reverted += 1,
      TradeState::Dropped => self.dropped += 1,
      TradeState::Planned | TradeState::Sent => {}
    }
  }
}

/// Everything the screen shows, updated from the channels and the keys.
pub struct App {
  /// Network name for the header.
  pub network: String,
  /// The config file's text for the config tab.
  pub config: String,
  /// Most base token the executor may hold, shown against its balance.
  pub executor_cap: Amount,
  /// The last [`DECISIONS`] decisions, newest first, each with its arrival number, which
  /// the selection holds on to.
  pub decisions: VecDeque<(u64, Decision)>,
  /// Arrival number of the next decision.
  next_seq: u64,
  /// The latest decision of every pair seen, by buy and sell pool.
  pub pairs: BTreeMap<(Id, Id), Decision>,
  /// The last [`TRADE_EVENTS`] trade events, oldest first.
  pub trades: VecDeque<TradeEvent>,
  /// Latest chain status, absent until the first one arrives.
  pub status: Option<Status>,
  /// Whether a chain is being built or runs; the status means nothing while connecting.
  pub phase: Phase,
  /// Running sums over settled trades.
  pub totals: Totals,
  /// The last [`LOG_LINES`] log lines, oldest first.
  pub log: VecDeque<String>,
  /// Outcome of the last reload.
  pub reload: Option<ReloadEvent>,
  /// Arrival number of the selected decision, so the selection stays on it while newer
  /// ones push it down; `None`, or a decision no longer listed, selects the top row.
  pub selected: Option<u64>,
  /// Page shown.
  pub tab: Tab,
  /// Which decisions are listed.
  pub filter: Filter,
  /// The decision pinned for the detail view, while it is open.
  pub detail: Option<Decision>,
  /// Whether the operator paused trading from this screen.
  pub paused: bool,
  /// Whether the runner took a quit and the bot is draining.
  pub stopping: bool,
}

impl App {
  /// An empty screen for `network`, showing `config` on its config tab.
  pub fn new(network: String, config: String, executor_cap: Amount) -> Self {
    Self {
      network,
      config,
      executor_cap,
      decisions: VecDeque::new(),
      next_seq: 0,
      pairs: BTreeMap::new(),
      trades: VecDeque::new(),
      status: None,
      phase: Phase::Connecting,
      totals: Totals::default(),
      log: VecDeque::new(),
      reload: None,
      selected: None,
      tab: Tab::Main,
      filter: Filter::All,
      detail: None,
      paused: false,
      stopping: false,
    }
  }

  /// The decisions the opportunities panel lists under the current filter, newest first,
  /// each with its arrival number.
  pub fn visible(&self) -> Vec<(u64, &Decision)> {
    self
      .decisions
      .iter()
      .filter(|(_, decision)| self.filter.admits(decision.verdict))
      .map(|(seq, decision)| (*seq, decision))
      .collect()
  }

  /// The row of the selected decision among the listed ones; the top row when nothing is
  /// selected or the selected decision is no longer listed.
  pub fn selected_row(&self) -> usize {
    let Some(selected) = self.selected else {
      return 0;
    };
    self
      .visible()
      .iter()
      .position(|(seq, _)| *seq == selected)
      .unwrap_or(0)
  }

  /// Takes a decision at the top of the list, dropping the oldest past [`DECISIONS`].
  pub fn on_decision(&mut self, decision: Decision) {
    let key = (decision.buy.pool().clone(), decision.sell.pool().clone());
    self.pairs.insert(key, decision.clone());
    if self.decisions.len() == DECISIONS {
      self.decisions.pop_back();
    }
    self.decisions.push_front((self.next_seq, decision));
    self.next_seq += 1;
  }

  /// Takes a trade event, dropping the oldest past [`TRADE_EVENTS`].
  pub fn on_trade(&mut self, event: TradeEvent) {
    self.totals.add(&event);
    if self.trades.len() == TRADE_EVENTS {
      self.trades.pop_front();
    }
    self.trades.push_back(event);
  }

  /// Takes the latest chain status.
  pub fn on_status(&mut self, status: Status) {
    self.status = Some(status);
  }

  /// Takes a reload outcome, notes it in the log and shows an accepted config's text and
  /// executor cap.
  pub fn on_reload(&mut self, mut reload: ReloadEvent) {
    if let Some(config) = reload.config.take() {
      self.config = config;
    }
    if let Some(cap) = reload.executor_cap {
      self.executor_cap = cap;
    }
    let line = match &reload.error {
      Some(error) => format!("reload refused: {error}"),
      None if reload.changes.is_empty() => "reload: nothing changed".to_string(),
      None => format!("reload: {}", reload.changes.join(", ")),
    };
    self.on_log(line);
    self.reload = Some(reload);
  }

  /// Takes a log line, dropping the oldest past [`LOG_LINES`].
  pub fn on_log(&mut self, line: String) {
    if self.log.len() == LOG_LINES {
      self.log.pop_front();
    }
    self.log.push_back(line);
  }

  /// Moves the selection by one row, staying within the listed decisions.
  fn select(&mut self, down: bool) {
    let row = self.selected_row();
    let visible = self.visible();
    let last = visible.len().saturating_sub(1);
    let row = if down {
      (row + 1).min(last)
    } else {
      row.saturating_sub(1)
    };
    self.selected = visible.get(row).map(|(seq, _)| *seq);
  }

  /// Opens the detail view on the selected decision, or closes it when open.
  fn toggle_detail(&mut self) {
    if self.detail.take().is_some() || self.tab != Tab::Main {
      return;
    }
    let row = self.selected_row();
    self.detail = self
      .visible()
      .get(row)
      .map(|(_, decision)| (*decision).clone());
  }

  /// Records a command the runner accepted, so the run state shown is the one the bot has
  /// rather than the one a key asked for.
  pub fn delivered(&mut self, command: UiCommand) {
    match command {
      UiCommand::Pause => self.paused = true,
      UiCommand::Resume => self.paused = false,
      UiCommand::Quit => self.stopping = true,
      UiCommand::Sweep | UiCommand::Reload => {}
    }
  }

  /// Applies a key to the screen and returns the command it asks of the bot, if any. Run
  /// state changes wait for [`App::delivered`].
  pub fn on_key(&mut self, key: KeyCode) -> Option<UiCommand> {
    match key {
      KeyCode::Char('p') => Some(if self.paused {
        UiCommand::Resume
      } else {
        UiCommand::Pause
      }),
      KeyCode::Char('s') => Some(UiCommand::Sweep),
      KeyCode::Char('r') => Some(UiCommand::Reload),
      KeyCode::Char('q') => Some(UiCommand::Quit),
      KeyCode::Char('f') => {
        self.filter = self.filter.next();
        self.selected = None;
        None
      }
      KeyCode::Tab => {
        self.tab = self.tab.next();
        self.detail = None;
        None
      }
      KeyCode::Up => {
        self.select(false);
        None
      }
      KeyCode::Down => {
        self.select(true);
        None
      }
      KeyCode::Enter => {
        self.toggle_detail();
        None
      }
      KeyCode::Esc => {
        self.detail = None;
        None
      }
      _ => None,
    }
  }
}
