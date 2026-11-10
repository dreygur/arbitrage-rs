//! Pools the adapter tracks, found from the configured token list or by scanning a V2 factory.

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;

use alloy::eips::BlockId;
use alloy::primitives::{Address, U160, U256};
use alloy::providers::Provider;

use crate::chain::evm::bindings::{Call3, IStateView, IUniswapV2Factory, IUniswapV2Pair};
use crate::chain::evm::config::{EvmConfig, Family};
use crate::chain::evm::convert::to_id;
use crate::chain::evm::multicall::{aggregate, decode, read};
use crate::chain::evm::venue::v4::{V4Key, token_id, virtual_base_reserve};
use crate::config::NetworkConfig;
use crate::discovery::{PoolInfo, select};
use crate::error::{Error, Result};
use crate::types::{Amount, Id};

/// Hookless V4 fee (pips) and tick spacing pairs probed for each token.
const PROBES: [(u32, i32); 4] = [(100, 1), (500, 10), (3000, 60), (10_000, 200)];
/// Pips in one basis point.
const PIPS_PER_BPS: u32 = 100;

/// A pool the adapter watches and trades on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracked {
  /// A V2 pair.
  V2 {
    /// Index of the venue in `EvmConfig::venues`.
    venue: usize,
    /// Pair address.
    pair: Address,
    /// Lower token of the pair.
    token0: Address,
    /// Higher token of the pair.
    token1: Address,
    /// Swap fee in basis points.
    fee_bps: u16,
  },
  /// A V4 pool.
  V4 {
    /// Index of the venue in `EvmConfig::venues`.
    venue: usize,
    /// The pool key.
    key: V4Key,
  },
}

impl Tracked {
  /// Id the core knows the pool by: the pair address, or the V4 pool id.
  pub fn id(&self) -> Id {
    match self {
      Self::V2 { pair, .. } => to_id(*pair),
      Self::V4 { key, .. } => Id::new(key.pool_id().to_vec()),
    }
  }

  /// Index of the venue the pool belongs to.
  pub fn venue(&self) -> usize {
    match self {
      Self::V2 { venue, .. } | Self::V4 { venue, .. } => *venue,
    }
  }
}

/// What the engine needs to encode one leg of a trade on a pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegSpec {
  /// Pool family, which picks the executor's leg handler.
  pub family: Family,
  /// Pair address, for a V2 pool.
  pub pair: Option<Address>,
  /// Pool key, for a V4 pool.
  pub key: Option<V4Key>,
  /// Real token zero (zero address for native), which decides `zeroForOne`.
  pub token0: Address,
  /// Swap fee in basis points.
  pub fee_bps: u16,
}

/// Leg specs for every tracked pool, keyed by pool id.
pub fn leg_specs(tracked: &[Tracked]) -> HashMap<Id, LegSpec> {
  tracked
    .iter()
    .map(|pool| {
      let spec = match *pool {
        Tracked::V2 {
          pair,
          token0,
          fee_bps,
          ..
        } => LegSpec {
          family: Family::V2,
          pair: Some(pair),
          key: None,
          token0,
          fee_bps,
        },
        Tracked::V4 { key, .. } => LegSpec {
          family: Family::V4,
          pair: None,
          key: Some(key),
          token0: key.currency0,
          fee_bps: u16::try_from(key.fee / PIPS_PER_BPS).unwrap_or(u16::MAX),
        },
      };
      (pool.id(), spec)
    })
    .collect()
}

/// Parses the address in `text`, naming the config `key` it came from on error.
fn parse_address(key: &str, text: &str) -> Result<Address> {
  Address::from_str(text).map_err(|e| Error::Config(format!("{key}: {text}: {e}")))
}

/// `a` and `b` in ascending order, as both V2 and V4 order a pool's tokens.
fn sorted(a: Address, b: Address) -> (Address, Address) {
  if a < b { (a, b) } else { (b, a) }
}

/// A pool a probe found, waiting for its base reserve.
struct Candidate {
  /// The pool.
  tracked: Tracked,
  /// Whether the base token (or native) is token zero.
  base_first: bool,
  /// Square root price from slot0, for a V4 pool.
  sqrt_price: U160,
}

/// The probe calls for `token` on every venue, each paired with the pool it would find.
///
/// V2 venues get `getPair`; V4 venues get `getSlot0` for every probe key against native and
/// against `base`. The V2 pair address is filled in from the answer.
fn probes(evm: &EvmConfig, base: Address, token: Address) -> Vec<(Tracked, Call3)> {
  let mut out = Vec::new();
  for (venue, config) in evm.venues.iter().enumerate() {
    match config.family {
      Family::V2 => {
        let (token0, token1) = sorted(base, token);
        let call = read(
          config.address,
          &IUniswapV2Factory::getPairCall {
            tokenA: base,
            tokenB: token,
          },
        );
        let tracked = Tracked::V2 {
          venue,
          pair: Address::ZERO,
          token0,
          token1,
          fee_bps: config.fee_bps.unwrap_or_default(),
        };
        out.push((tracked, call));
      }
      Family::V4 => {
        for (fee, tick_spacing) in PROBES {
          for side in [Address::ZERO, base] {
            let (currency0, currency1) = sorted(side, token);
            let key = V4Key {
              currency0,
              currency1,
              fee,
              tick_spacing,
              hooks: Address::ZERO,
            };
            let call = read(
              evm.state_view,
              &IStateView::getSlot0Call {
                poolId: key.pool_id(),
              },
            );
            out.push((Tracked::V4 { venue, key }, call));
          }
        }
      }
    }
  }
  out
}

/// Probes every venue for pools pairing `base` with each of `tokens`, in one batch.
///
/// A V2 pool is kept when the factory knows the pair, a V4 pool when its price is set,
/// protocol fee or not: the venue prices that fee per direction.
async fn probe<P: Provider>(
  provider: &P,
  evm: &EvmConfig,
  base: Address,
  tokens: &[Address],
) -> Result<Vec<Candidate>> {
  let mut pools = Vec::new();
  let mut calls = Vec::new();
  for &token in tokens.iter().filter(|&&t| t != base) {
    for (tracked, call) in probes(evm, base, token) {
      calls.push(call);
      pools.push(tracked);
    }
  }
  let results = aggregate(provider, evm.multicall, BlockId::latest(), calls).await?;
  let mut found = Vec::new();
  for (tracked, result) in pools.into_iter().zip(&results) {
    match tracked {
      Tracked::V2 {
        venue,
        token0,
        token1,
        fee_bps,
        ..
      } => {
        let Ok(pair) = decode::<IUniswapV2Factory::getPairCall>(result) else {
          continue;
        };
        if pair == Address::ZERO {
          continue;
        }
        found.push(Candidate {
          tracked: Tracked::V2 {
            venue,
            pair,
            token0,
            token1,
            fee_bps,
          },
          base_first: token0 == base,
          sqrt_price: U160::ZERO,
        });
      }
      Tracked::V4 { key, .. } => {
        let Ok(slot0) = decode::<IStateView::getSlot0Call>(result) else {
          continue;
        };
        if slot0.sqrtPriceX96.is_zero() {
          continue;
        }
        found.push(Candidate {
          tracked,
          base_first: key.currency0 == Address::ZERO || key.currency0 == base,
          sqrt_price: slot0.sqrtPriceX96,
        });
      }
    }
  }
  Ok(found)
}

/// Base token reserve of every candidate, read in one batch; a failed read or a V4 virtual
/// reserve that overflows counts as empty, so that pool falls below the floor instead of
/// failing discovery.
async fn base_reserves<P: Provider>(
  provider: &P,
  evm: &EvmConfig,
  candidates: &[Candidate],
) -> Result<Vec<Amount>> {
  let calls = candidates
    .iter()
    .map(|c| match c.tracked {
      Tracked::V2 { pair, .. } => read(pair, &IUniswapV2Pair::getReservesCall {}),
      Tracked::V4 { key, .. } => read(
        evm.state_view,
        &IStateView::getLiquidityCall {
          poolId: key.pool_id(),
        },
      ),
    })
    .collect();
  let results = aggregate(provider, evm.multicall, BlockId::latest(), calls).await?;
  let mut reserves = Vec::with_capacity(candidates.len());
  for (c, result) in candidates.iter().zip(&results) {
    let reserve = match c.tracked {
      Tracked::V2 { .. } => decode::<IUniswapV2Pair::getReservesCall>(result)
        .map(|r| {
          let reserve = if c.base_first { r.reserve0 } else { r.reserve1 };
          Amount::from(reserve.to::<u128>())
        })
        .unwrap_or_default(),
      Tracked::V4 { .. } => match decode::<IStateView::getLiquidityCall>(result) {
        Ok(liquidity) => {
          virtual_base_reserve(liquidity, c.sqrt_price, c.base_first).unwrap_or_default()
        }
        Err(_) => Amount::ZERO,
      },
    };
    reserves.push(reserve);
  }
  Ok(reserves)
}

/// The pools pairing `base` with any of `tokens` that pass `discovery::select`.
async fn lookup<P: Provider>(
  provider: &P,
  evm: &EvmConfig,
  base: Address,
  tokens: &[Address],
  min_base_reserve: Amount,
) -> Result<Vec<Tracked>> {
  let candidates = probe(provider, evm, base, tokens).await?;
  let reserves = base_reserves(provider, evm, &candidates).await?;
  let base_id = to_id(base);
  let mut by_id = HashMap::new();
  let mut infos = Vec::with_capacity(candidates.len());
  for (c, base_reserve) in candidates.into_iter().zip(reserves) {
    let (token0, token1) = match c.tracked {
      Tracked::V2 { token0, token1, .. } => (to_id(token0), to_id(token1)),
      Tracked::V4 { key, .. } => (
        token_id(key.currency0, &base_id),
        token_id(key.currency1, &base_id),
      ),
    };
    let pool = c.tracked.id();
    infos.push(PoolInfo {
      venue: c.tracked.venue(),
      pool: pool.clone(),
      token0,
      token1,
      base_reserve,
    });
    by_id.insert(pool, c.tracked);
  }
  Ok(
    select(&infos, &base_id, min_base_reserve)
      .into_iter()
      .flatten()
      .filter_map(|info| by_id.get(&info.pool).copied())
      .collect(),
  )
}

/// Pools for the tokens listed in `pairs.tokens`, filtered by `discovery::select`.
pub async fn listed<P: Provider>(
  provider: &P,
  cfg: &NetworkConfig,
  evm: &EvmConfig,
) -> Result<Vec<Tracked>> {
  let base = parse_address("base_token", &cfg.base_token)?;
  let tokens = cfg
    .pairs
    .tokens
    .iter()
    .map(|t| parse_address("pairs.tokens", t))
    .collect::<Result<Vec<_>>>()?;
  lookup(provider, evm, base, &tokens, cfg.pairs.min_base_reserve).await
}

/// Pools for every token the smaller V2 factory pairs with the base token.
///
/// The V2 venue with the lower `allPairsLength` is scanned over its first `pairs.max_scan`
/// pairs; each token found paired with the base is then looked up like `listed` does.
pub async fn discover<P: Provider>(
  provider: &P,
  cfg: &NetworkConfig,
  evm: &EvmConfig,
) -> Result<Vec<Tracked>> {
  let base = parse_address("base_token", &cfg.base_token)?;
  let block = BlockId::latest();
  let factories: Vec<Address> = evm
    .venues
    .iter()
    .filter(|v| v.family == Family::V2)
    .map(|v| v.address)
    .collect();
  let length_calls = factories
    .iter()
    .map(|&f| read(f, &IUniswapV2Factory::allPairsLengthCall {}))
    .collect();
  let lengths = aggregate(provider, evm.multicall, block, length_calls).await?;
  let mut smallest: Option<(Address, U256)> = None;
  for (&factory, result) in factories.iter().zip(&lengths) {
    let length = decode::<IUniswapV2Factory::allPairsLengthCall>(result)?;
    if smallest.is_none_or(|(_, best)| length < best) {
      smallest = Some((factory, length));
    }
  }
  let Some((factory, length)) = smallest else {
    return Err(Error::Config(
      "pairs.mode: discover needs a v2 venue".into(),
    ));
  };
  let count = length.min(U256::from(cfg.pairs.max_scan)).to::<u64>();
  let pair_calls = (0..count)
    .map(|i| {
      read(
        factory,
        &IUniswapV2Factory::allPairsCall {
          index: U256::from(i),
        },
      )
    })
    .collect();
  let pairs: Vec<Address> = aggregate(provider, evm.multicall, block, pair_calls)
    .await?
    .iter()
    .filter_map(|r| decode::<IUniswapV2Factory::allPairsCall>(r).ok())
    .collect();
  let token_calls = pairs
    .iter()
    .flat_map(|&pair| {
      [
        read(pair, &IUniswapV2Pair::token0Call {}),
        read(pair, &IUniswapV2Pair::token1Call {}),
      ]
    })
    .collect();
  let answers = aggregate(provider, evm.multicall, block, token_calls).await?;
  let mut tokens = BTreeSet::new();
  for both in answers.chunks(2) {
    let (Ok(token0), Ok(token1)) = (
      decode::<IUniswapV2Pair::token0Call>(&both[0]),
      decode::<IUniswapV2Pair::token1Call>(&both[1]),
    ) else {
      continue;
    };
    if token0 == base {
      tokens.insert(token1);
    } else if token1 == base {
      tokens.insert(token0);
    }
  }
  let tokens: Vec<Address> = tokens.into_iter().collect();
  lookup(provider, evm, base, &tokens, cfg.pairs.min_base_reserve).await
}

#[cfg(test)]
mod tests {
  use super::*;

  use alloy::primitives::aliases::{I24, U24, U112};
  use alloy::providers::mock::Asserter;
  use alloy::sol_types::SolCall;

  use crate::testkit::{mock_evm_config, mock_provider, push_multicall};

  /// The shipped ethereum config, the source of the core settings in these tests.
  const ETHEREUM: &str = include_str!("../../../config/ethereum.toml");
  /// Square root price of tick zero.
  const ONE: U160 = U160::from_limbs([0, 1 << 32, 0]);
  /// In range liquidity of the probed V4 pool.
  const L: u128 = 10_000_000_000;

  /// Wrapped native token.
  fn weth() -> Address {
    Address::repeat_byte(0xee)
  }

  /// The listed token.
  fn token() -> Address {
    Address::repeat_byte(0x11)
  }

  /// The V2 pair of the listed token.
  fn pair() -> Address {
    Address::repeat_byte(0x22)
  }

  /// Network config listing only `token()` against `weth()`, with a small reserve floor.
  fn network() -> NetworkConfig {
    let mut cfg = NetworkConfig::parse(ETHEREUM, "ethereum").unwrap();
    cfg.base_token = weth().to_string();
    cfg.pairs.tokens = vec![token().to_string()];
    cfg.pairs.min_base_reserve = Amount::from(1000u64);
    cfg
  }

  /// Encoded slot0 at price `sqrt` with `protocol_fee`.
  fn slot0(sqrt: U160, protocol_fee: u32) -> Vec<u8> {
    IStateView::getSlot0Call::abi_encode_returns(&IStateView::getSlot0Return {
      sqrtPriceX96: sqrt,
      tick: I24::ZERO,
      protocolFee: U24::from(protocol_fee),
      lpFee: U24::from(3000),
    })
  }

  /// Every tracked pool gets a leg spec under its id with its family's fields.
  #[test]
  fn leg_specs_cover_every_pool() {
    let key = V4Key {
      currency0: Address::ZERO,
      currency1: token(),
      fee: 500,
      tick_spacing: 10,
      hooks: Address::ZERO,
    };
    let tracked = [
      Tracked::V2 {
        venue: 0,
        pair: pair(),
        token0: token(),
        token1: weth(),
        fee_bps: 30,
      },
      Tracked::V4 { venue: 1, key },
    ];
    let specs = leg_specs(&tracked);
    assert_eq!(specs.len(), 2);
    assert_eq!(
      specs[&to_id(pair())],
      LegSpec {
        family: Family::V2,
        pair: Some(pair()),
        key: None,
        token0: token(),
        fee_bps: 30,
      }
    );
    assert_eq!(
      specs[&Id::new(key.pool_id().to_vec())],
      LegSpec {
        family: Family::V4,
        pair: None,
        key: Some(key),
        token0: Address::ZERO,
        fee_bps: 5,
      }
    );
  }

  /// A listed token found on both venues is kept: the V2 pair and both priced V4 keys, the
  /// one with a protocol fee included; unpriced and reverted keys are dropped.
  #[tokio::test]
  async fn listed_finds_pools_on_two_venues() {
    let asserter = Asserter::new();
    let mut probes = vec![Some(IUniswapV2Factory::getPairCall::abi_encode_returns(
      &pair(),
    ))];
    for (fee, _) in PROBES {
      let native = match fee {
        3000 => Some(slot0(ONE, 0)),
        500 => Some(slot0(ONE, 100)),
        _ => None,
      };
      probes.push(native);
      probes.push(Some(slot0(U160::ZERO, 0)));
    }
    push_multicall(&asserter, probes);
    let reserves = IUniswapV2Pair::getReservesReturn {
      reserve0: U112::from(1u64),
      reserve1: U112::from(5000u64),
      blockTimestampLast: 0,
    };
    push_multicall(
      &asserter,
      vec![
        Some(IUniswapV2Pair::getReservesCall::abi_encode_returns(
          &reserves,
        )),
        Some(IStateView::getLiquidityCall::abi_encode_returns(&L)),
        Some(IStateView::getLiquidityCall::abi_encode_returns(&L)),
      ],
    );
    let found = listed(&mock_provider(&asserter), &network(), &mock_evm_config())
      .await
      .unwrap();
    let charging = V4Key {
      currency0: Address::ZERO,
      currency1: token(),
      fee: 500,
      tick_spacing: 10,
      hooks: Address::ZERO,
    };
    let key = V4Key {
      fee: 3000,
      tick_spacing: 60,
      ..charging
    };
    assert_eq!(
      found,
      vec![
        Tracked::V2 {
          venue: 0,
          pair: pair(),
          token0: token(),
          token1: weth(),
          fee_bps: 30,
        },
        Tracked::V4 {
          venue: 1,
          key: charging,
        },
        Tracked::V4 { venue: 1, key },
      ]
    );
    assert!(asserter.read_q().is_empty());
  }

  /// Discovery scans the factory with fewer pairs, at most `max_scan` of them, keeps the
  /// tokens paired with the base, and looks them up on every venue.
  ///
  /// The factories hold 10 and 2 pairs with `max_scan` 3, so scanning the smaller one makes
  /// exactly two `allPairs` calls and scanning the larger would make three. The mock answers
  /// two, and `aggregate` rejects an answer whose length differs from the calls sent, so the
  /// test passes only when the smaller factory was chosen.
  #[tokio::test]
  async fn discover_scans_smaller_factory() {
    let sushi = Address::repeat_byte(0x66);
    let mut evm = mock_evm_config();
    let mut second = evm.venues[0].clone();
    second.name = "sushiswap".into();
    second.address = sushi;
    evm.venues.push(second);
    let mut cfg = network();
    cfg.pairs.max_scan = 3;
    let other = Address::repeat_byte(0x23);
    let sushi_pair = Address::repeat_byte(0x24);
    let asserter = Asserter::new();
    let length = |n: u64| {
      Some(IUniswapV2Factory::allPairsLengthCall::abi_encode_returns(
        &U256::from(n),
      ))
    };
    push_multicall(&asserter, vec![length(10), length(2)]);
    let address = |a: Address| Some(IUniswapV2Factory::allPairsCall::abi_encode_returns(&a));
    push_multicall(&asserter, vec![address(pair()), address(other)]);
    let holds = |a: Address| Some(IUniswapV2Pair::token0Call::abi_encode_returns(&a));
    push_multicall(
      &asserter,
      vec![
        holds(token()),
        holds(weth()),
        holds(Address::repeat_byte(0x12)),
        holds(Address::repeat_byte(0x13)),
      ],
    );
    let mut probes = vec![Some(IUniswapV2Factory::getPairCall::abi_encode_returns(
      &pair(),
    ))];
    probes.extend(std::iter::repeat_n(None, 2 * PROBES.len()));
    probes.push(Some(IUniswapV2Factory::getPairCall::abi_encode_returns(
      &sushi_pair,
    )));
    push_multicall(&asserter, probes);
    let reserves = IUniswapV2Pair::getReservesReturn {
      reserve0: U112::from(1u64),
      reserve1: U112::from(5000u64),
      blockTimestampLast: 0,
    };
    let reserves = Some(IUniswapV2Pair::getReservesCall::abi_encode_returns(
      &reserves,
    ));
    push_multicall(&asserter, vec![reserves.clone(), reserves]);
    let found = discover(&mock_provider(&asserter), &cfg, &evm)
      .await
      .unwrap();
    let v2 = |venue, pair| Tracked::V2 {
      venue,
      pair,
      token0: token(),
      token1: weth(),
      fee_bps: 30,
    };
    assert_eq!(found, vec![v2(0, pair()), v2(2, sushi_pair)]);
    assert!(asserter.read_q().is_empty());
  }
}
