//! Initialized ticks of a V4 pool within a bounded window around its current tick.

use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::aliases::I24;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use uniswap_v3_sdk::prelude::{
  Error as SdkError, Tick, TickDataProvider, TickIndex, TickListError,
};

use crate::chain::evm::bindings::{Call3, IStateView, Result3};
use crate::chain::evm::multicall::{aggregate, decode, read};
use crate::chain::evm::venue::v4::V4Key;
use crate::error::{Error, Result};

/// Ticks covered by one bitmap word, in units of tick spacing.
pub const WORD_TICKS: i32 = 256;

/// Bitmap word holding the compressed `tick`.
fn word_of(tick: i32, tick_spacing: i32) -> i32 {
  tick.compress(tick_spacing) >> 8
}

/// Lowest and highest tick of the `words` bitmap words each side of the word holding `tick`.
///
/// Words are clamped to the `int16` range StateView takes.
fn word_range(tick: i32, tick_spacing: i32, words: u16) -> (i16, i16) {
  let word = word_of(tick, tick_spacing);
  let words = i32::from(words);
  let first = (word - words).max(i32::from(i16::MIN));
  let last = (word + words).min(i32::from(i16::MAX));
  (first as i16, last as i16)
}

/// Initialized ticks known between `lower` and `upper` inclusive.
///
/// Lookups and next tick searches outside the window fail instead of assuming empty liquidity,
/// so a quote that would leave the window errors rather than returning a wrong number. The ticks
/// are shared behind an `Arc` because the SDK clones its pool, provider included, on every quote.
#[derive(Clone, Debug)]
pub struct BoundedTicks {
  /// Initialized ticks inside the window, sorted by index.
  ticks: Arc<Vec<Tick<i32>>>,
  /// Lowest tick the window covers.
  lower: i32,
  /// Highest tick the window covers.
  upper: i32,
}

impl BoundedTicks {
  /// Builds a window over `[lower, upper]`, sorting `ticks` and dropping any outside the window.
  pub fn new(mut ticks: Vec<Tick<i32>>, lower: i32, upper: i32) -> Self {
    ticks.retain(|t| t.index >= lower && t.index <= upper);
    ticks.sort_by_key(|t| t.index);
    Self {
      ticks: Arc::new(ticks),
      lower,
      upper,
    }
  }

  /// Tick bounds of the `words` bitmap words each side of the word holding `tick`: the first
  /// tick of the first word and the last tick before the word after the last one.
  pub fn bounds(tick: i32, tick_spacing: i32, words: u16) -> (i32, i32) {
    let (first, last) = word_range(tick, tick_spacing, words);
    let lower = i32::from(first) * WORD_TICKS * tick_spacing;
    let upper = (i32::from(last) + 1) * WORD_TICKS * tick_spacing - 1;
    (lower, upper)
  }

  /// Lowest tick the window covers.
  pub fn lower(&self) -> i32 {
    self.lower
  }

  /// Highest tick the window covers.
  pub fn upper(&self) -> i32 {
    self.upper
  }

  /// Initialized ticks inside the window, sorted by index.
  pub fn ticks(&self) -> &[Tick<i32>] {
    &self.ticks
  }

  /// Adds `delta` to the liquidity referenced at `index` with net change `net`, as a
  /// `ModifyLiquidity` does at one end of its range. Ticks outside the window are ignored; a
  /// tick whose gross liquidity drops to zero is removed.
  pub fn modify(&mut self, index: i32, delta: i128, net: i128) -> Result<()> {
    if index < self.lower || index > self.upper {
      return Ok(());
    }
    let ticks = Arc::make_mut(&mut self.ticks);
    let at = ticks.partition_point(|t| t.index < index);
    if ticks.get(at).is_none_or(|t| t.index != index) {
      ticks.insert(
        at,
        Tick {
          index,
          liquidity_gross: 0,
          liquidity_net: 0,
        },
      );
    }
    let tick = &mut ticks[at];
    tick.liquidity_gross = tick
      .liquidity_gross
      .checked_add_signed(delta)
      .ok_or(Error::Overflow)?;
    tick.liquidity_net = tick.liquidity_net.checked_add(net).ok_or(Error::Overflow)?;
    if tick.liquidity_gross == 0 {
      ticks.remove(at);
    }
    Ok(())
  }

  /// Fails unless `tick` lies inside the window.
  fn check(&self, tick: i32) -> std::result::Result<(), SdkError> {
    if tick < self.lower || tick > self.upper {
      return Err(SdkError::NoTickDataError);
    }
    Ok(())
  }
}

impl TickDataProvider for BoundedTicks {
  type Index = i32;

  /// Returns the initialized tick at `index`, failing outside the window or for an unknown tick.
  async fn get_tick(&self, index: i32) -> std::result::Result<Tick<i32>, SdkError> {
    self.check(index)?;
    let at = self.ticks.partition_point(|t| t.index < index);
    match self.ticks.get(at) {
      Some(tick) if tick.index == index => Ok(*tick),
      _ => Err(TickListError::NotContained.into()),
    }
  }

  /// Mirrors the PoolManager's bitmap search: the nearest initialized tick in the current word,
  /// or the word's edge when none is. Fails when the search or its answer leaves the window.
  async fn next_initialized_tick_within_one_word(
    &self,
    tick: i32,
    lte: bool,
    tick_spacing: i32,
  ) -> std::result::Result<(i32, bool), SdkError> {
    self.check(tick)?;
    let compressed = tick.compress(tick_spacing);
    let after = self.ticks.partition_point(|t| t.index <= tick);
    let (next, initialized) = if lte {
      let minimum = ((compressed >> 8) << 8) * tick_spacing;
      match after.checked_sub(1).map(|i| self.ticks[i].index) {
        Some(index) if index >= minimum => (index, true),
        _ => (minimum, false),
      }
    } else {
      let maximum = ((((compressed + 1) >> 8) + 1) * WORD_TICKS - 1) * tick_spacing;
      match self.ticks.get(after).map(|t| t.index) {
        Some(index) if index <= maximum => (index, true),
        _ => (maximum, false),
      }
    };
    self.check(next)?;
    Ok((next, initialized))
  }
}

/// One tick window to read: the pool, the tick to center on and the bitmap words each side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowRequest {
  /// The pool key.
  pub key: V4Key,
  /// Tick the window is centered on.
  pub tick: i32,
  /// Bitmap words read each side of the word holding `tick`.
  pub words: u16,
}

/// Initialized tick indices in the bitmap `results` of consecutive words starting at `first`.
fn set_ticks(results: &[Result3], first: i16, tick_spacing: i32) -> Result<Vec<i32>> {
  let mut indices = Vec::new();
  for (word, result) in (i32::from(first)..).zip(results) {
    let bitmap: U256 = decode::<IStateView::getTickBitmapCall>(result)?;
    for bit in 0..WORD_TICKS {
      if bitmap.bit(bit as usize) {
        indices.push((word * WORD_TICKS + bit) * tick_spacing);
      }
    }
  }
  Ok(indices)
}

/// Liquidity reads for every index in `indices` of the pool `pool_id`.
fn liquidity_calls(state_view: Address, pool_id: B256, indices: &[i32]) -> Result<Vec<Call3>> {
  indices
    .iter()
    .map(|&index| {
      let tick = I24::try_from(index).map_err(|_| Error::Overflow)?;
      Ok(read(
        state_view,
        &IStateView::getTickLiquidityCall {
          poolId: pool_id,
          tick,
        },
      ))
    })
    .collect()
}

/// Reads the initialized ticks of every requested window as of `block`, one result per request.
///
/// Makes two Multicall3 batches to StateView for all pools together, both pinned to `block` so
/// the bitmaps and the tick liquidity come from the same state: one for every bitmap word in
/// range, then one for the liquidity of every set bit. A reverted or undecodable read fails
/// only its own pool's window; a failed request fails them all.
pub async fn fetch_windows<P: Provider>(
  provider: &P,
  multicall: Address,
  state_view: Address,
  block: BlockId,
  requests: &[WindowRequest],
) -> Result<Vec<Result<BoundedTicks>>> {
  let ranges: Vec<(i16, i16)> = requests
    .iter()
    .map(|r| word_range(r.tick, r.key.tick_spacing, r.words))
    .collect();
  let mut bitmap_calls = Vec::new();
  for (request, &(first, last)) in requests.iter().zip(&ranges) {
    let pool_id = request.key.pool_id();
    for word in first..=last {
      bitmap_calls.push(read(
        state_view,
        &IStateView::getTickBitmapCall {
          poolId: pool_id,
          tick: word,
        },
      ));
    }
  }
  let bitmaps = aggregate(provider, multicall, block, bitmap_calls).await?;
  let mut offset = 0;
  let mut found = Vec::with_capacity(requests.len());
  let mut tick_calls = Vec::new();
  for (request, &(first, last)) in requests.iter().zip(&ranges) {
    let count = (i32::from(last) - i32::from(first) + 1) as usize;
    let words = &bitmaps[offset..offset + count];
    offset += count;
    let indices = set_ticks(words, first, request.key.tick_spacing).and_then(|indices| {
      tick_calls.extend(liquidity_calls(
        state_view,
        request.key.pool_id(),
        &indices,
      )?);
      Ok(indices)
    });
    found.push(indices);
  }
  let liquidity = aggregate(provider, multicall, block, tick_calls).await?;
  let mut offset = 0;
  let mut windows = Vec::with_capacity(requests.len());
  for (request, indices) in requests.iter().zip(found) {
    let (lower, upper) =
      BoundedTicks::bounds(request.tick, request.key.tick_spacing, request.words);
    let window = indices.and_then(|indices| {
      let answers = &liquidity[offset..offset + indices.len()];
      offset += indices.len();
      let mut ticks = Vec::with_capacity(indices.len());
      for (index, result) in indices.into_iter().zip(answers) {
        let values = decode::<IStateView::getTickLiquidityCall>(result)?;
        ticks.push(Tick {
          index,
          liquidity_gross: values.liquidityGross,
          liquidity_net: values.liquidityNet,
        });
      }
      Ok(BoundedTicks::new(ticks, lower, upper))
    });
    windows.push(window);
  }
  Ok(windows)
}

/// Reads the initialized ticks of one window as of `block`, as `fetch_windows` does.
pub async fn fetch_window<P: Provider>(
  provider: &P,
  multicall: Address,
  state_view: Address,
  block: BlockId,
  request: WindowRequest,
) -> Result<BoundedTicks> {
  fetch_windows(provider, multicall, state_view, block, &[request])
    .await?
    .pop()
    .unwrap_or_else(|| Err(Error::Contract("no tick window returned".into())))
}

#[cfg(test)]
mod tests {
  use super::*;
  use futures::FutureExt;

  /// Tick spacing of the test windows.
  const SPACING: i32 = 60;

  /// Empty window over the word each side of the word holding tick zero.
  fn window() -> BoundedTicks {
    let (lower, upper) = BoundedTicks::bounds(0, SPACING, 1);
    BoundedTicks::new(Vec::new(), lower, upper)
  }

  /// The window ends at the last tick of the last fetched word, not one spacing short of it.
  #[test]
  fn bounds_cover_whole_words() {
    assert_eq!(BoundedTicks::bounds(0, SPACING, 1), (-15_360, 30_719));
  }

  /// A tick between the last usable tick and the end of the last word is still answered.
  #[test]
  fn tick_in_last_spacing_is_inside() {
    let found = window()
      .next_initialized_tick_within_one_word(30_700, true, SPACING)
      .now_or_never()
      .unwrap();
    assert_eq!(found.unwrap(), (15_360, false));
  }

  /// Lookups and searches past the fetched words fail in both directions.
  #[test]
  fn past_fetched_words_errors() {
    let ticks = window();
    let up = ticks.next_initialized_tick_within_one_word(30_719, false, SPACING);
    assert!(up.now_or_never().unwrap().is_err());
    let beyond = ticks.next_initialized_tick_within_one_word(30_720, true, SPACING);
    assert!(beyond.now_or_never().unwrap().is_err());
    let down = ticks.next_initialized_tick_within_one_word(-15_361, true, SPACING);
    assert!(down.now_or_never().unwrap().is_err());
    assert!(ticks.get_tick(30_720).now_or_never().unwrap().is_err());
  }
}
