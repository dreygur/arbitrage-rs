//! Chain neutral network configuration loaded from `config/<network>.toml`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use serde::{Deserialize, Deserializer};

use crate::error::{Error, Result};
use crate::types::Amount;

/// Env var holding the node HTTP endpoint.
const RPC_URL: &str = "RPC_URL";
/// Env var holding the node websocket endpoint.
const WS_URL: &str = "WS_URL";

/// Parses a decimal string into an [`Amount`]; hex and signs are rejected.
fn amount<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Amount, D::Error> {
  let text = String::deserialize(d)?;
  if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
    return Err(serde::de::Error::custom(format!(
      "amount must be a decimal string, got {text:?}"
    )));
  }
  Amount::from_str_radix(&text, 10).map_err(serde::de::Error::custom)
}

/// Builds a config error that names the offending key.
fn invalid(key: &str, why: &str) -> Error {
  Error::Config(format!("{key}: {why}"))
}

/// Kind of chain the bot runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChainKind {
  /// An EVM chain.
  Evm,
}

/// How the pairs to watch are chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairMode {
  /// Watch the configured token list.
  List,
  /// Discover pairs from the venues.
  Discover,
}

/// Where an account's signing key comes from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum AccountSource {
  /// Several keys derived from a mnemonic held in an env var.
  Mnemonic {
    /// Env var holding the mnemonic phrase.
    env: String,
    /// Derivation indices, one account each.
    indices: Vec<u32>,
  },
  /// A single private key held in an env var.
  Key {
    /// Env var holding the private key.
    env: String,
  },
}

impl AccountSource {
  /// Name of the env var that holds this source's secret.
  fn env(&self) -> &str {
    match self {
      Self::Mnemonic { env, .. } | Self::Key { env } => env,
    }
  }

  /// Number of accounts this source yields.
  fn count(&self) -> usize {
    match self {
      Self::Mnemonic { indices, .. } => indices.len(),
      Self::Key { .. } => 1,
    }
  }
}

/// Holding limits for the executor contract and the operator accounts.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Limits {
  /// Executor balance above which profit is swept.
  #[serde(deserialize_with = "amount")]
  pub executor_max_hold: Amount,
  /// Executor balance left behind after a sweep.
  #[serde(deserialize_with = "amount")]
  pub executor_keep: Amount,
  /// Account balance above which the excess is moved out.
  #[serde(deserialize_with = "amount")]
  pub account_max_hold: Amount,
  /// Account balance left behind after moving the excess.
  #[serde(deserialize_with = "amount")]
  pub account_keep: Amount,
  /// Account balance below which it is flagged low on gas.
  #[serde(deserialize_with = "amount")]
  pub account_min_gas: Amount,
  /// Smallest transfer worth sending.
  #[serde(deserialize_with = "amount")]
  pub min_transfer: Amount,
}

/// Circuit breaker thresholds.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct BreakerConfig {
  /// Reverts within the window that open the breaker.
  pub max_reverts: u32,
  /// Window length in blocks.
  pub revert_window: u64,
  /// Blocks the breaker stays open.
  pub cooldown_blocks: u64,
  /// Gas spent on reverts within the window that opens the breaker.
  #[serde(deserialize_with = "amount")]
  pub max_revert_gas: Amount,
}

/// Settings that can change while the bot runs.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Live {
  /// Smallest profit worth trading, in base token units.
  #[serde(deserialize_with = "amount")]
  pub min_profit: Amount,
  /// Largest base token amount to borrow.
  #[serde(deserialize_with = "amount")]
  pub max_trade: Amount,
  /// Most native token one trade may spend on gas.
  #[serde(deserialize_with = "amount")]
  pub max_gas_cost: Amount,
  /// Blocks after which an unmined trade is dropped.
  pub trade_timeout: u64,
  /// Holding limits.
  pub limits: Limits,
}

/// Which pairs to watch.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PairsConfig {
  /// How pairs are chosen.
  pub mode: PairMode,
  /// Tokens paired with the base token in list mode.
  #[serde(default)]
  pub tokens: Vec<String>,
  /// Most pairs to examine in discover mode.
  pub max_scan: u64,
  /// Smallest base token reserve a discovered pool needs.
  #[serde(deserialize_with = "amount")]
  pub min_base_reserve: Amount,
}

/// Top level keys other than the live and breaker settings.
///
/// The live and breaker settings are read from the same text by their own
/// structs rather than flattened, because serde's flatten loses the key that
/// a bad value came from.
#[derive(Deserialize)]
struct Core {
  /// Chain kind.
  kind: ChainKind,
  /// Base token address.
  base_token: String,
  /// Account sources.
  accounts: Vec<AccountSource>,
  /// Owner index.
  owner: usize,
  /// Treasury address.
  treasury: String,
  /// Optional journal path.
  #[serde(default)]
  journal: PathBuf,
  /// Pair selection.
  pairs: PairsConfig,
  /// Adapter table.
  adapter: toml::Table,
}

/// Everything loaded from `config/<network>.toml`.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkConfig {
  /// Chain kind.
  pub kind: ChainKind,
  /// Base token address the bot borrows and profits in.
  pub base_token: String,
  /// Settings that can change at runtime.
  pub live: Live,
  /// Circuit breaker thresholds.
  pub breakers: BreakerConfig,
  /// Account sources, flattened into one list of accounts.
  pub accounts: Vec<AccountSource>,
  /// Index into the flattened account list of the owner.
  pub owner: usize,
  /// Address that receives swept profit.
  pub treasury: String,
  /// Trade journal path; defaults to `logs/trades-<network>.jsonl`.
  pub journal: PathBuf,
  /// Pair selection.
  pub pairs: PairsConfig,
  /// Chain specific settings, parsed by the chain adapter.
  pub adapter: toml::Table,
}

/// How a new config differs from the running one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
  /// Nothing differs.
  None,
  /// Only live settings differ and can be applied in place.
  Live,
  /// Something else differs and the bot must restart.
  Restart,
}

impl NetworkConfig {
  /// Parses config text for `network` and validates it.
  pub fn parse(text: &str, network: &str) -> Result<Self> {
    let core: Core = toml::from_str(text)?;
    let journal = if core.journal.as_os_str().is_empty() {
      PathBuf::from(format!("logs/trades-{network}.jsonl"))
    } else {
      core.journal
    };
    let cfg = Self {
      kind: core.kind,
      base_token: core.base_token,
      live: toml::from_str(text)?,
      breakers: toml::from_str(text)?,
      accounts: core.accounts,
      owner: core.owner,
      treasury: core.treasury,
      journal,
      pairs: core.pairs,
      adapter: core.adapter,
    };
    cfg.validate()?;
    Ok(cfg)
  }

  /// Reads and parses `config/<network>.toml`.
  pub fn load(network: &str) -> Result<Self> {
    let text = std::fs::read_to_string(format!("config/{network}.toml"))?;
    Self::parse(&text, network)
  }

  /// Checks cross field rules, naming the key that breaks one.
  fn validate(&self) -> Result<()> {
    let accounts: usize = self.accounts.iter().map(AccountSource::count).sum();
    if accounts == 0 {
      return Err(invalid("accounts", "at least one account is required"));
    }
    if self.owner >= accounts {
      return Err(invalid("owner", "index is outside the accounts list"));
    }
    let limits = &self.live.limits;
    if limits.executor_keep >= limits.executor_max_hold {
      return Err(invalid(
        "limits.executor_keep",
        "must be below executor_max_hold",
      ));
    }
    if limits.account_keep >= limits.account_max_hold {
      return Err(invalid(
        "limits.account_keep",
        "must be below account_max_hold",
      ));
    }
    if limits.account_keep < limits.account_min_gas {
      return Err(invalid(
        "limits.account_min_gas",
        "must not exceed account_keep",
      ));
    }
    if self.live.max_gas_cost.is_zero() {
      return Err(invalid("max_gas_cost", "must be greater than zero"));
    }
    if self.pairs.mode == PairMode::List && self.pairs.tokens.is_empty() {
      return Err(invalid(
        "pairs.tokens",
        "list mode needs at least one token",
      ));
    }
    Ok(())
  }

  /// Hashes the values of the env vars the config depends on.
  ///
  /// A missing var hashes as empty, so setting or changing one changes the
  /// fingerprint and a reload reports a restart.
  pub fn env_fingerprint(&self) -> u64 {
    let mut hasher = DefaultHasher::new();
    let names = [RPC_URL, WS_URL]
      .into_iter()
      .chain(self.accounts.iter().map(AccountSource::env));
    for name in names {
      name.hash(&mut hasher);
      std::env::var(name).unwrap_or_default().hash(&mut hasher);
    }
    hasher.finish()
  }

  /// Compares this running config and fingerprint with a freshly loaded one.
  pub fn diff(&self, old_fp: u64, new: &NetworkConfig, new_fp: u64) -> Change {
    if old_fp != new_fp {
      return Change::Restart;
    }
    let mut rest = new.clone();
    rest.live = self.live.clone();
    if rest != *self {
      return Change::Restart;
    }
    if new.live == self.live {
      Change::None
    } else {
      Change::Live
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const ETHEREUM: &str = include_str!("../config/ethereum.toml");

  /// Parses the ethereum config with an edit applied to the shipped text.
  fn edited(from: &str, to: &str) -> Result<NetworkConfig> {
    assert!(ETHEREUM.contains(from), "fixture lacks {from}");
    NetworkConfig::parse(&ETHEREUM.replace(from, to), "ethereum")
  }

  /// The shipped ethereum config parses with the documented values.
  #[test]
  fn parses_ethereum_config() {
    let cfg = NetworkConfig::parse(ETHEREUM, "ethereum").unwrap();
    assert_eq!(cfg.kind, ChainKind::Evm);
    assert_eq!(cfg.live.trade_timeout, 3);
    assert_eq!(
      cfg.live.min_profit,
      Amount::from(10u64).pow(Amount::from(15u64))
    );
    assert_eq!(cfg.breakers.cooldown_blocks, 50);
    assert_eq!(cfg.accounts.len(), 1);
    assert_eq!(cfg.owner, 0);
    assert_eq!(cfg.pairs.mode, PairMode::List);
    assert_eq!(cfg.pairs.tokens.len(), 4);
    assert_eq!(cfg.journal, PathBuf::from("logs/trades-ethereum.jsonl"));
    assert_eq!(cfg.adapter["chain_id"].as_integer(), Some(1));
  }

  /// A non decimal amount is rejected and the key is named.
  #[test]
  fn rejects_bad_amount() {
    let err = edited("min_profit = \"1000000000000000\"", "min_profit = \"0x10\"").unwrap_err();
    assert!(err.to_string().contains("min_profit"), "{err}");
  }

  /// List mode without tokens is rejected.
  #[test]
  fn list_mode_needs_tokens() {
    let (head, rest) = ETHEREUM.split_once("tokens = [").unwrap();
    let (_, tail) = rest.split_once("]\n").unwrap();
    let err = NetworkConfig::parse(&format!("{head}tokens = []\n{tail}"), "ethereum").unwrap_err();
    assert!(err.to_string().contains("pairs.tokens"), "{err}");
  }

  /// At least one account is required.
  #[test]
  fn needs_an_account() {
    let err = edited("indices = [0, 1, 2]", "indices = []").unwrap_err();
    assert!(err.to_string().contains("accounts"), "{err}");
  }

  /// The owner index must address an existing account.
  #[test]
  fn owner_index_in_range() {
    let err = edited("owner = 0", "owner = 3").unwrap_err();
    assert!(err.to_string().contains("owner"), "{err}");
    assert!(edited("owner = 0", "owner = 2").is_ok());
  }

  /// Keep amounts must sit below their caps.
  #[test]
  fn keep_must_be_below_cap() {
    let err = edited(
      "executor_keep = \"100000000000000000\"",
      "executor_keep = \"1000000000000000000\"",
    )
    .unwrap_err();
    assert!(err.to_string().contains("executor_keep"), "{err}");
    let err = edited(
      "account_keep = \"500000000000000000\"",
      "account_keep = \"2000000000000000000\"",
    )
    .unwrap_err();
    assert!(err.to_string().contains("account_keep"), "{err}");
  }

  /// The account keep must cover the minimum gas balance.
  #[test]
  fn account_keep_covers_min_gas() {
    let err = edited(
      "account_min_gas = \"100000000000000000\"",
      "account_min_gas = \"600000000000000000\"",
    )
    .unwrap_err();
    assert!(err.to_string().contains("account_min_gas"), "{err}");
  }

  /// A zero gas cost cap would block every trade and is rejected.
  #[test]
  fn max_gas_cost_must_be_positive() {
    let err = edited(
      "max_gas_cost = \"20000000000000000\"",
      "max_gas_cost = \"0\"",
    )
    .unwrap_err();
    assert!(err.to_string().contains("max_gas_cost"), "{err}");
  }

  /// Changing profit thresholds is a live change.
  #[test]
  fn diff_live_for_thresholds() {
    let old = NetworkConfig::parse(ETHEREUM, "ethereum").unwrap();
    let new = edited("trade_timeout = 3", "trade_timeout = 5").unwrap();
    assert_eq!(old.diff(1, &new, 1), Change::Live);
    assert_eq!(old.diff(1, &old, 1), Change::None);
  }

  /// Changing the gas cost cap is a live change.
  #[test]
  fn diff_live_for_max_gas_cost() {
    let old = NetworkConfig::parse(ETHEREUM, "ethereum").unwrap();
    let new = edited(
      "max_gas_cost = \"20000000000000000\"",
      "max_gas_cost = \"30000000000000000\"",
    )
    .unwrap();
    assert_eq!(old.diff(1, &new, 1), Change::Live);
  }

  /// Changing the adapter table needs a restart.
  #[test]
  fn diff_restart_for_adapter_change() {
    let old = NetworkConfig::parse(ETHEREUM, "ethereum").unwrap();
    let new = edited("gas_v2_v2 = 350000", "gas_v2_v2 = 360000").unwrap();
    assert_eq!(old.diff(1, &new, 1), Change::Restart);
  }

  /// A different env fingerprint needs a restart.
  #[test]
  fn diff_restart_for_env_change() {
    let old = NetworkConfig::parse(ETHEREUM, "ethereum").unwrap();
    assert_eq!(old.diff(1, &old, 2), Change::Restart);
    let mut cfg = old.clone();
    cfg.accounts = vec![AccountSource::Key {
      env: "ARB_TEST_KEY_FP".into(),
    }];
    let before = cfg.env_fingerprint();
    // SAFETY: single threaded use of a variable name no other test touches.
    unsafe { std::env::set_var("ARB_TEST_KEY_FP", "changed") };
    assert_ne!(before, cfg.env_fingerprint());
  }
}
