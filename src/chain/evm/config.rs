//! EVM settings parsed from the `[adapter]` table of a network config.

use alloy::primitives::Address;
use serde::{Deserialize, Deserializer};

use crate::config::NetworkConfig;
use crate::error::{Error, Result};
use crate::types::{Amount, amount_str};

/// Parses a decimal string into a `u128`.
fn decimal_u128<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u128, D::Error> {
  let text = String::deserialize(d)?;
  text.parse().map_err(serde::de::Error::custom)
}

/// Pool family a venue belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
  /// Constant product pairs from a V2 factory.
  V2,
  /// Singleton PoolManager pools.
  V4,
}

/// One exchange the adapter watches.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct EvmVenue {
  /// Name shown in logs and descriptions.
  pub name: String,
  /// Pool family.
  pub family: Family,
  /// Factory (V2) or PoolManager (V4) address.
  pub address: Address,
  /// Swap fee in basis points; required for V2.
  #[serde(default)]
  pub fee_bps: Option<u16>,
  /// Bitmap words read around the current tick; required for V4.
  #[serde(default)]
  pub tick_words: Option<u16>,
}

/// A token to fund the test accounts with on a fork.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ForkToken {
  /// Token address.
  pub address: Address,
  /// Amount to move to the executor owner.
  #[serde(with = "amount_str")]
  pub amount: Amount,
  /// Holder impersonated to pay for it.
  pub whale: Address,
}

/// Funding applied to accounts when running on a fork.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ForkSection {
  /// Native balance given to each account.
  #[serde(with = "amount_str")]
  pub fund_eth: Amount,
  /// Tokens to transfer from whales.
  #[serde(default)]
  pub tokens: Vec<ForkToken>,
}

/// Everything the EVM adapter needs beyond the chain neutral config.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct EvmConfig {
  /// Expected chain id.
  pub chain_id: u64,
  /// Uniswap V4 PoolManager.
  pub pool_manager: Address,
  /// Uniswap V4 StateView.
  pub state_view: Address,
  /// Multicall3 address.
  pub multicall: Address,
  /// Priority fee per gas in wei.
  #[serde(deserialize_with = "decimal_u128")]
  pub priority_fee: u128,
  /// Ceiling on the fee per gas in wei.
  #[serde(deserialize_with = "decimal_u128")]
  pub max_fee_per_gas: u128,
  /// Starting gas limit for a V2 to V2 trade.
  pub gas_v2_v2: u64,
  /// Starting gas limit for a V2 to V4 trade.
  pub gas_v2_v4: u64,
  /// Starting gas limit for a V4 to V4 trade.
  pub gas_v4_v4: u64,
  /// Ceiling on any gas limit.
  pub max_gas_limit: u64,
  /// Venues to watch.
  pub venues: Vec<EvmVenue>,
  /// Fork funding, absent on a live network.
  #[serde(default)]
  pub fork: Option<ForkSection>,
}

impl EvmConfig {
  /// Parses and validates the adapter table of `cfg`.
  pub fn parse(cfg: &NetworkConfig) -> Result<Self> {
    let evm: Self = cfg
      .adapter
      .clone()
      .try_into()
      .map_err(|e: toml::de::Error| Error::Config(format!("adapter: {e}")))?;
    evm.validate()?;
    Ok(evm)
  }

  /// Checks cross field rules, naming the key that breaks one.
  fn validate(&self) -> Result<()> {
    if self.priority_fee > self.max_fee_per_gas {
      return Err(Error::Config(
        "adapter.priority_fee: must not exceed max_fee_per_gas".into(),
      ));
    }
    for venue in &self.venues {
      match venue.family {
        Family::V2 if venue.fee_bps.is_none() => {
          return Err(Error::Config(format!(
            "adapter.venues.{}.fee_bps: required for a v2 venue",
            venue.name
          )));
        }
        Family::V4 if venue.tick_words.is_none() => {
          return Err(Error::Config(format!(
            "adapter.venues.{}.tick_words: required for a v4 venue",
            venue.name
          )));
        }
        _ => {}
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const ETHEREUM: &str = include_str!("../../../config/ethereum.toml");

  /// Parses the ethereum config with an edit applied to the shipped text.
  fn edited(from: &str, to: &str) -> Result<EvmConfig> {
    assert!(ETHEREUM.contains(from), "fixture lacks {from}");
    let cfg = NetworkConfig::parse(&ETHEREUM.replace(from, to), "ethereum")?;
    EvmConfig::parse(&cfg)
  }

  /// The shipped ethereum adapter section parses with its documented values.
  #[test]
  fn evm_config_parses_ethereum() {
    let cfg = NetworkConfig::parse(ETHEREUM, "ethereum").unwrap();
    let evm = EvmConfig::parse(&cfg).unwrap();
    assert_eq!(evm.chain_id, 1);
    assert_eq!(evm.priority_fee, 1_000_000_000);
    assert_eq!(evm.max_fee_per_gas, 50_000_000_000);
    assert_eq!(evm.gas_v4_v4, 350_000);
    assert_eq!(evm.max_gas_limit, 1_000_000);
    assert_eq!(evm.venues.len(), 3);
    assert_eq!(evm.venues[0].family, Family::V4);
    assert_eq!(evm.venues[0].tick_words, Some(4));
    assert_eq!(evm.venues[1].fee_bps, Some(30));
    let fork = evm.fork.unwrap();
    assert_eq!(fork.tokens.len(), 4);
    assert_eq!(fork.fund_eth, Amount::from(10u64).pow(Amount::from(18u64)));
  }

  /// A V2 venue without a fee is rejected.
  #[test]
  fn v2_venue_needs_fee() {
    let err = edited("fee_bps = 30\n\n[[adapter.venues]]", "\n[[adapter.venues]]").unwrap_err();
    assert!(err.to_string().contains("fee_bps"), "{err}");
  }

  /// A priority fee above the fee ceiling is rejected.
  #[test]
  fn priority_fee_above_max_fee_rejected() {
    let err = edited(
      "priority_fee = \"1000000000\"",
      "priority_fee = \"60000000000\"",
    )
    .unwrap_err();
    assert!(err.to_string().contains("priority_fee"), "{err}");
  }
}
