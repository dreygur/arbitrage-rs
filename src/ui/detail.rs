//! The detail view of one decision: its pools, legs and profit curve recomputed from the
//! decision's own snapshots, and how its trade was sent and settled.

use ratatui::Frame;
use ratatui::widgets::{Block, Paragraph};

use super::app::App;
use super::views::{
  BASE_DECIMALS, format_amount, format_gwei, format_latency, or_dash, verdict_label,
};
use crate::bot::Decision;
use crate::error::Result;
use crate::sizing::gain;
use crate::types::{Amount, Id, Opportunity, TradeEvent};
use crate::venue::{Venue, side_for};

/// Sizes on the profit curve, as multiples of the chosen size over [`CURVE_DIVISOR`]: one
/// fifth up to twice the chosen size.
const CURVE_POINTS: u64 = 10;
/// Divisor of the curve's size multiples.
const CURVE_DIVISOR: u64 = 5;
/// Width of the longest profit bar.
const BAR_WIDTH: u64 = 40;

/// Base token amount written with its decimals.
fn money(amount: Amount) -> String {
  format_amount(amount, BASE_DECIMALS)
}

/// The base token of the pair: the buy pool's token that is not the one traded.
fn base_of<'a>(buy: &'a dyn Venue, op: &Opportunity) -> Option<&'a Id> {
  buy.tokens().into_iter().find(|token| **token != op.token)
}

/// Quotes both legs at the chosen size: the traded token bought, then the base sold back.
fn legs(decision: &Decision, op: &Opportunity, base: &Id) -> Result<(Amount, Amount)> {
  let bought = decision
    .buy
    .quote_out(op.amount, side_for(decision.buy.as_ref(), base)?)?;
  let back = decision
    .sell
    .quote_out(bought, side_for(decision.sell.as_ref(), &op.token)?)?;
  Ok((bought, back))
}

/// The profit curve over sizes around the chosen one, with bars scaled to the best point.
fn curve(decision: &Decision, op: &Opportunity, base: &Id) -> Vec<String> {
  let points: Vec<(Amount, Result<Option<Amount>>)> = (1..=CURVE_POINTS)
    .map(|step| {
      let size = op.amount.saturating_mul(Amount::from(step)) / Amount::from(CURVE_DIVISOR);
      let profit = gain(decision.buy.as_ref(), decision.sell.as_ref(), base, size);
      (size, profit)
    })
    .collect();
  let best = points
    .iter()
    .filter_map(|(_, profit)| profit.as_ref().ok().copied().flatten())
    .max()
    .unwrap_or(Amount::ZERO);
  points
    .into_iter()
    .map(|(size, profit)| {
      let shown = match profit {
        Ok(Some(profit)) => {
          let bar = profit.saturating_mul(Amount::from(BAR_WIDTH)) / best.max(Amount::from(1u64));
          format!("{:>10}  {}", money(profit), "#".repeat(bar.to::<usize>()))
        }
        Ok(None) => "loss".to_string(),
        Err(error) => format!("fails: {error}"),
      };
      format!("  {:>12}  {shown}", money(size))
    })
    .collect()
}

/// The latest event of the trade sent for `op`, matched by pools and size, then followed by
/// its ledger id since later events may carry less detail.
fn trade_for<'a>(app: &'a App, op: &Opportunity) -> Option<&'a TradeEvent> {
  let sent = app.trades.iter().rev().find(|event| {
    event.detail.buy_pool.as_ref() == Some(&op.buy_pool)
      && event.detail.sell_pool.as_ref() == Some(&op.sell_pool)
      && event.detail.amount == Some(op.amount)
  })?;
  app.trades.iter().rev().find(|event| event.id == sent.id)
}

/// Send details, result and timing of a trade.
fn trade_lines(event: &TradeEvent) -> Vec<String> {
  let detail = &event.detail;
  let mut lines = vec![
    format!(
      "Send     account {}  nonce {}  gas limit {}  fee cap {} gwei  tip {} gwei  floor {} ETH",
      event.account,
      event.nonce,
      or_dash(detail.gas_limit),
      or_dash(detail.fee_cap.map(format_gwei)),
      or_dash(detail.tip.map(format_gwei)),
      or_dash(detail.floor.map(money)),
    ),
    format!(
      "Result   {:?}  tx {}  block {}  gas used {}  actual profit {} ETH  revert {}",
      event.state,
      or_dash(event.tx.as_deref()),
      or_dash(detail.mined_block),
      or_dash(detail.gas_used),
      or_dash(detail.actual_profit.map(money)),
      or_dash(detail.revert.as_deref()),
    ),
  ];
  if let Some([read, decided, signed, sent]) = detail.latency_us {
    lines.push(format!(
      "Timing   read {}  decided {}  signed {}  sent {}",
      format_latency(read),
      format_latency(decided),
      format_latency(signed),
      format_latency(sent),
    ));
  }
  lines
}

/// Every line of the detail view for `decision`.
fn lines(app: &App, decision: &Decision) -> Vec<String> {
  let mut lines = vec![
    format!(
      "Block {}  verdict {}",
      decision.block,
      verdict_label(decision.verdict)
    ),
    format!("Buy      {}", decision.buy.describe()),
    format!("Sell     {}", decision.sell.describe()),
  ];
  let Some(op) = &decision.op else {
    lines.push("Quoting failed; nothing to size.".into());
    return lines;
  };
  let Some(base) = base_of(decision.buy.as_ref(), op) else {
    lines.push("The buy pool does not hold the traded token.".into());
    return lines;
  };
  lines.push(format!(
    "Size     {} ETH  profit {} ETH  gas {} ETH  floor {} ETH",
    money(op.amount),
    money(op.profit),
    money(decision.cost),
    money(op.floor),
  ));
  lines.push(match legs(decision, op, base) {
    Ok((bought, back)) => format!(
      "Legs     buy gives {bought} token units, sell returns {} ETH",
      money(back)
    ),
    Err(error) => format!("Legs     quote failed: {error}"),
  });
  lines.push("Profit by size (ETH)".into());
  lines.extend(curve(decision, op, base));
  match trade_for(app, op) {
    Some(event) => lines.extend(trade_lines(event)),
    None => lines.push("Not sent.".into()),
  }
  lines
}

/// Draws the pinned decision in full.
pub fn draw_detail(frame: &mut Frame, app: &App) {
  let text = match &app.detail {
    Some(decision) => lines(app, decision).join("\n"),
    None => "Nothing selected.".to_string(),
  };
  let panel = Paragraph::new(text).block(Block::bordered().title("Detail  enter or esc to close"));
  frame.render_widget(panel, frame.area());
}
