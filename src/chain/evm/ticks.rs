//! Initialized ticks of a V4 pool within a bounded window around its current tick.

use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::aliases::I24;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use uniswap_v3_sdk::prelude::{
  Error as SdkError, Tick, TickDataProvider, TickIndex, TickListError,
};

use crate::chain::evm::bindings::{Call3, IMulticall3, IStateView, Result3};
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

/// Decodes one Multicall3 result as the return value of call `C`.
fn decode<C: SolCall>(result: &Result3) -> Result<C::Return> {
  if !result.success {
    return Err(Error::Contract(format!(
      "{} failed in multicall",
      C::SIGNATURE
    )));
  }
  C::abi_decode_returns(&result.returnData).map_err(|e| Error::Contract(e.to_string()))
}

/// Sends `calls` to StateView through one Multicall3 `aggregate3` at `block`, failing on any
/// reverted call.
async fn aggregate<P: Provider>(
  provider: &P,
  multicall: Address,
  state_view: Address,
  block: BlockId,
  calls: Vec<Vec<u8>>,
) -> Result<Vec<Result3>> {
  let calls = calls
    .into_iter()
    .map(|data| Call3 {
      target: state_view,
      allowFailure: false,
      callData: data.into(),
    })
    .collect();
  Ok(
    IMulticall3::new(multicall, provider)
      .aggregate3(calls)
      .block(block)
      .call()
      .await?,
  )
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

/// Reads the initialized ticks of the pool `key` names within `words` bitmap words each side of
/// the word holding `tick`, as of `block`.
///
/// Makes two Multicall3 `aggregate3` calls to StateView, both pinned to `block` so the bitmap
/// and the tick liquidity come from the same state: one for every bitmap word in range, then
/// one for the liquidity of every set bit.
pub async fn fetch_window<P: Provider>(
  provider: &P,
  multicall: Address,
  state_view: Address,
  block: BlockId,
  key: &V4Key,
  tick: i32,
  words: u16,
) -> Result<BoundedTicks> {
  let pool_id = key.pool_id();
  let tick_spacing = key.tick_spacing;
  let (first, last) = word_range(tick, tick_spacing, words);
  let (lower, upper) = BoundedTicks::bounds(tick, tick_spacing, words);
  let bitmap_calls = (first..=last)
    .map(|word| {
      IStateView::getTickBitmapCall {
        poolId: pool_id,
        tick: word,
      }
      .abi_encode()
    })
    .collect();
  let bitmaps = aggregate(provider, multicall, state_view, block, bitmap_calls).await?;
  let mut indices = Vec::new();
  for (word, result) in (first..=last).zip(&bitmaps) {
    let bitmap: U256 = decode::<IStateView::getTickBitmapCall>(result)?;
    for bit in 0..WORD_TICKS {
      if bitmap.bit(bit as usize) {
        indices.push((i32::from(word) * WORD_TICKS + bit) * tick_spacing);
      }
    }
  }
  if indices.is_empty() {
    return Ok(BoundedTicks::new(Vec::new(), lower, upper));
  }
  let liquidity_calls = indices
    .iter()
    .map(|&index| {
      let tick = I24::try_from(index).map_err(|_| Error::Overflow)?;
      Ok(
        IStateView::getTickLiquidityCall {
          poolId: pool_id,
          tick,
        }
        .abi_encode(),
      )
    })
    .collect::<Result<Vec<_>>>()?;
  let liquidity = aggregate(provider, multicall, state_view, block, liquidity_calls).await?;
  let mut ticks = Vec::with_capacity(indices.len());
  for (index, result) in indices.into_iter().zip(&liquidity) {
    let values = decode::<IStateView::getTickLiquidityCall>(result)?;
    ticks.push(Tick {
      index,
      liquidity_gross: values.liquidityGross,
      liquidity_net: values.liquidityNet,
    });
  }
  Ok(BoundedTicks::new(ticks, lower, upper))
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
