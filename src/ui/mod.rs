//! Terminal screen over the bot's decisions and trade events.
//!
//! The screen runs on its own blocking thread and only ever reads its channels with
//! `try_recv`, so a slow terminal can never hold up the trading path.

pub mod app;
pub mod detail;
pub mod views;

use std::io::{self, Write};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};

use self::app::App;
use self::detail::draw_detail;
use self::views::draw;
use crate::bot::Decision;
use crate::error::Result;
use crate::types::{Phase, ReloadEvent, Status, TradeEvent};

/// Time between frames, for ten frames a second; also the longest a key waits to be seen.
const FRAME: Duration = Duration::from_millis(100);

/// What the operator asks of the bot from the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiCommand {
  /// Stop opening new trades.
  Pause,
  /// Resume opening trades.
  Resume,
  /// Move profits to the treasury now.
  Sweep,
  /// Reread the config.
  Reload,
  /// Stop submitting, let trades in flight settle, then exit.
  Quit,
}

/// Tracing writer that sends each formatted line to the log panel, because writing to the
/// terminal would tear the screen. Lines are dropped while the panel is behind, and go to
/// stderr once the screen has closed so the last words of a run are not lost.
#[derive(Clone)]
pub struct UiLog(mpsc::Sender<String>);

impl UiLog {
  /// A writer and the receiver the screen reads its lines from, holding up to `capacity`.
  pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel(capacity);
    (Self(tx), rx)
  }
}

impl Write for UiLog {
  /// Sends one formatted event; tracing writes each event in a single call.
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    let line = String::from_utf8_lossy(buf).trim_end().to_string();
    match self.0.try_send(line) {
      Ok(()) | Err(TrySendError::Full(_)) => Ok(buf.len()),
      Err(TrySendError::Closed(_)) => io::stderr().write(buf),
    }
  }

  /// Nothing is buffered here.
  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

/// The channels the screen reads each frame.
pub struct Inputs {
  /// Decisions from the bot.
  pub decisions: mpsc::Receiver<Decision>,
  /// Trade events from the chain.
  pub events: mpsc::Receiver<TradeEvent>,
  /// The latest chain status.
  pub status: watch::Receiver<Status>,
  /// Whether a chain is being built or runs.
  pub phase: watch::Receiver<Phase>,
  /// Reload outcomes.
  pub reloads: mpsc::Receiver<ReloadEvent>,
  /// Log lines from [`UiLog`].
  pub logs: mpsc::Receiver<String>,
}

impl Inputs {
  /// Moves everything that arrived since the last frame into `app`.
  fn drain_into(&mut self, app: &mut App) {
    while let Ok(decision) = self.decisions.try_recv() {
      app.on_decision(decision);
    }
    while let Ok(event) = self.events.try_recv() {
      app.on_trade(event);
    }
    while let Ok(reload) = self.reloads.try_recv() {
      app.on_reload(reload);
    }
    while let Ok(line) = self.logs.try_recv() {
      app.on_log(line);
    }
    app.on_status(self.status.borrow_and_update().clone());
    app.phase = *self.phase.borrow_and_update();
  }
}

/// The key a key press means to the screen. Raw mode turns Ctrl+C into a plain key instead
/// of a signal, so it is read as `q` to keep it a way out.
fn key_code(key: &KeyEvent) -> KeyCode {
  if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
    KeyCode::Char('q')
  } else {
    key.code
  }
}

/// Draws and reads keys until the command receiver is dropped, which is how the runner says
/// it has finished.
fn event_loop(
  terminal: &mut DefaultTerminal,
  mut app: App,
  mut inputs: Inputs,
  commands: mpsc::Sender<UiCommand>,
) -> Result<()> {
  while !commands.is_closed() {
    inputs.drain_into(&mut app);
    terminal.draw(|frame| {
      if app.detail.is_some() {
        draw_detail(frame, &app);
      } else {
        draw(frame, &app);
      }
    })?;
    if event::poll(FRAME)?
      && let Event::Key(key) = event::read()?
      && key.kind == KeyEventKind::Press
      && let Some(command) = app.on_key(key_code(&key))
    {
      match commands.try_send(command) {
        Ok(()) => app.delivered(command),
        Err(_) => app.on_log(format!("{command:?} not delivered, the runner is busy")),
      }
    }
  }
  Ok(())
}

/// Runs the screen at ten frames a second until the runner drops `commands`.
///
/// The terminal is restored when the loop ends or fails, and by ratatui's panic hook on a
/// panic. That hook covers every thread in the process: a panic in another task restores
/// the terminal while this loop keeps drawing to the plain screen until `q`.
pub fn run_ui(app: App, inputs: Inputs, commands: mpsc::Sender<UiCommand>) -> Result<()> {
  let mut terminal = ratatui::try_init()?;
  let result = event_loop(&mut terminal, app, inputs, commands);
  ratatui::restore();
  result
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ratatui::Terminal;
  use ratatui::backend::TestBackend;

  use super::app::App;
  use super::detail::draw_detail;
  use super::views::{VERDICT_WIDTH, draw, format_amount};
  use super::*;
  use crate::bot::Decision;
  use crate::testkit::MockVenue;
  use crate::types::{
    Amount, EntryKind, Id, Opportunity, Side, Status, TradeDetail, TradeEvent, TradeState, Verdict,
  };
  use crate::venue::Venue;

  /// One ether in wei.
  const ETHER: u64 = 1_000_000_000_000_000_000;
  /// One gwei in wei.
  const GWEI: u128 = 1_000_000_000;

  /// Id from a name.
  fn id(name: &str) -> Id {
    Id::new(name.as_bytes().to_vec())
  }

  /// The buy pool: base is cheap here.
  fn buy_venue() -> MockVenue {
    MockVenue::new("a", "base", "other", 4 * ETHER, 10 * ETHER, 30)
  }

  /// The sell pool: base is dear here.
  fn sell_venue() -> MockVenue {
    MockVenue::new("b", "base", "other", 4 * ETHER, 8 * ETHER, 30)
  }

  /// A decision buying on pool a and selling on pool b at `amount` with the given verdict.
  fn decision(amount: u64, verdict: Verdict) -> Decision {
    let buy: Arc<dyn Venue> = Arc::new(buy_venue());
    let sell: Arc<dyn Venue> = Arc::new(sell_venue());
    Decision {
      block: 7,
      op: Some(Opportunity {
        buy_pool: id("a"),
        sell_pool: id("b"),
        token: id("other"),
        amount: Amount::from(amount),
        profit: Amount::from(ETHER / 100),
        floor: Amount::from(ETHER / 200),
      }),
      cost: Amount::from(ETHER / 1000),
      verdict,
      buy,
      sell,
    }
  }

  /// An app for the test network with a one ether executor cap.
  fn app() -> App {
    App::new(
      "testnet".into(),
      "kind = \"evm\"".into(),
      Amount::from(ETHER),
    )
  }

  /// A trade event for the a to b trade at `amount` in `state`.
  fn trade(amount: u64, state: TradeState, detail: TradeDetail) -> TradeEvent {
    TradeEvent {
      id: 42,
      kind: EntryKind::Trade,
      state,
      block: 8,
      account: "0x70997970C51812dc3A010C7d01b50e0d17dc79C8".into(),
      nonce: 3,
      tx: Some("0xabcdef0123456789".into()),
      detail: TradeDetail {
        buy_pool: Some(id("a")),
        sell_pool: Some(id("b")),
        pair: Some((id("base"), id("other"))),
        amount: Some(Amount::from(amount)),
        ..detail
      },
    }
  }

  /// Renders `paint` on a wide test terminal and returns its text, one line per row.
  fn screen(paint: impl FnOnce(&mut ratatui::Frame)) -> String {
    let mut terminal = Terminal::new(TestBackend::new(200, 60)).unwrap();
    terminal.draw(paint).unwrap();
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
      for x in 0..buffer.area.width {
        text.push_str(buffer[(x, y)].symbol());
      }
      text.push('\n');
    }
    text
  }

  /// The last column of an opportunities row, inside the panel border, trimmed.
  fn verdict_cell(row: &str) -> String {
    let inner: Vec<char> = row.trim_end().trim_end_matches('│').chars().collect();
    let start = inner.len().saturating_sub(usize::from(VERDICT_WIDTH));
    inner[start..].iter().collect::<String>().trim().to_string()
  }

  /// Every verdict reaches the opportunities panel under its own label, uncut.
  #[test]
  fn opportunities_show_verdicts() {
    let cases = [
      (Verdict::Sent, "SENT"),
      (Verdict::BelowMin, "skip: min"),
      (Verdict::GasExceedsProfit, "skip: gas"),
      (Verdict::GasCap, "skip: gas cap"),
      (Verdict::QuoteError, "skip: quote"),
      (Verdict::Locked(24_123_456), "locked #24123456"),
      (Verdict::LowGas, "skip: low gas"),
      (Verdict::CircuitOpen, "breaker"),
      (Verdict::Paused, "paused"),
    ];
    for (verdict, label) in cases {
      let mut app = app();
      app.on_decision(decision(ETHER / 10, verdict));
      let text = screen(|frame| draw(frame, &app));
      let row = text
        .lines()
        .find(|line| line.contains("mock a"))
        .unwrap_or_else(|| panic!("no opportunity row:\n{text}"));
      assert_eq!(verdict_cell(row), label, "{row}");
    }
  }

  /// The header shows the block, head age, next base fee and the fee cap against its ceiling.
  #[test]
  fn header_shows_fee_against_cap() {
    let mut app = app();
    app.phase = Phase::Running;
    app.on_status(Status {
      block: 123,
      next_base_fee: 12 * GWEI,
      fee_cap: 25 * GWEI,
      max_fee_per_gas: 50 * GWEI,
      head_age_ms: 40,
      executor: "0xexec".into(),
      executor_balance: Amount::ZERO,
      treasury: "0xtreasury".into(),
      treasury_balance: Amount::ZERO,
      accounts: Vec::new(),
    });
    let text = screen(|frame| draw(frame, &app));
    let header = text.lines().next().unwrap();
    for part in [
      "testnet",
      "block 123",
      "head age 40ms",
      "base 12.0000 gwei",
      "fee cap 25.0000 / 50.0000 gwei",
    ] {
      assert!(header.contains(part), "missing {part}: {header}");
    }
  }

  /// Before the first chain is built the header says the bot is connecting, not running,
  /// though the status channel already holds an empty status; a rebuild says so too.
  #[test]
  fn header_shows_connect_and_rebuild() {
    let mut app = app();
    app.on_status(Status::default());
    let text = screen(|frame| draw(frame, &app));
    let header = text.lines().next().unwrap();
    assert!(
      header.contains("connecting to testnet, loading pools"),
      "{header}"
    );
    assert!(!header.contains("running"), "{header}");
    assert!(!header.contains("block 0"), "{header}");
    app.phase = Phase::Rebuilding;
    let text = screen(|frame| draw(frame, &app));
    let header = text.lines().next().unwrap();
    assert!(header.contains("rebuilding, loading pools"), "{header}");
    assert!(!header.contains("running"), "{header}");
  }

  /// A trade row shows the quoted and the realised profit side by side.
  #[test]
  fn trade_row_shows_expected_and_actual() {
    let mut app = app();
    app.on_trade(trade(
      ETHER / 10,
      TradeState::Executed,
      TradeDetail {
        quoted_profit: Some(Amount::from(12_300_000_000_000_000u64)),
        actual_profit: Some(Amount::from(10_000_000_000_000_000u64)),
        gas_used: Some(210_000),
        effective_gas_price: Some(GWEI),
        ..Default::default()
      },
    ));
    let text = screen(|frame| draw(frame, &app));
    let row = text
      .lines()
      .find(|line| line.contains("Executed"))
      .unwrap_or_else(|| panic!("no Executed row:\n{text}"));
    assert!(row.contains("0.0123"), "{row}");
    assert!(row.contains("0.0100"), "{row}");
    assert!(row.contains("210000"), "{row}");
  }

  /// The detail view quotes both legs from the decision's snapshots and shows the revert.
  #[test]
  fn detail_shows_legs_and_revert() {
    let amount = ETHER / 10;
    let mut app = app();
    app.on_decision(decision(amount, Verdict::Sent));
    app.on_trade(trade(
      amount,
      TradeState::Reverted,
      TradeDetail {
        revert: Some("FloorNotMet".into()),
        gas_used: Some(90_000),
        ..Default::default()
      },
    ));
    assert_eq!(app.on_key(KeyCode::Enter), None);
    let bought = buy_venue()
      .quote_out(Amount::from(amount), Side::ZeroForOne)
      .unwrap();
    let back = sell_venue().quote_out(bought, Side::OneForZero).unwrap();
    let text = screen(|frame| draw_detail(frame, &app));
    for part in [
      "mock a".to_string(),
      "mock b".to_string(),
      bought.to_string(),
      format_amount(back, 18),
      "FloorNotMet".to_string(),
      "90000".to_string(),
    ] {
      assert!(text.contains(&part), "missing {part}:\n{text}");
    }
  }

  /// A decision with `verdict` quoted at `block`.
  fn decision_at(block: u64, verdict: Verdict) -> Decision {
    Decision {
      block,
      ..decision(ETHER / 10, verdict)
    }
  }

  /// Decisions of earlier blocks stay listed, newest first, so a sent trade can still be
  /// opened after the next block; the list keeps at most [`app::DECISIONS`].
  #[test]
  fn decisions_kept_across_blocks() {
    let mut app = app();
    app.on_decision(decision_at(7, Verdict::Sent));
    app.on_decision(decision_at(8, Verdict::BelowMin));
    app.on_decision(decision_at(9, Verdict::BelowMin));
    let blocks: Vec<u64> = app.visible().iter().map(|(_, d)| d.block).collect();
    assert_eq!(blocks, vec![9, 8, 7]);
    assert_eq!(app.on_key(KeyCode::Char('f')), None);
    let sent: Vec<u64> = app.visible().iter().map(|(_, d)| d.block).collect();
    assert_eq!(sent, vec![7]);
    let text = screen(|frame| draw(frame, &app));
    let row = text
      .lines()
      .find(|line| line.contains("mock a"))
      .unwrap_or_else(|| panic!("no opportunity row:\n{text}"));
    assert!(row.trim_start_matches('│').starts_with("7 "), "{row}");
    for block in 10..10 + app::DECISIONS as u64 {
      app.on_decision(decision_at(block, Verdict::BelowMin));
    }
    assert_eq!(app.decisions.len(), app::DECISIONS);
  }

  /// The selection stays on the decision the operator chose while newer ones arrive above
  /// it, and enter opens that decision.
  #[test]
  fn selection_follows_decision() {
    let mut app = app();
    app.on_decision(decision_at(7, Verdict::Sent));
    app.on_decision(decision_at(8, Verdict::BelowMin));
    assert_eq!(app.on_key(KeyCode::Down), None);
    assert_eq!(app.visible()[app.selected_row()].1.block, 7);
    app.on_decision(decision_at(9, Verdict::BelowMin));
    app.on_decision(decision_at(10, Verdict::BelowMin));
    assert_eq!(app.visible()[app.selected_row()].1.block, 7);
    assert_eq!(app.on_key(KeyCode::Enter), None);
    assert_eq!(app.detail.as_ref().unwrap().block, 7);
  }

  /// The command keys map to their commands and `p` alternates pause and resume.
  #[test]
  fn keys_map_to_commands() {
    let mut app = app();
    assert_eq!(app.on_key(KeyCode::Char('p')), Some(UiCommand::Pause));
    app.delivered(UiCommand::Pause);
    assert_eq!(app.on_key(KeyCode::Char('p')), Some(UiCommand::Resume));
    assert_eq!(app.on_key(KeyCode::Char('s')), Some(UiCommand::Sweep));
    assert_eq!(app.on_key(KeyCode::Char('r')), Some(UiCommand::Reload));
    assert_eq!(app.on_key(KeyCode::Char('f')), None);
    assert_eq!(app.on_key(KeyCode::Char('q')), Some(UiCommand::Quit));
  }

  /// The shown paused flag only changes once the runner has the command.
  #[test]
  fn undelivered_pause_keeps_running() {
    let mut app = app();
    assert_eq!(app.on_key(KeyCode::Char('p')), Some(UiCommand::Pause));
    assert!(!app.paused);
    assert_eq!(app.on_key(KeyCode::Char('p')), Some(UiCommand::Pause));
    app.delivered(UiCommand::Pause);
    assert!(app.paused);
  }

  /// Ctrl+C quits like `q`, since raw mode turns it into a plain key.
  #[test]
  fn ctrl_c_quits() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(key_code(&ctrl_c), KeyCode::Char('q'));
    let plain_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
    assert_eq!(key_code(&plain_c), KeyCode::Char('c'));
  }

  /// While draining the header offers a forced quit, and a second `q` still asks for one.
  #[test]
  fn second_quit_forces() {
    let mut app = app();
    assert_eq!(app.on_key(KeyCode::Char('q')), Some(UiCommand::Quit));
    app.delivered(UiCommand::Quit);
    let text = screen(|frame| draw(frame, &app));
    let header = text.lines().next().unwrap();
    assert!(header.contains("stopping, q again to force"), "{header}");
    assert_eq!(app.on_key(KeyCode::Char('q')), Some(UiCommand::Quit));
  }

  /// An accepted reload replaces the config tab's text and the executor cap, and a refused
  /// one keeps both.
  #[test]
  fn reload_replaces_config_text() {
    let mut app = app();
    app.on_reload(ReloadEvent {
      error: Some("config: owner: index is outside the accounts list".into()),
      ..Default::default()
    });
    assert_eq!(app.config, "kind = \"evm\"");
    assert_eq!(app.executor_cap, Amount::from(ETHER));
    app.on_reload(ReloadEvent {
      changes: vec!["min_profit".into()],
      config: Some("kind = \"evm\"\nmin_profit = \"1\"".into()),
      executor_cap: Some(Amount::from(2 * ETHER)),
      ..Default::default()
    });
    assert_eq!(app.config, "kind = \"evm\"\nmin_profit = \"1\"");
    assert_eq!(app.executor_cap, Amount::from(2 * ETHER));
    assert_eq!(app.log.back().unwrap(), "reload: min_profit");
  }

  /// Wei print in token units with four places.
  #[test]
  fn amount_formatting() {
    assert_eq!(
      format_amount(Amount::from(1_234_500_000_000_000_000u64), 18),
      "1.2345"
    );
    assert_eq!(format_amount(Amount::ZERO, 18), "0.0000");
    assert_eq!(format_amount(Amount::from(25u64), 1), "2.5000");
  }
}
