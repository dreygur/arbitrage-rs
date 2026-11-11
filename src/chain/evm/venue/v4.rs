//! Uniswap V4 concentrated liquidity pool, quoted locally over a bounded tick window.

use alloy::primitives::aliases::U24;
use alloy::primitives::{Address, B256, U160, keccak256};
use alloy::sol_types::SolValue;
use futures::FutureExt;
use uniswap_v3_sdk::prelude::{MAX_SQRT_RATIO, MIN_SQRT_RATIO, TickIndex};
use uniswap_v4_sdk::prelude::Pool;
use uniswap_v4_sdk::prelude::sdk_core::prelude::{
  Currency, CurrencyAmount, Ether, FractionBase, FromBig, ToBig, Token,
};

use crate::chain::evm::bindings::PoolKey;
use crate::chain::evm::convert::to_id;
use crate::chain::evm::ticks::{BoundedTicks, WORD_TICKS};
use crate::error::{Error, Result};
use crate::types::{Amount, Id, Side};
use crate::venue::Venue;

/// Chain id the SDK currencies are labeled with; the quoting math never reads it.
const SDK_CHAIN_ID: u64 = 1;
/// Fees at or above this many pips are not static LP fees, so the SDK cannot price them.
const MAX_FEE: u32 = 1_000_000;
/// Pips in one basis point.
const PIPS_PER_BPS: u32 = 100;
/// Fractional bits of a Q64.96 square root price.
const Q96_BITS: usize = 96;
/// Pips in a whole, the denominator of every V4 fee.
const PIPS: u32 = 1_000_000;
/// Bits of one direction's protocol fee in slot0's packed value: zero for one in the low
/// twelve, one for zero in the next twelve.
const PROTOCOL_FEE_BITS: u32 = 12;
/// Mask of one direction's protocol fee.
const PROTOCOL_FEE_MASK: u32 = (1 << PROTOCOL_FEE_BITS) - 1;
/// Tick spacing of the V4 fixture.
#[cfg(any(test, feature = "testkit"))]
const FIXTURE_SPACING: i32 = 60;
/// Lower and upper tick of the V4 fixture's one position.
#[cfg(any(test, feature = "testkit"))]
const FIXTURE_RANGE: (i32, i32) = (-600, 600);
/// Liquidity of the V4 fixture's position.
#[cfg(any(test, feature = "testkit"))]
const FIXTURE_LIQUIDITY: u128 = 10_000_000_000;
/// Bitmap words fetched each side of the V4 fixture's current word.
#[cfg(any(test, feature = "testkit"))]
const FIXTURE_WORDS: u16 = 2;
/// Square root price of tick zero, a price of one.
#[cfg(any(test, feature = "testkit"))]
const FIXTURE_SQRT_PRICE: U160 = U160::from_limbs([0, 1 << 32, 0]);

/// Identifies a V4 pool, as the PoolManager's `PoolKey` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V4Key {
  /// Lower currency of the pool, `Address::ZERO` for native.
  pub currency0: Address,
  /// Higher currency of the pool.
  pub currency1: Address,
  /// LP fee in pips (hundredths of a basis point).
  pub fee: u32,
  /// Distance between usable ticks.
  pub tick_spacing: i32,
  /// Hooks contract, zero for a hookless pool.
  pub hooks: Address,
}

impl V4Key {
  /// The key as the Solidity `PoolKey` struct, as the PoolManager and the executor take it.
  pub fn pool_key(&self) -> PoolKey {
    PoolKey {
      currency0: self.currency0,
      currency1: self.currency1,
      fee: U24::saturating_from(self.fee),
      tickSpacing: self.tick_spacing.to_i24(),
      hooks: self.hooks,
    }
  }

  /// Pool id the PoolManager uses: `keccak256(abi.encode(key))`.
  pub fn pool_id(&self) -> B256 {
    keccak256(self.pool_key().abi_encode())
  }
}

/// SDK currency for `address`: native for the zero address, else an ERC20 token.
fn currency(address: Address, decimals: u8) -> Currency {
  if address == Address::ZERO {
    Currency::NativeCurrency(Ether::on_chain(SDK_CHAIN_ID))
  } else {
    Currency::Token(Token::new(
      SDK_CHAIN_ID,
      address,
      decimals,
      None,
      None,
      0,
      0,
    ))
  }
}

/// The protocol fee in pips a swap in direction `side` pays, out of slot0's `packed` value:
/// the low 12 bits for zero for one, the high 12 for one for zero.
pub fn protocol_fee(packed: u32, side: Side) -> u32 {
  match side {
    Side::ZeroForOne => packed & PROTOCOL_FEE_MASK,
    Side::OneForZero => (packed >> PROTOCOL_FEE_BITS) & PROTOCOL_FEE_MASK,
  }
}

/// The fee in pips the PoolManager charges a swap with `protocol` and `lp` fees:
/// `protocol + lp - protocol * lp / 1_000_000`, rounded down as Uniswap's
/// `ProtocolFeeLibrary.calculateSwapFee` does. `protocol` is one direction's 12 bit fee and
/// `lp` an LP fee below a whole, as [`V4Pool::new`] passes them: the `u64` math cannot
/// overflow or go below zero, and the result is under two million.
pub fn swap_fee(protocol: u32, lp: u32) -> u32 {
  let (protocol, lp) = (u64::from(protocol), u64::from(lp));
  let fee = protocol + lp - protocol * lp / u64::from(PIPS);
  u32::try_from(fee).expect("a 12 bit protocol fee plus an LP fee below a whole fits u32")
}

/// Id the core sees for `address`: `base_alias` for native, else the address.
pub fn token_id(address: Address, base_alias: &Id) -> Id {
  if address == Address::ZERO {
    base_alias.clone()
  } else {
    to_id(address)
  }
}

/// Base token amount `liquidity` stands for at `sqrt_price`, in integer math.
///
/// `L * 2^96 / sqrtP` when the base is currency zero (`base_first`), `L * sqrtP / 2^96`
/// otherwise. Discovery uses it before a pool is built, `V4Pool::base_reserve` after.
pub fn virtual_base_reserve(liquidity: u128, sqrt_price: U160, base_first: bool) -> Result<Amount> {
  let liquidity = Amount::from(liquidity);
  let sqrt_price = Amount::from(sqrt_price);
  let q96 = Amount::from(1u64) << Q96_BITS;
  if base_first {
    liquidity
      .checked_mul(q96)
      .and_then(|v| v.checked_div(sqrt_price))
      .ok_or(Error::Overflow)
  } else {
    liquidity
      .checked_mul(sqrt_price)
      .map(|v| v >> Q96_BITS)
      .ok_or(Error::Overflow)
  }
}

/// A V4 pool's slot0, liquidity and tick window, wrapped in the SDK pool that quotes it.
#[derive(Clone, Debug)]
pub struct V4Pool {
  /// Name of the venue the pool belongs to.
  venue: String,
  /// Pool id.
  pool: Id,
  /// Id the core sees for currency zero; native is reported as the base alias.
  token0: Id,
  /// Id the core sees for currency one.
  token1: Id,
  /// Whether the base token is currency zero, which decides the formula of `base_reserve`.
  base_first: bool,
  /// The pool key, kept for its fee and tick spacing.
  key: V4Key,
  /// Slot0's packed protocol fee, zero for most pools.
  protocol_fees: u32,
  /// SDK pool holding price, liquidity, current tick and the tick window, priced at the zero
  /// for one swap fee. Its own pool id follows that fee, so `pool` is the id used everywhere.
  sdk: Pool<BoundedTicks>,
  /// The same pool priced at the one for zero swap fee, present only when that differs from
  /// the zero for one fee; every state change is applied to both.
  one_for_zero: Option<Pool<BoundedTicks>>,
}

impl V4Pool {
  /// Builds a pool from its key, slot0's packed `protocol_fees`, decimals, slot0, liquidity
  /// and tick window.
  ///
  /// `base_alias` is the id reported for native currency (the wrapped native token), and the
  /// side that holds it is the base side. Each direction is priced at the swap fee the
  /// PoolManager charges, the LP fee combined with that direction's protocol fee. Fails for
  /// keys the SDK cannot price (unsorted currencies, a non positive tick spacing or a fee
  /// that is not a static LP fee) and for pools where neither currency is native or
  /// `base_alias`.
  #[allow(
    clippy::too_many_arguments,
    reason = "each argument is a separate piece of pool state read from the chain"
  )]
  pub fn new(
    venue: String,
    pool: Id,
    base_alias: Id,
    key: V4Key,
    protocol_fees: u32,
    decimals: (u8, u8),
    sqrt_price_x96: U160,
    liquidity: u128,
    tick: i32,
    ticks: BoundedTicks,
  ) -> Result<Self> {
    if key.currency0 >= key.currency1 {
      return Err(Error::Config(format!(
        "v4 key currencies {} and {} are not sorted",
        key.currency0, key.currency1
      )));
    }
    if key.tick_spacing <= 0 {
      return Err(Error::Config(format!(
        "v4 tick spacing {} is not positive",
        key.tick_spacing
      )));
    }
    if key.fee >= MAX_FEE {
      return Err(Error::Config(format!(
        "v4 fee {} is not a static fee",
        key.fee
      )));
    }
    let priced = |side: Side| -> Result<Pool<BoundedTicks>> {
      let fee = swap_fee(protocol_fee(protocol_fees, side), key.fee);
      if fee >= MAX_FEE {
        return Err(Error::Config(format!(
          "v4 swap fee {fee} is not below a whole"
        )));
      }
      let mut sdk = Pool::new_with_tick_data_provider(
        currency(key.currency0, decimals.0),
        currency(key.currency1, decimals.1),
        U24::from(fee),
        key.tick_spacing,
        key.hooks,
        sqrt_price_x96,
        liquidity,
        ticks.clone(),
      )
      .map_err(|e| Error::Config(format!("v4 pool {pool}: {e}")))?;
      sdk.tick_current = tick;
      Ok(sdk)
    };
    let sdk = priced(Side::ZeroForOne)?;
    let one_for_zero = if protocol_fee(protocol_fees, Side::ZeroForOne)
      == protocol_fee(protocol_fees, Side::OneForZero)
    {
      None
    } else {
      Some(priced(Side::OneForZero)?)
    };
    let token0 = token_id(key.currency0, &base_alias);
    let token1 = token_id(key.currency1, &base_alias);
    if token0 != base_alias && token1 != base_alias {
      return Err(Error::Config(format!(
        "v4 pool {pool} holds neither native nor the base token"
      )));
    }
    Ok(Self {
      venue,
      pool,
      base_first: token0 == base_alias,
      token0,
      token1,
      key,
      protocol_fees,
      sdk,
      one_for_zero,
    })
  }

  /// Every SDK pool this pool holds, so a state change reaches each direction's pricing.
  fn sdk_pools(&mut self) -> impl Iterator<Item = &mut Pool<BoundedTicks>> {
    std::iter::once(&mut self.sdk).chain(self.one_for_zero.as_mut())
  }

  /// The SDK pool priced for a swap in direction `side`.
  fn priced_for(&self, side: Side) -> &Pool<BoundedTicks> {
    match (side, &self.one_for_zero) {
      (Side::OneForZero, Some(pool)) => pool,
      _ => &self.sdk,
    }
  }

  /// Applies a `Swap` event: the new price, in range liquidity and current tick.
  pub fn apply_swap(&mut self, sqrt_price: U160, liquidity: u128, tick: i32) {
    for sdk in self.sdk_pools() {
      sdk.sqrt_price_x96 = sqrt_price;
      sdk.liquidity = liquidity;
      sdk.tick_current = tick;
    }
  }

  /// Applies a `ModifyLiquidity` event of `delta` over `[tick_lower, tick_upper)`.
  ///
  /// Ends outside the window are not tracked. The pool is left unchanged on overflow.
  pub fn apply_modify(&mut self, tick_lower: i32, tick_upper: i32, delta: i128) -> Result<()> {
    let mut ticks = self.sdk.tick_data_provider.clone();
    ticks.modify(tick_lower, delta, delta)?;
    ticks.modify(
      tick_upper,
      delta,
      delta.checked_neg().ok_or(Error::Overflow)?,
    )?;
    let tick = self.sdk.tick_current;
    let mut liquidity = self.sdk.liquidity;
    if tick_lower <= tick && tick < tick_upper {
      liquidity = liquidity.checked_add_signed(delta).ok_or(Error::Overflow)?;
    }
    for sdk in self.sdk_pools() {
      sdk.tick_data_provider = ticks.clone();
      sdk.liquidity = liquidity;
    }
    Ok(())
  }

  /// Replaces the tick window with a freshly fetched one.
  pub fn replace_window(&mut self, ticks: BoundedTicks) {
    for sdk in self.sdk_pools() {
      sdk.tick_data_provider = ticks.clone();
    }
  }

  /// Whether the current tick is within one bitmap word of the window's edge.
  ///
  /// Only a refetch trigger: quotes stay correct until they would leave the window.
  pub fn near_edge(&self) -> bool {
    let span = WORD_TICKS * self.key.tick_spacing;
    let ticks = &self.sdk.tick_data_provider;
    let tick = self.sdk.tick_current;
    tick - ticks.lower() < span || ticks.upper() - tick < span
  }

  /// Current tick, the center a refetched window is built around.
  pub fn tick(&self) -> i32 {
    self.sdk.tick_current
  }

  /// Base token amount the in range liquidity stands for, in integer math.
  pub fn base_reserve(&self) -> Result<Amount> {
    virtual_base_reserve(self.sdk.liquidity, self.sdk.sqrt_price_x96, self.base_first)
  }
}

impl Venue for V4Pool {
  /// Pool id.
  fn pool(&self) -> &Id {
    &self.pool
  }

  /// Both tokens, currency zero first, native reported as the base alias.
  fn tokens(&self) -> [&Id; 2] {
    [&self.token0, &self.token1]
  }

  /// Exact input quote from the SDK over the tick window, at the swap fee of direction
  /// `side`.
  ///
  /// The tick provider answers from memory, so the SDK's future completes on its first poll.
  /// A pending future, an SDK error or a swap that would leave the window is
  /// `InsufficientLiquidity`; inputs of 2^255 or more do not fit the SDK's signed amount.
  fn quote_out(&self, amount_in: Amount, side: Side) -> Result<Amount> {
    if amount_in.bit(255) {
      return Err(Error::Overflow);
    }
    let sdk = self.priced_for(side);
    let price = sdk.sqrt_price_x96;
    let (currency, at_limit) = match side {
      Side::ZeroForOne => (&sdk.currency0, price <= MIN_SQRT_RATIO + U160::from(1u8)),
      Side::OneForZero => (&sdk.currency1, price >= MAX_SQRT_RATIO - U160::from(1u8)),
    };
    if at_limit {
      return Err(Error::InsufficientLiquidity);
    }
    let input = CurrencyAmount::from_raw_amount(currency.clone(), amount_in.to_big_int())
      .map_err(|_| Error::Overflow)?;
    match sdk.get_output_amount(&input, None).now_or_never() {
      Some(Ok((output, _))) => Ok(Amount::from_big_int(output.quotient())),
      _ => Err(Error::InsufficientLiquidity),
    }
  }

  /// Venue name, LP fee, price, liquidity and tick, with slot0's packed protocol fee when the
  /// pool charges one.
  fn describe(&self) -> String {
    let protocol = if self.protocol_fees == 0 {
      String::new()
    } else {
      format!(" pf={:#x}", self.protocol_fees)
    };
    format!(
      "{} {}bps{protocol} p={} L={} t={}",
      self.venue,
      self.key.fee / PIPS_PER_BPS,
      self.sdk.sqrt_price_x96,
      self.sdk.liquidity,
      self.sdk.tick_current
    )
  }
}

/// Hand built native ETH / `quote` V4 pool for tests and benches: fee 3000, spacing 60, tick
/// zero, one position across ticks -600..600, and native reported as the id `base`.
#[cfg(any(test, feature = "testkit"))]
pub fn v4_fixture_quoting(quote: Address) -> V4Pool {
  let key = V4Key {
    currency0: Address::ZERO,
    currency1: quote,
    fee: 3000,
    tick_spacing: FIXTURE_SPACING,
    hooks: Address::ZERO,
  };
  let (lower, upper) = BoundedTicks::bounds(0, FIXTURE_SPACING, FIXTURE_WORDS);
  let net = FIXTURE_LIQUIDITY as i128;
  let ticks = vec![
    uniswap_v3_sdk::prelude::Tick::new(FIXTURE_RANGE.0, FIXTURE_LIQUIDITY, net),
    uniswap_v3_sdk::prelude::Tick::new(FIXTURE_RANGE.1, FIXTURE_LIQUIDITY, -net),
  ];
  V4Pool::new(
    "uniswap_v4".into(),
    Id::new(key.pool_id().to_vec()),
    Id::new(b"base".to_vec()),
    key,
    0,
    (18, 18),
    FIXTURE_SQRT_PRICE,
    FIXTURE_LIQUIDITY,
    0,
    BoundedTicks::new(ticks, lower, upper),
  )
  .expect("fixture key is valid")
}

/// The V4 fixture quoting the token at `0x11..11`.
#[cfg(any(test, feature = "testkit"))]
pub fn v4_fixture() -> V4Pool {
  v4_fixture_quoting(Address::repeat_byte(0x11))
}

#[cfg(test)]
mod tests {
  use super::*;
  use uniswap_v3_sdk::prelude::{Tick, TickListDataProvider, get_sqrt_ratio_at_tick};

  /// Tick spacing of the test pools.
  const SPACING: i32 = 60;
  /// Liquidity of one test position.
  const L: u128 = 1_000_000_000_000_000_000;

  /// The non native currency of the test pools.
  fn token() -> Address {
    Address::repeat_byte(0x11)
  }

  /// Id reported for native currency.
  fn weth() -> Id {
    Id::new(vec![0xee; 20])
  }

  /// Key of the native ETH pool at fee 3000 and spacing 60.
  fn key() -> V4Key {
    V4Key {
      currency0: Address::ZERO,
      currency1: token(),
      fee: 3000,
      tick_spacing: SPACING,
      hooks: Address::ZERO,
    }
  }

  /// Ticks of one position of `liquidity` over `[lower, upper)`.
  fn position(lower: i32, upper: i32, liquidity: u128) -> Vec<Tick> {
    let net = liquidity as i128;
    vec![
      Tick::new(lower, liquidity, net),
      Tick::new(upper, liquidity, -net),
    ]
  }

  /// Square root price at `tick`.
  fn price_at(tick: i32) -> U160 {
    get_sqrt_ratio_at_tick(tick.to_i24()).unwrap()
  }

  /// Pool at `tick` with `ticks`, a window of `words` words, in range `liquidity` and the
  /// packed `protocol_fees` slot0 reports.
  fn build_charging(
    ticks: Vec<Tick>,
    liquidity: u128,
    tick: i32,
    words: u16,
    protocol_fees: u32,
  ) -> V4Pool {
    let (lower, upper) = BoundedTicks::bounds(tick, SPACING, words);
    V4Pool::new(
      "uniswap_v4".into(),
      Id::new(key().pool_id().to_vec()),
      weth(),
      key(),
      protocol_fees,
      (18, 18),
      price_at(tick),
      liquidity,
      tick,
      BoundedTicks::new(ticks, lower, upper),
    )
    .unwrap()
  }

  /// Pool at `tick` with `ticks`, a window of `words` words, in range `liquidity` and no
  /// protocol fee.
  fn build(ticks: Vec<Tick>, liquidity: u128, tick: i32, words: u16) -> V4Pool {
    build_charging(ticks, liquidity, tick, words, 0)
  }

  /// SDK pool over the same ticks with no window, the reference our quotes must match.
  fn reference(ticks: Vec<Tick>, liquidity: u128, tick: i32) -> Pool<TickListDataProvider> {
    reference_at_fee(ticks, liquidity, tick, 3000)
  }

  /// [`reference`] priced at `fee` pips.
  fn reference_at_fee(
    ticks: Vec<Tick>,
    liquidity: u128,
    tick: i32,
    fee: u32,
  ) -> Pool<TickListDataProvider> {
    Pool::new_with_tick_data_provider(
      Currency::NativeCurrency(Ether::on_chain(SDK_CHAIN_ID)),
      Currency::Token(Token::new(SDK_CHAIN_ID, token(), 18, None, None, 0, 0)),
      U24::from(fee),
      SPACING,
      Address::ZERO,
      price_at(tick),
      liquidity,
      TickListDataProvider::new(ticks, SPACING),
    )
    .unwrap()
  }

  /// Reference quote of `amount_in` in direction `side`.
  fn reference_quote(
    pool: &Pool<TickListDataProvider>,
    amount_in: u128,
    side: Side,
  ) -> Result<Amount> {
    let currency = match side {
      Side::ZeroForOne => pool.currency0.clone(),
      Side::OneForZero => pool.currency1.clone(),
    };
    let input =
      CurrencyAmount::from_raw_amount(currency, Amount::from(amount_in).to_big_int()).unwrap();
    match pool.get_output_amount(&input, None).now_or_never() {
      Some(Ok((out, _))) => Ok(Amount::from_big_int(out.quotient())),
      _ => Err(Error::InsufficientLiquidity),
    }
  }

  /// Quotes inside the window equal the SDK's over the same ticks without a window.
  #[test]
  fn quote_matches_unbounded_reference() {
    let pool = build(position(-600, 600, L), L, 0, 2);
    let reference = reference(position(-600, 600, L), L, 0);
    for side in [Side::ZeroForOne, Side::OneForZero] {
      for amount in [1u128, 1_000_000_000_000_000, 20_000_000_000_000_000] {
        let ours = pool.quote_out(Amount::from(amount), side).unwrap();
        assert_eq!(ours, reference_quote(&reference, amount, side).unwrap());
        assert!(amount < 2 || ours > Amount::ZERO);
      }
    }
  }

  /// The swap fee is `protocol + lp - protocol * lp / 1_000_000`, with the zero for one
  /// protocol fee in the low 12 bits of slot0's value and the one for zero fee in the high 12.
  #[test]
  fn swap_fee_combines_protocol_and_lp() {
    assert_eq!(protocol_fee(0x032032, Side::ZeroForOne), 50);
    assert_eq!(protocol_fee(0x032032, Side::OneForZero), 50);
    assert_eq!(swap_fee(50, 500), 550);
    assert_eq!(swap_fee(1000, 10_000), 10_990);
    assert_eq!(swap_fee(125, 500), 625);
    assert_eq!(swap_fee(0, 3000), 3000);
    let one_sided = 0x0003e8;
    assert_eq!(protocol_fee(one_sided, Side::ZeroForOne), 1000);
    assert_eq!(protocol_fee(one_sided, Side::OneForZero), 0);
    let both = (1000 << 12) | 100;
    assert_eq!(protocol_fee(both, Side::ZeroForOne), 100);
    assert_eq!(protocol_fee(both, Side::OneForZero), 1000);
  }

  /// A pool with a protocol fee quotes each direction as the SDK does at that direction's
  /// combined fee, whether the fee is the same both ways, one sided or different per side.
  #[test]
  fn protocol_fee_pool_quotes_at_swap_fee() {
    let amount = 20_000_000_000_000_000u128;
    for packed in [0x1f41f4, 0x0003e8, (1000 << 12) | 100] {
      let pool = build_charging(position(-600, 600, L), L, 0, 2, packed);
      for side in [Side::ZeroForOne, Side::OneForZero] {
        let fee = swap_fee(protocol_fee(packed, side), 3000);
        let reference = reference_at_fee(position(-600, 600, L), L, 0, fee);
        assert_eq!(
          pool.quote_out(Amount::from(amount), side).unwrap(),
          reference_quote(&reference, amount, side).unwrap(),
          "packed {packed:#x} {side:?}"
        );
      }
    }
    let plain = build(position(-600, 600, L), L, 0, 2);
    let charged = build_charging(position(-600, 600, L), L, 0, 2, 0x0003e8);
    let small = Amount::from(amount);
    assert!(
      charged.quote_out(small, Side::ZeroForOne).unwrap()
        < plain.quote_out(small, Side::ZeroForOne).unwrap()
    );
    assert_eq!(
      charged.quote_out(small, Side::OneForZero).unwrap(),
      plain.quote_out(small, Side::OneForZero).unwrap()
    );
  }

  /// State changes reach both directions of a pool whose fees differ per side.
  #[test]
  fn protocol_fee_pool_follows_swaps() {
    let packed = 0x0003e8;
    let mut pool = build_charging(position(-600, 600, L), L, 0, 2, packed);
    pool.apply_swap(price_at(-300), L, -300);
    let fresh = build_charging(position(-600, 600, L), L, -300, 2, packed);
    let amount = Amount::from(1_000_000_000_000_000u64);
    for side in [Side::ZeroForOne, Side::OneForZero] {
      assert_eq!(
        pool.quote_out(amount, side).unwrap(),
        fresh.quote_out(amount, side).unwrap()
      );
    }
  }

  /// One position past both edges of a one word window, ticks -15360..=30719.
  fn wide_position() -> Vec<Tick> {
    position(-30_000, 60_000, L)
  }

  /// Inputs that move the price past the one word window but stay inside `wide_position`,
  /// down past -15360 and up past 30719.
  const PAST_WINDOW: [(Side, u128); 2] = [
    (Side::ZeroForOne, 2_000_000_000_000_000_000),
    (Side::OneForZero, 5_000_000_000_000_000_000),
  ];

  /// A quote that would leave the window errors in either direction, even where the full tick
  /// list fills it.
  #[test]
  fn quote_past_window_errors() {
    let pool = build(wide_position(), L, 0, 1);
    let reference = reference(wide_position(), L, 0);
    for (side, amount) in PAST_WINDOW {
      assert!(reference_quote(&reference, amount, side).is_ok());
      assert!(matches!(
        pool.quote_out(Amount::from(amount), side),
        Err(Error::InsufficientLiquidity)
      ));
    }
  }

  /// Native currency is reported as the base alias.
  #[test]
  fn native_reports_weth() {
    let pool = build(position(-600, 600, L), L, 0, 2);
    assert_eq!(pool.tokens(), [&weth(), &to_id(token())]);
  }

  /// A swap event moves the price the quotes start from.
  #[test]
  fn apply_swap_moves_price() {
    let mut pool = build(position(-600, 600, L), L, 0, 2);
    let amount = Amount::from(1_000_000_000_000_000u64);
    let before = pool.quote_out(amount, Side::OneForZero).unwrap();
    pool.apply_swap(price_at(-300), L, -300);
    let after = pool.quote_out(amount, Side::OneForZero).unwrap();
    let fresh = build(position(-600, 600, L), L, -300, 2);
    assert!(after > before);
    assert_eq!(after, fresh.quote_out(amount, Side::OneForZero).unwrap());
    assert!(pool.describe().ends_with("t=-300"));
  }

  /// Adding and removing positions gives the same quotes as a pool built with the result.
  ///
  /// The amounts cross ticks 600 or -600 but not 1200 or -1200, so every changed tick is used.
  #[test]
  fn apply_modify_matches_fresh_pool() {
    let crossing = Amount::from(80_000_000_000_000_000u64);
    let mut pool = build(position(-600, 600, L), L, 0, 2);
    pool.apply_modify(-1200, 1200, L as i128).unwrap();
    let mut both = position(-1200, 1200, L);
    both.extend(position(-600, 600, L));
    let fresh = build(both, 2 * L, 0, 2);
    for side in [Side::ZeroForOne, Side::OneForZero] {
      assert_eq!(
        pool.quote_out(crossing, side).unwrap(),
        fresh.quote_out(crossing, side).unwrap()
      );
    }
    let within = Amount::from(50_000_000_000_000_000u64);
    pool.apply_modify(-600, 600, -(L as i128)).unwrap();
    let wide = build(position(-1200, 1200, L), L, 0, 2);
    for side in [Side::ZeroForOne, Side::OneForZero] {
      assert_eq!(
        pool.quote_out(within, side).unwrap(),
        wide.quote_out(within, side).unwrap()
      );
    }
    assert!(pool.quote_out(crossing, Side::ZeroForOne).is_err());
  }

  /// A position starting below the current tick adds no active liquidity, only its ticks.
  #[test]
  fn apply_modify_outside_range_keeps_active_liquidity() {
    let mut pool = build(position(-600, 600, L), L, 0, 2);
    pool.apply_modify(600, 1200, L as i128).unwrap();
    assert!(pool.describe().contains(" L=1000000000000000000 "));
    let net = L as i128;
    let merged = vec![
      Tick::new(-600, L, net),
      Tick::new(600, 2 * L, 0),
      Tick::new(1200, L, -net),
    ];
    let fresh = build(merged, L, 0, 2);
    let crossing = Amount::from(50_000_000_000_000_000u64);
    assert_eq!(
      pool.quote_out(crossing, Side::OneForZero).unwrap(),
      fresh.quote_out(crossing, Side::OneForZero).unwrap()
    );
  }

  /// A wider window lets quotes through in both directions that the narrow one refused.
  #[test]
  fn replace_window_extends_quotes() {
    let mut pool = build(wide_position(), L, 0, 1);
    let (lower, upper) = BoundedTicks::bounds(0, SPACING, 2);
    pool.replace_window(BoundedTicks::new(wide_position(), lower, upper));
    let reference = reference(wide_position(), L, 0);
    for (side, amount) in PAST_WINDOW {
      assert_eq!(
        pool.quote_out(Amount::from(amount), side).unwrap(),
        reference_quote(&reference, amount, side).unwrap()
      );
    }
  }

  /// A pool where neither currency is native or the base alias is refused.
  #[test]
  fn pool_without_base_rejected() {
    let (lower, upper) = BoundedTicks::bounds(0, SPACING, 2);
    let result = V4Pool::new(
      "uniswap_v4".into(),
      Id::new(vec![3; 32]),
      weth(),
      V4Key {
        currency0: token(),
        currency1: Address::repeat_byte(0x22),
        ..key()
      },
      0,
      (18, 18),
      price_at(0),
      L,
      0,
      BoundedTicks::new(Vec::new(), lower, upper),
    );
    assert!(matches!(result, Err(Error::Config(_))));
  }

  /// The edge flag is raised once the tick comes within one word of the window's bound.
  #[test]
  fn near_edge_detected() {
    let mut pool = build(position(-600, 600, L), L, 0, 2);
    assert!(!pool.near_edge());
    pool.apply_swap(price_at(-15_400), 0, -15_400);
    assert!(pool.near_edge());
  }

  /// The base reserve is `L / P` on the base side zero and `L * P` on the base side one.
  #[test]
  fn base_reserve_from_liquidity() {
    let sqrt = U160::from(1u64) << 97;
    let tick = 13_862;
    let (lower, upper) = BoundedTicks::bounds(tick, SPACING, 2);
    let native = V4Pool::new(
      "uniswap_v4".into(),
      Id::new(vec![1; 32]),
      weth(),
      key(),
      0,
      (18, 18),
      sqrt,
      L,
      tick,
      BoundedTicks::new(Vec::new(), lower, upper),
    )
    .unwrap();
    assert_eq!(native.base_reserve().unwrap(), Amount::from(L / 2));
    let wrapped = Address::repeat_byte(0xee);
    let erc20 = V4Pool::new(
      "uniswap_v4".into(),
      Id::new(vec![2; 32]),
      to_id(wrapped),
      V4Key {
        currency0: token(),
        currency1: wrapped,
        ..key()
      },
      0,
      (6, 18),
      sqrt,
      L,
      tick,
      BoundedTicks::new(Vec::new(), lower, upper),
    )
    .unwrap();
    assert_eq!(erc20.base_reserve().unwrap(), Amount::from(2 * L));
  }

  /// The description names the venue, fee, price, liquidity and tick.
  #[test]
  fn describe_names_venue() {
    let pool = build(position(-600, 600, L), L, 0, 2);
    assert_eq!(
      pool.describe(),
      "uniswap_v4 30bps p=79228162514264337593543950336 L=1000000000000000000 t=0"
    );
  }

  /// The shared fixture quotes both ways at tick zero.
  #[test]
  fn fixture_quotes_both_ways() {
    let pool = crate::testkit::v4_fixture();
    assert_eq!(
      pool.describe(),
      "uniswap_v4 30bps p=79228162514264337593543950336 L=10000000000 t=0"
    );
    for side in [Side::ZeroForOne, Side::OneForZero] {
      assert!(pool.quote_out(Amount::from(1_000_000u64), side).unwrap() > Amount::ZERO);
    }
  }

  /// The pool id matches the SDK's for the same key.
  #[test]
  fn pool_id_matches_sdk() {
    let expected = Pool::get_pool_id(
      &Currency::NativeCurrency(Ether::on_chain(SDK_CHAIN_ID)),
      &Currency::Token(Token::new(SDK_CHAIN_ID, token(), 18, None, None, 0, 0)),
      U24::from(3000),
      SPACING,
      Address::ZERO,
    )
    .unwrap();
    assert_eq!(key().pool_id(), expected);
  }
}
