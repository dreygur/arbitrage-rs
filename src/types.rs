//! Chain neutral value types shared by the core, the chains and the dashboard.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Token amount in the smallest unit of its token.
pub type Amount = ruint::aliases::U256;

/// Serde helper that writes an [`Amount`] as a decimal string.
pub mod amount_str {
  use super::*;

  /// Serializes the amount as a decimal string.
  pub fn serialize<S: Serializer>(value: &Amount, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(value)
  }

  /// Parses a decimal string into an amount.
  pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Amount, D::Error> {
    let text = String::deserialize(d)?;
    text.parse().map_err(serde::de::Error::custom)
  }
}

/// Serde helper that writes an optional [`Amount`] as a decimal string or null.
pub mod amount_opt {
  use super::*;

  /// Serializes the amount as a decimal string, or null when absent.
  pub fn serialize<S: Serializer>(value: &Option<Amount>, s: S) -> Result<S::Ok, S::Error> {
    match value {
      Some(v) => s.collect_str(v),
      None => s.serialize_none(),
    }
  }

  /// Parses an optional decimal string into an amount.
  pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Amount>, D::Error> {
    match Option::<String>::deserialize(d)? {
      Some(text) => text.parse().map(Some).map_err(serde::de::Error::custom),
      None => Ok(None),
    }
  }
}

/// Opaque chain neutral identifier (a token or pool key), kept as raw bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(Vec<u8>);

impl Id {
  /// Wraps raw bytes.
  pub fn new(bytes: Vec<u8>) -> Self {
    Self(bytes)
  }

  /// Returns the raw bytes.
  pub fn as_bytes(&self) -> &[u8] {
    &self.0
  }
}

impl fmt::Display for Id {
  /// Writes the bytes as `0x` prefixed lowercase hex.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "0x")?;
    for byte in &self.0 {
      write!(f, "{byte:02x}")?;
    }
    Ok(())
  }
}

impl Serialize for Id {
  /// Writes the id as its `0x` hex string.
  fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(self)
  }
}

impl<'de> Deserialize<'de> for Id {
  /// Reads an id from a `0x` hex string.
  fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    let text = String::deserialize(d)?;
    let hex = text
      .strip_prefix("0x")
      .ok_or_else(|| serde::de::Error::custom("id must start with 0x"))?;
    if hex.len() % 2 != 0 || !hex.is_ascii() {
      return Err(serde::de::Error::custom("id must be even length hex"));
    }
    let bytes = (0..hex.len())
      .step_by(2)
      .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
      .collect::<Result<Vec<u8>, _>>()
      .map_err(serde::de::Error::custom)?;
    Ok(Self(bytes))
  }
}

/// Swap direction within a pool, relative to its token order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
  /// Token zero in, token one out.
  ZeroForOne,
  /// Token one in, token zero out.
  OneForZero,
}

impl Side {
  /// Returns the opposite direction.
  pub fn flip(self) -> Self {
    match self {
      Self::ZeroForOne => Self::OneForZero,
      Self::OneForZero => Self::ZeroForOne,
    }
  }
}

/// A profitable round trip: buy on one pool, sell on the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opportunity {
  /// Pool where the non base token is bought.
  pub buy_pool: Id,
  /// Pool where the non base token is sold.
  pub sell_pool: Id,
  /// The non base token of the pair.
  pub token: Id,
  /// Base token amount to borrow.
  pub amount: Amount,
  /// Expected profit in base token units.
  pub profit: Amount,
  /// Minimum acceptable return, below which the trade reverts.
  pub floor: Amount,
}

/// Outcome of evaluating an opportunity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// The trade was sent.
  Sent,
  /// Profit is below the configured minimum.
  BelowMin,
  /// Gas cost would exceed the profit.
  GasExceedsProfit,
  /// Fee per gas or gas cost is above the configured cap.
  GasCap,
  /// Quoting failed.
  QuoteError,
  /// The pair is locked by an in flight trade until the given block.
  Locked(u64),
  /// The sending account is low on gas funds.
  LowGas,
  /// A circuit breaker is open.
  CircuitOpen,
  /// Trading is paused.
  Paused,
}

/// State change reported by a chain feed for one block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Update {
  /// Pools whose state changed in this block.
  pub touched: HashSet<Id>,
  /// Block number of the update.
  pub block: u64,
  /// Whether pool state must be reloaded from scratch.
  pub reload: bool,
  /// Whether the feed must be restarted.
  pub restart: bool,
}

/// Lifecycle stage of a trade.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeState {
  /// Decided but not yet sent.
  Planned,
  /// Submitted to the node.
  Sent,
  /// Mined successfully.
  Executed,
  /// Mined and reverted.
  Reverted,
  /// Abandoned without being mined.
  Dropped,
}

/// Operator command sent to the running bot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
  /// Stop opening new trades.
  Pause,
  /// Resume opening trades.
  Resume,
  /// Move profits to the treasury.
  Sweep,
}

/// What kind of transaction a ledger entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
  /// An arbitrage trade.
  Trade,
  /// A replacement transaction that cancels a stuck one.
  Cancel,
  /// A profit sweep to the treasury.
  Sweep,
  /// A gas top up forwarded to an operator.
  Forward,
}

/// Optional details of a trade, filled in as it progresses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeDetail {
  /// Base and non base token of the pair.
  pub pair: Option<(Id, Id)>,
  /// Pool where the non base token is bought.
  pub buy_pool: Option<Id>,
  /// Pool where the non base token is sold.
  pub sell_pool: Option<Id>,
  /// Human readable description of the buy pool.
  pub buy_describe: Option<String>,
  /// Human readable description of the sell pool.
  pub sell_describe: Option<String>,
  /// Borrowed base token amount.
  #[serde(with = "amount_opt")]
  pub amount: Option<Amount>,
  /// Profit quoted at decision time.
  #[serde(with = "amount_opt")]
  pub quoted_profit: Option<Amount>,
  /// Minimum acceptable return.
  #[serde(with = "amount_opt")]
  pub floor: Option<Amount>,
  /// Gas limit set on the transaction.
  pub gas_limit: Option<u64>,
  /// Fee cap per gas.
  pub fee_cap: Option<u128>,
  /// Priority fee per gas.
  pub tip: Option<u128>,
  /// Block the transaction was mined in.
  pub mined_block: Option<u64>,
  /// Gas the transaction used.
  pub gas_used: Option<u64>,
  /// Price per gas actually paid.
  pub effective_gas_price: Option<u128>,
  /// Profit realised on chain.
  #[serde(with = "amount_opt")]
  pub actual_profit: Option<Amount>,
  /// Revert reason when the transaction failed.
  pub revert: Option<String>,
  /// Stage latencies in microseconds.
  pub latency_us: Option<[u64; 4]>,
}

/// One ledger entry describing a transaction at a point in its lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeEvent {
  /// Ledger sequence number.
  pub id: u64,
  /// What the transaction is.
  pub kind: EntryKind,
  /// Current lifecycle stage.
  pub state: TradeState,
  /// Block at which this state was recorded.
  pub block: u64,
  /// Sending account address.
  pub account: String,
  /// Account nonce used.
  pub nonce: u64,
  /// Transaction hash once sent.
  pub tx: Option<String>,
  /// Optional trade details.
  pub detail: TradeDetail,
}

/// Role of an account in the executor contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
  /// Owns the executor and may sweep and fund.
  Owner,
  /// Sends trades.
  Operator,
}

/// Snapshot of one account for the dashboard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountStatus {
  /// Account address.
  pub address: String,
  /// Account role.
  pub role: Role,
  /// Native balance.
  #[serde(with = "amount_str")]
  pub eth: Amount,
  /// Next nonce.
  pub nonce: u64,
  /// Transactions currently in flight.
  pub in_flight: usize,
  /// Whether the balance is below the gas floor.
  pub low_gas: bool,
}

/// Snapshot of the whole bot for the dashboard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
  /// Latest block seen.
  pub block: u64,
  /// Predicted base fee of the next block.
  pub next_base_fee: u128,
  /// Fee cap currently applied to trades.
  pub fee_cap: u128,
  /// Configured ceiling on fee per gas.
  pub max_fee_per_gas: u128,
  /// Delay of the feed behind the chain head, in milliseconds.
  pub feed_lag_ms: u64,
  /// Executor contract address.
  pub executor: String,
  /// Executor balance in the base token.
  #[serde(with = "amount_str")]
  pub executor_balance: Amount,
  /// Treasury address.
  pub treasury: String,
  /// Treasury balance in the base token.
  #[serde(with = "amount_str")]
  pub treasury_balance: Amount,
  /// Per account snapshots.
  pub accounts: Vec<AccountStatus>,
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Ids print as `0x` prefixed lowercase hex.
  #[test]
  fn id_displays_as_hex() {
    assert_eq!(Id::new(vec![0xab, 0x01]).to_string(), "0xab01");
  }

  /// Flipping twice returns the original side.
  #[test]
  fn side_flips() {
    assert_eq!(Side::ZeroForOne.flip(), Side::OneForZero);
    assert_eq!(Side::OneForZero.flip().flip(), Side::OneForZero);
  }

  /// Ids and amounts survive a JSON round trip in their string forms.
  #[test]
  fn serde_round_trip() {
    let id = Id::new(vec![0xab, 0x01]);
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"0xab01\"");
    assert_eq!(serde_json::from_str::<Id>("\"0xab01\"").unwrap(), id);
    let detail = TradeDetail {
      amount: Some(Amount::from(10u64).pow(Amount::from(30u64))),
      ..Default::default()
    };
    let json = serde_json::to_string(&detail).unwrap();
    assert!(json.contains("\"1000000000000000000000000000000\""));
    assert_eq!(serde_json::from_str::<TradeDetail>(&json).unwrap(), detail);
  }
}
