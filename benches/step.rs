//! Baseline timing of one strategy step over 200 constant product pairs on two venues.

use std::collections::HashSet;
use std::hint::black_box;
use std::sync::Arc;

use arbitrage::strategy::find_all;
use arbitrage::testkit::MockVenue;
use arbitrage::types::{Amount, Id};
use arbitrage::venue::Venue;
use criterion::{Criterion, criterion_group, criterion_main};

/// Number of token pairs listed on both venues.
const PAIRS: u64 = 200;
/// Base token reserve of every pool.
const BASE_RESERVE: u64 = 1_000_000_000;
/// Quote token reserve of the first venue.
const QUOTE_RESERVE: u64 = 1_000_000_000;
/// Swap fee of every pool in basis points.
const FEE_BPS: u16 = 30;
/// Largest base input a trade may use.
const MAX_TRADE: u64 = 100_000_000;

/// Builds both venues: pair `i` has the second venue's quote reserve raised by `i * 0.05%`.
fn mock_venues() -> Vec<Arc<dyn Venue>> {
  let mut venues: Vec<Arc<dyn Venue>> = Vec::new();
  for i in 0..PAIRS {
    let quote = format!("quote{i:03}");
    let raised = QUOTE_RESERVE * (10_000 + 5 * i) / 10_000;
    venues.push(Arc::new(MockVenue::new(
      &format!("a{i:03}"),
      "base",
      &quote,
      BASE_RESERVE,
      QUOTE_RESERVE,
      FEE_BPS,
    )));
    venues.push(Arc::new(MockVenue::new(
      &format!("b{i:03}"),
      "base",
      &quote,
      BASE_RESERVE,
      raised,
      FEE_BPS,
    )));
  }
  venues
}

/// Times `find_all` over every pair and over a single touched pool.
fn bench_step(c: &mut Criterion) {
  let venues = mock_venues();
  let base = Id::new(b"base".to_vec());
  let max_trade = Amount::from(MAX_TRADE);
  let touched: HashSet<Id> = HashSet::from([Id::new(b"b100".to_vec())]);

  c.bench_function("find_all_all_pairs", |b| {
    b.iter(|| find_all(black_box(&venues), &base, max_trade, None))
  });
  c.bench_function("find_all_one_touched", |b| {
    b.iter(|| find_all(black_box(&venues), &base, max_trade, Some(&touched)))
  });
}

criterion_group!(benches, bench_step);
criterion_main!(benches);
