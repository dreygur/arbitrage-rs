//! Timing of one strategy step over 200 constant product pairs plus 50 V4 against V2 pairs.

use std::collections::HashSet;
use std::hint::black_box;
use std::sync::Arc;

use alloy::primitives::Address;
use arbitrage::chain::evm::convert::to_id;
use arbitrage::chain::evm::venue::v2::V2Pool;
use arbitrage::strategy::{Evaluation, find_all};
use arbitrage::testkit::{MockVenue, v4_fixture_quoting};
use arbitrage::types::{Amount, Id};
use arbitrage::venue::Venue;
use criterion::{Criterion, criterion_group, criterion_main};

/// Number of token pairs listed on both mock venues.
const PAIRS: u64 = 200;
/// Number of V4 fixture pools, each paired with one V2 pool.
const V4_PAIRS: u64 = 50;
/// Base token reserve of every mock and V2 pool.
const BASE_RESERVE: u64 = 1_000_000_000;
/// Quote token reserve of the first venue.
const QUOTE_RESERVE: u64 = 1_000_000_000;
/// Swap fee of every mock and V2 pool in basis points.
const FEE_BPS: u16 = 30;
/// Largest base input a trade may use.
const MAX_TRADE: u64 = 100_000_000;

/// Builds both mock venues: pair `i` has the second venue's quote reserve raised by `i * 0.05%`.
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

/// Builds V4 fixtures at price one, each against a V2 pool whose quote reserve is raised by
/// `1% + i * 0.02%`, so every pair has a trade across both families.
fn v4_venues() -> Vec<Arc<dyn Venue>> {
  let mut venues: Vec<Arc<dyn Venue>> = Vec::new();
  for i in 0..V4_PAIRS {
    let quote = Address::left_padding_from(&(1_000 + i).to_be_bytes());
    let raised = QUOTE_RESERVE * (10_100 + 2 * i) / 10_000;
    venues.push(Arc::new(v4_fixture_quoting(quote)));
    venues.push(Arc::new(V2Pool::new(
      "uniswap_v2".into(),
      Id::new(format!("v2{i:03}").into_bytes()),
      Id::new(b"base".to_vec()),
      to_id(quote),
      Amount::from(BASE_RESERVE),
      Amount::from(raised),
      FEE_BPS,
    )));
  }
  venues
}

/// Times `find_all` over every pair, over one touched mock pool and over one touched V4 pool.
fn bench_step(c: &mut Criterion) {
  let mut venues = mock_venues();
  venues.extend(v4_venues());
  let base = Id::new(b"base".to_vec());
  let max_trade = Amount::from(MAX_TRADE);
  let touched: HashSet<Id> = HashSet::from([Id::new(b"b100".to_vec())]);
  let v4_found = find_all(&venues, &base, max_trade, None)
    .iter()
    .filter(|e| match e {
      Evaluation::Found { buy, sell, .. } => {
        buy.describe().starts_with("uniswap_v4") || sell.describe().starts_with("uniswap_v4")
      }
      Evaluation::Failed { .. } => false,
    })
    .count();
  assert_eq!(v4_found as u64, V4_PAIRS, "every V4 pair must find a trade");

  c.bench_function("find_all_all_pairs", |b| {
    b.iter(|| find_all(black_box(&venues), &base, max_trade, None))
  });
  c.bench_function("find_all_one_touched", |b| {
    b.iter(|| find_all(black_box(&venues), &base, max_trade, Some(&touched)))
  });
  let v4_pool = venues
    .iter()
    .find(|v| v.describe().starts_with("uniswap_v4"))
    .expect("the mix holds V4 pools")
    .pool()
    .clone();
  let touched_v4: HashSet<Id> = HashSet::from([v4_pool]);
  c.bench_function("find_all_one_v4_touched", |b| {
    b.iter(|| find_all(black_box(&venues), &base, max_trade, Some(&touched_v4)))
  });
}

criterion_group!(benches, bench_step);
criterion_main!(benches);
