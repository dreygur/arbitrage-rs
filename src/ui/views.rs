//! The main screen: header, totals, holdings and the panels of each tab.

use std::collections::HashSet;
use std::fmt::Display;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Paragraph, Row, Table, TableState};

use super::app::{App, Tab};
use crate::bot::Decision;
use crate::types::{Amount, Phase, Role, Verdict};

/// Decimals of the base token and of the native token, both 18 on every chain run today.
pub const BASE_DECIMALS: u8 = 18;
/// Decimals that turn wei into gwei.
pub const GWEI_DECIMALS: u8 = 9;
/// Places shown after the decimal point.
const PLACES: u8 = 4;
/// Hex digits kept at each end of a shortened address or hash.
const SHORT_KEEP: usize = 6;
/// Basis points in one whole, for the spread on the pairs tab.
const BPS: u64 = 10_000;
/// Width of the verdict column: `locked #` and an eight digit trade id.
pub const VERDICT_WIDTH: u16 = 16;
/// Words of a pool's `describe` text a table shows: the venue name and fee tier.
const VENUE_WORDS: usize = 2;
/// Width of the block column: a nine digit block number.
const BLOCK_WIDTH: u16 = 9;
/// The key help shown on the last row.
const KEYS: &str =
  "tab page  up/down select  enter detail  p pause/resume  s sweep  r reload  f filter  q quit";

/// Writes `amount` of a token with `decimals` decimals as a decimal number with four places,
/// truncated rather than rounded so a shown profit is never more than the real one.
pub fn format_amount(amount: Amount, decimals: u8) -> String {
  let ten = Amount::from(10u64);
  let unit = ten.pow(Amount::from(decimals));
  let whole = amount / unit;
  let fraction = amount % unit;
  let places = if decimals >= PLACES {
    fraction / ten.pow(Amount::from(decimals - PLACES))
  } else {
    fraction * ten.pow(Amount::from(PLACES - decimals))
  };
  format!("{whole}.{:04}", places.to::<u64>())
}

/// Writes `gain - loss` in base token units, signed, since net profit can be negative.
pub fn format_net(gain: Amount, loss: Amount) -> String {
  if gain >= loss {
    format_amount(gain - loss, BASE_DECIMALS)
  } else {
    format!("-{}", format_amount(loss - gain, BASE_DECIMALS))
  }
}

/// Writes a fee per gas in gwei.
pub fn format_gwei(fee: u128) -> String {
  format_amount(Amount::from(fee), GWEI_DECIMALS)
}

/// Writes microseconds as milliseconds with three places.
pub fn format_latency(micros: u64) -> String {
  format!("{}.{:03}ms", micros / 1000, micros % 1000)
}

/// Writes the value, or a dash when it is absent.
pub fn or_dash<T: Display>(value: Option<T>) -> String {
  value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

/// Shortens a long address or hash to its two ends so table columns stay narrow.
pub fn short(text: &str) -> String {
  if text.len() <= 2 * SHORT_KEEP + 2 || !text.is_ascii() {
    return text.to_string();
  }
  format!(
    "{}..{}",
    &text[..SHORT_KEEP + 2],
    &text[text.len() - SHORT_KEEP..]
  )
}

/// The venue name and fee tier, the first two words of a pool's `describe` text. Table
/// columns are too narrow for the price and reserve numbers that follow, which only show
/// truncated; the detail view keeps the full text.
pub fn venue_label(describe: &str) -> String {
  describe
    .split_whitespace()
    .take(VENUE_WORDS)
    .collect::<Vec<_>>()
    .join(" ")
}

/// The label a verdict is listed under.
pub fn verdict_label(verdict: Verdict) -> String {
  match verdict {
    Verdict::Sent => "SENT".into(),
    Verdict::BelowMin => "skip: min".into(),
    Verdict::GasExceedsProfit => "skip: gas".into(),
    Verdict::GasCap => "skip: gas cap".into(),
    Verdict::QuoteError => "skip: quote".into(),
    Verdict::Locked(trade) => format!("locked #{trade}"),
    Verdict::LowGas => "skip: low gas".into(),
    Verdict::CircuitOpen => "breaker".into(),
    Verdict::Paused => "paused".into(),
  }
}

/// Whether the bot is connecting, rebuilding, running, paused or draining before exit.
fn run_state(app: &App) -> &'static str {
  if app.stopping {
    "stopping, q again to force"
  } else if app.phase == Phase::Connecting {
    "not trading yet"
  } else if app.phase == Phase::Rebuilding {
    "rebuilding, loading pools"
  } else if app.paused {
    "paused"
  } else {
    "running"
  }
}

/// The tab names with the shown one in brackets.
fn tab_names(tab: Tab) -> String {
  [
    (Tab::Main, "Main"),
    (Tab::Pairs, "Pairs"),
    (Tab::Config, "Config"),
  ]
  .iter()
  .map(|(each, name)| {
    if *each == tab {
      format!("[{name}]")
    } else {
      name.to_string()
    }
  })
  .collect::<Vec<_>>()
  .join(" ")
}

/// Network, block, head age (time since the last head was seen), next base fee, the fee
/// cap against its ceiling and run state; while the first chain is built, which can take
/// minutes, a line saying so instead.
fn header(app: &App) -> String {
  let chain = match (&app.status, app.phase) {
    (_, Phase::Connecting) => format!("connecting to {}, loading pools", app.network),
    (Some(status), _) => format!(
      "{}  block {}  head age {}ms  base {} gwei  fee cap {} / {} gwei",
      app.network,
      status.block,
      status.head_age_ms,
      format_gwei(status.next_base_fee),
      format_gwei(status.fee_cap),
      format_gwei(status.max_fee_per_gas),
    ),
    (None, _) => format!("{}  waiting for status", app.network),
  };
  format!("{chain}  {}  {}", run_state(app), tab_names(app.tab))
}

/// Net, gross and gas sums and the settled trade counts.
fn totals(app: &App) -> String {
  let t = &app.totals;
  format!(
    "net {}  gross {}  gas {}  executed {}  reverted {}  dropped {}",
    format_net(t.gross, t.gas),
    format_amount(t.gross, BASE_DECIMALS),
    format_amount(t.gas, BASE_DECIMALS),
    t.executed,
    t.reverted,
    t.dropped,
  )
}

/// The executor's balance against its cap, the treasury's balance and accounts ready to send.
fn holdings(app: &App) -> String {
  let Some(status) = &app.status else {
    return "holdings unknown".to_string();
  };
  let ready = status.accounts.iter().filter(|a| !a.low_gas).count();
  format!(
    "executor {} / {}  treasury {}  accounts ready {}/{}",
    format_amount(status.executor_balance, BASE_DECIMALS),
    format_amount(app.executor_cap, BASE_DECIMALS),
    format_amount(status.treasury_balance, BASE_DECIMALS),
    ready,
    status.accounts.len(),
  )
}

/// Block, pair, venues, size, profit, gas, net and verdict of one decision.
fn decision_row(decision: &Decision) -> Row<'static> {
  let money = |amount| format_amount(amount, BASE_DECIMALS);
  let (pair, size, profit, net) = match &decision.op {
    Some(op) => (
      short(&op.token.to_string()),
      money(op.amount),
      money(op.profit),
      format_net(op.profit, decision.cost),
    ),
    None => ("-".into(), "-".into(), "-".into(), "-".into()),
  };
  Row::new(vec![
    decision.block.to_string(),
    pair,
    venue_label(&decision.buy.describe()),
    venue_label(&decision.sell.describe()),
    size,
    profit,
    money(decision.cost),
    net,
    verdict_label(decision.verdict),
  ])
}

/// The recent decisions under the filter, newest first, with the selected row highlighted.
fn opportunities(frame: &mut Frame, app: &App, area: Rect) {
  let visible = app.visible();
  let rows: Vec<Row> = visible.iter().map(|(_, d)| decision_row(d)).collect();
  let widths = [
    Constraint::Length(BLOCK_WIDTH),
    Constraint::Length(16),
    Constraint::Fill(1),
    Constraint::Fill(1),
    Constraint::Length(10),
    Constraint::Length(10),
    Constraint::Length(10),
    Constraint::Length(10),
    Constraint::Length(VERDICT_WIDTH),
  ];
  let header = Row::new(vec![
    "block", "pair", "buy", "sell", "size", "profit", "gas", "net", "verdict",
  ]);
  let table = Table::new(rows, widths)
    .header(header.style(Style::new().add_modifier(Modifier::BOLD)))
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title(format!("Opportunities  filter {:?}", app.filter)));
  let mut state = TableState::default().with_selected(Some(app.selected_row()));
  frame.render_stateful_widget(table, area, &mut state);
}

/// The latest state of each trade, newest first.
fn trades(frame: &mut Frame, app: &App, area: Rect) {
  let money = |amount| format_amount(amount, BASE_DECIMALS);
  let mut seen = HashSet::new();
  let rows: Vec<Row> = app
    .trades
    .iter()
    .rev()
    .filter(|event| seen.insert(event.id))
    .map(|event| {
      let detail = &event.detail;
      Row::new(vec![
        event.id.to_string(),
        format!("{:?}", event.kind),
        or_dash(
          detail
            .pair
            .as_ref()
            .map(|(_, token)| short(&token.to_string())),
        ),
        short(&event.account),
        format!("{:?}", event.state),
        or_dash(event.tx.as_deref().map(short)),
        or_dash(detail.quoted_profit.map(money)),
        or_dash(detail.actual_profit.map(money)),
        or_dash(detail.gas_used),
        or_dash(detail.latency_us.map(|stages| format_latency(stages[3]))),
      ])
    })
    .collect();
  let widths = [
    Constraint::Length(6),
    Constraint::Length(8),
    Constraint::Length(16),
    Constraint::Length(16),
    Constraint::Length(9),
    Constraint::Length(16),
    Constraint::Length(10),
    Constraint::Length(10),
    Constraint::Length(9),
    Constraint::Length(10),
  ];
  let header = Row::new(vec![
    "id", "kind", "pair", "account", "state", "tx", "expected", "actual", "gas used", "latency",
  ]);
  let table = Table::new(rows, widths)
    .header(header.style(Style::new().add_modifier(Modifier::BOLD)))
    .block(Block::bordered().title("Trades"));
  frame.render_widget(table, area);
}

/// Every account with its role, balance, nonce, trades in flight and low gas flag.
fn accounts(frame: &mut Frame, app: &App, area: Rect) {
  let list = app.status.as_ref().map_or(&[][..], |s| &s.accounts[..]);
  let rows: Vec<Row> = list
    .iter()
    .map(|account| {
      Row::new(vec![
        short(&account.address),
        match account.role {
          Role::Owner => "owner".into(),
          Role::Operator => "operator".into(),
        },
        format_amount(account.eth, BASE_DECIMALS),
        account.nonce.to_string(),
        account.in_flight.to_string(),
        if account.low_gas { "LOW" } else { "" }.into(),
      ])
    })
    .collect();
  let widths = [
    Constraint::Length(16),
    Constraint::Length(9),
    Constraint::Length(10),
    Constraint::Length(6),
    Constraint::Length(9),
    Constraint::Length(7),
  ];
  let header = Row::new(vec!["address", "role", "ETH", "nonce", "in flight", "gas"]);
  let table = Table::new(rows, widths)
    .header(header.style(Style::new().add_modifier(Modifier::BOLD)))
    .block(Block::bordered().title("Accounts"));
  frame.render_widget(table, area);
}

/// The newest log lines that fit.
fn log(frame: &mut Frame, app: &App, area: Rect) {
  let fit = usize::from(area.height.saturating_sub(2));
  let skip = app.log.len().saturating_sub(fit);
  let text: Vec<&str> = app.log.iter().skip(skip).map(String::as_str).collect();
  let panel = Paragraph::new(text.join("\n")).block(Block::bordered().title("Log"));
  frame.render_widget(panel, area);
}

/// Opportunities over trades, with accounts and the log side by side below.
fn main_tab(frame: &mut Frame, app: &App, area: Rect) {
  let [top, middle, bottom] = Layout::vertical([
    Constraint::Percentage(40),
    Constraint::Percentage(30),
    Constraint::Percentage(30),
  ])
  .areas(area);
  let [left, right] =
    Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(bottom);
  opportunities(frame, app, top);
  trades(frame, app, middle);
  accounts(frame, app, left);
  log(frame, app, right);
}

/// Every pair seen, with its venues, spread (profit over size) and the block of its last update.
fn pairs_tab(frame: &mut Frame, app: &App, area: Rect) {
  let rows: Vec<Row> = app
    .pairs
    .values()
    .map(|decision| {
      let spread = decision.op.as_ref().and_then(|op| {
        let bps = op.profit.checked_mul(Amount::from(BPS))?;
        bps.checked_div(op.amount)
      });
      Row::new(vec![
        venue_label(&decision.buy.describe()),
        venue_label(&decision.sell.describe()),
        or_dash(spread),
        decision.block.to_string(),
        verdict_label(decision.verdict),
      ])
    })
    .collect();
  let widths = [
    Constraint::Fill(1),
    Constraint::Fill(1),
    Constraint::Length(11),
    Constraint::Length(11),
    Constraint::Length(VERDICT_WIDTH),
  ];
  let header = Row::new(vec!["buy", "sell", "spread bps", "last block", "verdict"]);
  let table = Table::new(rows, widths)
    .header(header.style(Style::new().add_modifier(Modifier::BOLD)))
    .block(Block::bordered().title("Pairs"));
  frame.render_widget(table, area);
}

/// The config file as loaded, read only.
fn config_tab(frame: &mut Frame, app: &App, area: Rect) {
  let panel = Paragraph::new(app.config.as_str()).block(Block::bordered().title("Config"));
  frame.render_widget(panel, area);
}

/// Draws the main screen with the page of the current tab.
pub fn draw(frame: &mut Frame, app: &App) {
  let [top, sums, held, body, keys] = Layout::vertical([
    Constraint::Length(1),
    Constraint::Length(1),
    Constraint::Length(1),
    Constraint::Min(0),
    Constraint::Length(1),
  ])
  .areas(frame.area());
  frame.render_widget(Paragraph::new(header(app)), top);
  frame.render_widget(Paragraph::new(totals(app)), sums);
  frame.render_widget(Paragraph::new(holdings(app)), held);
  match app.tab {
    Tab::Main => main_tab(frame, app, body),
    Tab::Pairs => pairs_tab(frame, app, body),
    Tab::Config => config_tab(frame, app, body),
  }
  frame.render_widget(Paragraph::new(KEYS), keys);
}
