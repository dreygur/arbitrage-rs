//! The step loop that turns chain updates into trade decisions.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{Instrument, info_span, warn};

use crate::chain::Chain;
use crate::config::Live;
use crate::error::Result;
use crate::strategy::{Evaluation, find_all};
use crate::types::{Amount, Control, Id, Opportunity, Verdict};
use crate::venue::Venue;

/// Pause after a failed `changes` call before asking again.
const BACKOFF: Duration = Duration::from_millis(500);

/// How a call to [`Bot::run`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
  /// The feed asked for a restart.
  Restart,
  /// The step budget was used up.
  Done,
}

/// The record of one evaluated pair, for the dashboard.
pub struct Decision {
  /// Block the evaluation ran for.
  pub block: u64,
  /// The trade considered, absent when quoting failed.
  pub op: Option<Opportunity>,
  /// Gas cost, zero when the chain could not price it.
  pub cost: Amount,
  /// What became of the evaluation.
  pub verdict: Verdict,
  /// Venue where the non base token is bought.
  pub buy: Arc<dyn Venue>,
  /// Venue where the non base token is sold.
  pub sell: Arc<dyn Venue>,
}

/// Why a priced opportunity is not sent, or `None` when it passes every check.
fn rejection(profit: Amount, cost: Option<Amount>, live: &Live) -> Option<Verdict> {
  let Some(cost) = cost.filter(|cost| *cost <= live.max_gas_cost) else {
    return Some(Verdict::GasCap);
  };
  if profit <= cost {
    Some(Verdict::GasExceedsProfit)
  } else if profit <= cost.saturating_add(live.min_profit) {
    Some(Verdict::BelowMin)
  } else {
    None
  }
}

/// Evaluates chain updates and submits the trades worth sending.
pub struct Bot<C: Chain> {
  /// The network.
  chain: Arc<C>,
  /// The token the bot borrows and profits in.
  base: Id,
  /// Whether new trades are held back.
  paused: AtomicBool,
  /// Settings that can change while running.
  live: RwLock<Live>,
  /// Where decisions go; a full channel drops them.
  decisions: mpsc::Sender<Decision>,
}

impl<C: Chain> Bot<C> {
  /// Builds a bot over `chain` that reports to `decisions`.
  pub fn new(chain: Arc<C>, live: Live, decisions: mpsc::Sender<Decision>) -> Self {
    Self {
      base: chain.base(),
      chain,
      paused: AtomicBool::new(false),
      live: RwLock::new(live),
      decisions,
    }
  }

  /// Pauses or resumes trading and tells the chain.
  pub fn set_paused(&self, paused: bool) {
    self.paused.store(paused, Ordering::SeqCst);
    self.chain.control(if paused {
      Control::Pause
    } else {
      Control::Resume
    });
  }

  /// Replaces the live settings here and on the chain.
  pub fn apply_live(&self, live: Live) {
    self.chain.apply_live(&live);
    *self.live.write().unwrap_or_else(|e| e.into_inner()) = live;
  }

  /// Sends a decision without ever waiting for room.
  fn report(&self, decision: Decision) {
    if self.decisions.try_send(decision).is_err() {
      warn!("decision channel full, dropping a decision");
    }
  }

  /// Evaluates the pairs touched in `block` and submits the passing trades.
  ///
  /// Returns each evaluated opportunity with its verdict; pairs whose quote failed have none.
  pub async fn step(
    &self,
    block: u64,
    touched: Option<&HashSet<Id>>,
  ) -> Vec<(Opportunity, Verdict)> {
    let span = info_span!("step", block, elapsed_us = tracing::field::Empty);
    let started = Instant::now();
    let results = self.evaluate(block, touched).instrument(span.clone()).await;
    span.record("elapsed_us", started.elapsed().as_micros() as u64);
    results
  }

  /// The body of [`Bot::step`].
  async fn evaluate(
    &self,
    block: u64,
    touched: Option<&HashSet<Id>>,
  ) -> Vec<(Opportunity, Verdict)> {
    let live = self.live.read().unwrap_or_else(|e| e.into_inner()).clone();
    let paused = self.paused.load(Ordering::SeqCst);
    let evaluations = find_all(&self.chain.snapshots(), &self.base, live.max_trade, touched);
    let mut pending = Vec::new();
    let mut to_send = Vec::new();
    for evaluation in evaluations {
      match evaluation {
        Evaluation::Failed { buy, sell, .. } => self.report(Decision {
          block,
          op: None,
          cost: Amount::ZERO,
          verdict: Verdict::QuoteError,
          buy,
          sell,
        }),
        Evaluation::Found { mut op, buy, sell } => {
          let cost = self.chain.cost(&op);
          let verdict = if paused {
            Some(Verdict::Paused)
          } else {
            rejection(op.profit, cost, &live)
          };
          let cost = cost.unwrap_or(Amount::ZERO);
          if verdict.is_none() {
            op.floor = cost.saturating_add(live.min_profit);
            to_send.push(op.clone());
          }
          pending.push((op, cost, verdict, buy, sell));
        }
      }
    }
    let mut sent = if to_send.is_empty() {
      Vec::new()
    } else {
      self.chain.submit(to_send).await
    }
    .into_iter();
    let mut results = Vec::new();
    for (op, cost, verdict, buy, sell) in pending {
      let verdict = match verdict {
        Some(verdict) => verdict,
        None => sent.next().map_or(Verdict::Sent, |(_, verdict)| verdict),
      };
      self.report(Decision {
        block,
        op: Some(op.clone()),
        cost,
        verdict,
        buy,
        sell,
      });
      results.push((op, verdict));
    }
    results
  }

  /// Steps once, then once per chain update until a restart or the step budget ends.
  pub async fn run(&self, max_steps: Option<u64>) -> Result<Exit> {
    self.step(0, None).await;
    let mut steps = 0u64;
    while max_steps.is_none_or(|max| steps < max) {
      steps += 1;
      let update = match self.chain.changes().await {
        Ok(update) => update,
        Err(error) => {
          warn!(%error, "chain feed failed, retrying");
          tokio::time::sleep(BACKOFF).await;
          continue;
        }
      };
      if update.restart {
        return Ok(Exit::Restart);
      }
      let touched = (!update.reload).then_some(&update.touched);
      self.step(update.block, touched).await;
    }
    Ok(Exit::Done)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::Error;
  use crate::testkit::{MockChain, MockVenue};
  use crate::types::Update;

  /// Id from a name.
  fn id(name: &str) -> Id {
    Id::new(name.as_bytes().to_vec())
  }

  /// Live settings from the shipped config with the given thresholds.
  fn live(min_profit: u64, max_gas_cost: u64) -> Live {
    let text = include_str!("../config/ethereum.toml");
    let mut live: Live = toml::from_str(text).unwrap();
    live.min_profit = Amount::from(min_profit);
    live.max_trade = Amount::from(10_000u64);
    live.max_gas_cost = Amount::from(max_gas_cost);
    live
  }

  /// Handle to a mock pool of `base` and `other`.
  fn venue(pool: &str, other: &str, r1: u64) -> Arc<dyn Venue> {
    Arc::new(MockVenue::new(pool, "base", other, 10_000, r1, 30))
  }

  /// A profitable pair, buying on `a` and selling on `b`.
  fn pair() -> Vec<Arc<dyn Venue>> {
    vec![venue("a", "other", 25_000), venue("b", "other", 20_000)]
  }

  /// A bot over a mock chain with the given pools and thresholds, plus the decision receiver.
  fn setup(
    venues: Vec<Arc<dyn Venue>>,
    live: Live,
    room: usize,
  ) -> (Bot<MockChain>, Arc<MockChain>, mpsc::Receiver<Decision>) {
    let chain = Arc::new(MockChain::new("base", venues));
    let (tx, rx) = mpsc::channel(room);
    (Bot::new(chain.clone(), live, tx), chain, rx)
  }

  /// Drains the decisions queued so far.
  fn drain(rx: &mut mpsc::Receiver<Decision>) -> Vec<Decision> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
  }

  /// A passing trade is submitted with a floor of cost plus the minimum profit.
  #[tokio::test]
  async fn submits_when_profit_clears_cost() {
    let (bot, chain, mut rx) = setup(pair(), live(10, 1_000_000), 8);
    chain.set_cost("a", Some(Amount::from(3u64)));
    let results = bot.step(1, None).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, Verdict::Sent);
    let submits = chain.submits.lock().unwrap();
    assert_eq!(submits.len(), 1);
    assert_eq!(submits[0][0].floor, Amount::from(13u64));
    let decisions = drain(&mut rx);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].cost, Amount::from(3u64));
    assert_eq!(decisions[0].verdict, Verdict::Sent);
  }

  /// A cost above the profit is refused as gas exceeding profit.
  #[tokio::test]
  async fn gas_exceeds_profit_verdict() {
    let (bot, chain, _rx) = setup(pair(), live(10, u64::MAX), 8);
    chain.set_cost("a", Some(Amount::from(1_000_000u64)));
    let results = bot.step(1, None).await;
    assert_eq!(results[0].1, Verdict::GasExceedsProfit);
    assert!(chain.submits.lock().unwrap().is_empty());
  }

  /// Profit that covers gas but not the minimum on top is below the minimum.
  #[tokio::test]
  async fn below_min_verdict() {
    let (bot, chain, _rx) = setup(pair(), live(1_000_000, u64::MAX), 8);
    let results = bot.step(1, None).await;
    assert_eq!(results[0].1, Verdict::BelowMin);
    assert!(chain.submits.lock().unwrap().is_empty());
  }

  /// A cost above `max_gas_cost` is a gas cap hit.
  #[tokio::test]
  async fn gas_cap_verdict_when_cost_above_max() {
    let (bot, chain, _rx) = setup(pair(), live(10, 5), 8);
    chain.set_cost("a", Some(Amount::from(6u64)));
    let results = bot.step(1, None).await;
    assert_eq!(results[0].1, Verdict::GasCap);
    assert!(chain.submits.lock().unwrap().is_empty());
  }

  /// An unknown cost, from the chain's fee cap, is a gas cap hit.
  #[tokio::test]
  async fn gas_cap_verdict_when_fee_cap_hit() {
    let (bot, chain, _rx) = setup(pair(), live(10, u64::MAX), 8);
    chain.set_cost("a", None);
    let results = bot.step(1, None).await;
    assert_eq!(results[0].1, Verdict::GasCap);
    assert!(chain.submits.lock().unwrap().is_empty());
  }

  /// While paused nothing is submitted and the decision says why.
  #[tokio::test]
  async fn paused_submits_nothing() {
    let (bot, chain, mut rx) = setup(pair(), live(10, u64::MAX), 8);
    bot.set_paused(true);
    let results = bot.step(1, None).await;
    assert_eq!(results[0].1, Verdict::Paused);
    assert!(chain.submits.lock().unwrap().is_empty());
    assert_eq!(drain(&mut rx)[0].verdict, Verdict::Paused);
  }

  /// A pair that cannot be quoted yields a decision without a trade.
  #[tokio::test]
  async fn quote_error_becomes_decision() {
    let mut broken = MockVenue::new("a", "base", "other", 10_000, 25_000, 30);
    broken.fail_above = Some(Amount::ZERO);
    let venues: Vec<Arc<dyn Venue>> = vec![Arc::new(broken), venue("b", "other", 20_000)];
    let (bot, chain, mut rx) = setup(venues, live(10, u64::MAX), 8);
    let results = bot.step(1, None).await;
    assert!(results.is_empty());
    let decisions = drain(&mut rx);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].verdict, Verdict::QuoteError);
    assert!(decisions[0].op.is_none());
    assert!(chain.submits.lock().unwrap().is_empty());
  }

  /// The engine's verdict reaches the result and the decision.
  #[tokio::test]
  async fn engine_verdicts_pass_through() {
    let (bot, chain, mut rx) = setup(pair(), live(10, u64::MAX), 8);
    chain.set_verdict(Verdict::Locked(7));
    let results = bot.step(1, None).await;
    assert_eq!(results[0].1, Verdict::Locked(7));
    assert_eq!(drain(&mut rx)[0].verdict, Verdict::Locked(7));
  }

  /// A failing feed is retried after a pause and the next update is still stepped.
  #[tokio::test(start_paused = true)]
  async fn keeps_running_after_rpc_error() {
    let (bot, chain, mut rx) = setup(pair(), live(10, u64::MAX), 8);
    chain.push_update(Err(Error::Rpc("down".into())));
    chain.push_update(Ok(Update {
      block: 5,
      reload: true,
      ..Default::default()
    }));
    let exit = bot.run(Some(2)).await.unwrap();
    assert_eq!(exit, Exit::Done);
    let blocks: Vec<u64> = drain(&mut rx).iter().map(|d| d.block).collect();
    assert_eq!(blocks, vec![0, 5]);
  }

  /// Only pairs with a touched pool are evaluated.
  #[tokio::test]
  async fn only_touched_pairs_evaluated() {
    let mut venues = pair();
    venues.push(venue("c", "third", 25_000));
    venues.push(venue("d", "third", 20_000));
    let (bot, _chain, mut rx) = setup(venues, live(10, u64::MAX), 8);
    let touched: HashSet<Id> = [id("c")].into();
    let results = bot.step(1, Some(&touched)).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0.token, id("third"));
    assert_eq!(drain(&mut rx).len(), 1);
  }

  /// A full decision channel drops decisions instead of blocking the step.
  #[tokio::test]
  async fn full_decision_channel_does_not_block() {
    let mut venues = pair();
    venues.push(venue("c", "third", 25_000));
    venues.push(venue("d", "third", 20_000));
    let (bot, _chain, mut rx) = setup(venues, live(10, u64::MAX), 1);
    let results = bot.step(1, None).await;
    assert_eq!(results.len(), 2);
    assert_eq!(drain(&mut rx).len(), 1);
  }

  /// A restart update ends the run with `Exit::Restart`.
  #[tokio::test]
  async fn restart_update_exits_run() {
    let (bot, chain, _rx) = setup(pair(), live(10, u64::MAX), 8);
    chain.push_update(Ok(Update {
      restart: true,
      ..Default::default()
    }));
    assert_eq!(bot.run(None).await.unwrap(), Exit::Restart);
  }

  /// Pausing and resuming reach the chain as controls.
  #[tokio::test]
  async fn pause_sends_control() {
    let (bot, chain, _rx) = setup(pair(), live(10, u64::MAX), 8);
    bot.set_paused(true);
    bot.set_paused(false);
    assert_eq!(
      *chain.controls.lock().unwrap(),
      vec![Control::Pause, Control::Resume]
    );
  }
}
