//! Screen state fed by the bot's channels and the keyboard.

use std::collections::{BTreeMap, VecDeque};

use ratatui::crossterm::event::KeyCode;

use super::UiCommand;
use crate::bot::Decision;
use crate::types::{Amount, EntryKind, Id, ReloadEvent, Status, TradeEvent, TradeState, Verdict};

/// Trade events kept for the trades panel and the detail view; older ones are only in the
/// journal.
pub const TRADE_EVENTS: usize = 500;
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
  /// Decisions of the latest step.
  pub decisions: Vec<Decision>,
  /// The latest decision of every pair seen, by buy and sell pool.
  pub pairs: BTreeMap<(Id, Id), Decision>,
  /// The last [`TRADE_EVENTS`] trade events, oldest first.
  pub trades: VecDeque<TradeEvent>,
  /// Latest chain status, absent until the first one arrives.
  pub status: Option<Status>,
  /// Running sums over settled trades.
  pub totals: Totals,
  /// The last [`LOG_LINES`] log lines, oldest first.
  pub log: VecDeque<String>,
  /// Outcome of the last reload.
  pub reload: Option<ReloadEvent>,
  /// Selected row among the filtered decisions.
  pub selected: usize,
  /// Page shown.
  pub tab: Tab,
  /// Which decisions are listed.
  pub filter: Filter,
  /// The decision pinned for the detail view, while it is open.
  pub detail: Option<Decision>,
  /// Whether the operator paused trading from this screen.
  pub paused: bool,
  /// Whether the operator asked to quit and the bot is draining.
  pub stopping: bool,
}

impl App {
  /// An empty screen for `network`, showing `config` on its config tab.
  pub fn new(network: String, config: String, executor_cap: Amount) -> Self {
    Self {
      network,
      config,
      executor_cap,
      decisions: Vec::new(),
      pairs: BTreeMap::new(),
      trades: VecDeque::new(),
      status: None,
      totals: Totals::default(),
      log: VecDeque::new(),
      reload: None,
      selected: 0,
      tab: Tab::Main,
      filter: Filter::All,
      detail: None,
      paused: false,
      stopping: false,
    }
  }

  /// The decisions the opportunities panel lists under the current filter.
  pub fn visible(&self) -> Vec<&Decision> {
    self
      .decisions
      .iter()
      .filter(|decision| self.filter.admits(decision.verdict))
      .collect()
  }

  /// Takes a decision; one from a newer block replaces the previous step's list.
  pub fn on_decision(&mut self, decision: Decision) {
    if self
      .decisions
      .first()
      .is_some_and(|first| first.block != decision.block)
    {
      self.decisions.clear();
    }
    let key = (decision.buy.pool().clone(), decision.sell.pool().clone());
    self.pairs.insert(key, decision.clone());
    self.decisions.push(decision);
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

  /// Takes a reload outcome and notes it in the log.
  pub fn on_reload(&mut self, reload: ReloadEvent) {
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
    let last = self.visible().len().saturating_sub(1);
    self.selected = if down {
      (self.selected + 1).min(last)
    } else {
      self.selected.saturating_sub(1).min(last)
    };
  }

  /// Opens the detail view on the selected decision, or closes it when open.
  fn toggle_detail(&mut self) {
    if self.detail.take().is_some() || self.tab != Tab::Main {
      return;
    }
    let visible = self.visible();
    let row = self.selected.min(visible.len().saturating_sub(1));
    self.detail = visible.get(row).map(|decision| (*decision).clone());
  }

  /// Applies a key to the screen and returns the command it asks of the bot, if any.
  pub fn on_key(&mut self, key: KeyCode) -> Option<UiCommand> {
    match key {
      KeyCode::Char('p') => {
        self.paused = !self.paused;
        Some(if self.paused {
          UiCommand::Pause
        } else {
          UiCommand::Resume
        })
      }
      KeyCode::Char('s') => Some(UiCommand::Sweep),
      KeyCode::Char('r') => Some(UiCommand::Reload),
      KeyCode::Char('q') => {
        self.stopping = true;
        Some(UiCommand::Quit)
      }
      KeyCode::Char('f') => {
        self.filter = self.filter.next();
        self.selected = 0;
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
