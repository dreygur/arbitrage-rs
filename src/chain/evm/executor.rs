//! Executor lifecycle: reuse the deployed `FlashArbitrage` named in `.arbitrage-<network>.json`
//! when it still fits the config, otherwise deploy a new one from the owner and record it.
//!
//! The deploy and every `setOperator` obey the fee cap the engine's transactions do: nothing
//! is sent while the fee per gas is over it, and each is signed with the engine's fee cap and
//! tip. They are not trades, so `max_gas_limit` and `max_gas_cost`, the per trade caps, do
//! not bind them: the limit is the node's estimate plus 20%, and the worst case cost is
//! logged before each is sent.

use std::collections::{BTreeSet, HashSet};
use std::future::IntoFuture;
use std::path::Path;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::Address;
use alloy::providers::Provider;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::chain::evm::bindings::FlashArbitrage;
use crate::chain::evm::config::EvmConfig;
use crate::chain::evm::gas::{fee_cap, fee_per_gas, gas_cost, next_base_fee};
use crate::error::{Error, Result};

/// Gas limit headroom over the node's estimate, as `6 / 5` (20%).
const HEADROOM: (u64, u64) = (6, 5);

/// The executor a previous run deployed, saved as JSON so the next run can find it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorRecord {
  /// Contract address.
  pub address: Address,
  /// Account that deployed it and alone may sweep.
  pub owner: Address,
  /// Fixed recipient of its sweeps.
  pub treasury: Address,
  /// Chain it was deployed on.
  pub chain_id: u64,
  /// Operators granted by the last run, so one dropped from the config can be revoked;
  /// empty in a record written before this field existed.
  #[serde(default)]
  pub operators: Vec<Address>,
}

/// The fee cap and tip an owner transaction is signed with when the next base fee is
/// `next_base_fee`, or an error while the fee per gas is over the cap.
pub fn setup_fees(next_base_fee: u128, evm: &EvmConfig) -> Result<(u128, u128)> {
  if fee_per_gas(next_base_fee, evm).is_none() {
    return Err(Error::Contract(format!(
      "fee per gas {} over the cap from max_fee_per_gas {}, executor setup not sent",
      next_base_fee.saturating_add(evm.priority_fee),
      evm.max_fee_per_gas
    )));
  }
  Ok((fee_cap(next_base_fee, evm), evm.priority_fee))
}

/// The gas limit for an owner setup transaction the node estimated at `estimate`: 20%
/// headroom and no `max_gas_limit` clamp, since that caps trades and the deploy alone needs
/// more than a trade.
pub fn setup_gas(estimate: u64) -> u64 {
  let padded = u128::from(estimate) * u128::from(HEADROOM.0) / u128::from(HEADROOM.1);
  u64::try_from(padded).unwrap_or(u64::MAX)
}

/// The `setOperator` calls that bring the contract in line with `configured`: a grant for
/// each configured operator not in `granted`, and a revoke for each `recorded` operator no
/// longer configured that still holds the role.
pub fn operator_changes(
  recorded: &[Address],
  configured: &[Address],
  granted: &HashSet<Address>,
) -> Vec<(Address, bool)> {
  let grants = configured
    .iter()
    .filter(|op| !granted.contains(*op))
    .map(|&op| (op, true));
  let revokes = recorded
    .iter()
    .filter(|op| !configured.contains(*op) && granted.contains(*op))
    .map(|&op| (op, false));
  grants.chain(revokes).collect()
}

/// The fee cap and tip for owner transactions sent now, from the latest block's base fee.
pub async fn current_fees<P: Provider>(provider: &P, evm: &EvmConfig) -> Result<(u128, u128)> {
  let block = provider
    .get_block_by_number(BlockNumberOrTag::Latest)
    .await?
    .ok_or_else(|| Error::Rpc("latest block missing".into()))?;
  let header = &block.header.inner;
  let next = next_base_fee(
    header.base_fee_per_gas.map(u128::from).unwrap_or_default(),
    header.gas_used,
    header.gas_limit,
  );
  setup_fees(next, evm)
}

/// What `ensure_executor` does about the executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutorAction {
  /// Keep using the recorded contract at this address.
  Reuse(Address),
  /// Deploy a new contract.
  Deploy,
}

/// Reuses the recorded executor only when it has code, the contract there `fits` (its own
/// owner, treasury, PoolManager and wrapped native token match the config, as read from
/// it) and the record is for this chain. The treasury is immutable and only the owner can
/// sweep, so a contract that does not fit must never get trades.
pub fn executor_action(
  record: Option<&ExecutorRecord>,
  code_present: bool,
  fits: bool,
  chain_id: u64,
) -> ExecutorAction {
  match record {
    Some(record) if code_present && fits && record.chain_id == chain_id => {
      ExecutorAction::Reuse(record.address)
    }
    _ => ExecutorAction::Deploy,
  }
}

/// The address an immutable getter returned, or `None` when the contract does not answer
/// it (a revert, no data, or data that does not decode), which marks a contract that is
/// not ours. A node failure is an error, since it proves nothing about the contract.
async fn immutable(
  call: impl IntoFuture<Output = std::result::Result<Address, alloy::contract::Error>>,
) -> Result<Option<Address>> {
  match call.await {
    Ok(address) => Ok(Some(address)),
    Err(alloy::contract::Error::TransportError(e)) if e.as_error_resp().is_none() => Err(e.into()),
    Err(_) => Ok(None),
  }
}

/// Whether the contract at `address` was built for this config: its `owner()`,
/// `treasury()`, `pm()` and `weth()` must match. After a refork the recorded address can
/// hold someone else's contract, which the record file alone cannot tell.
async fn executor_fits<P: Provider>(
  provider: &P,
  address: Address,
  owner: Address,
  treasury: Address,
  pool_manager: Address,
  weth: Address,
) -> Result<bool> {
  let contract = FlashArbitrage::new(address, provider);
  if immutable(contract.owner().call()).await? != Some(owner) {
    return Ok(false);
  }
  if immutable(contract.treasury().call()).await? != Some(treasury) {
    return Ok(false);
  }
  if immutable(contract.pm().call()).await? != Some(pool_manager) {
    return Ok(false);
  }
  Ok(immutable(contract.weth().call()).await? == Some(weth))
}

/// Decides whether to reuse the executor in `record` or deploy a new one, reading its code
/// and immutables from the chain.
async fn plan_executor<P: Provider>(
  provider: &P,
  record: Option<&ExecutorRecord>,
  evm: &EvmConfig,
  weth: Address,
  treasury: Address,
  owner: Address,
) -> Result<ExecutorAction> {
  let Some(found) = record else {
    return Ok(ExecutorAction::Deploy);
  };
  let code_present = !provider.get_code_at(found.address).await?.is_empty();
  let fits = code_present
    && executor_fits(
      provider,
      found.address,
      owner,
      treasury,
      evm.pool_manager,
      weth,
    )
    .await?;
  Ok(executor_action(record, code_present, fits, evm.chain_id))
}

/// Reads the record at `path`, `None` when the file does not exist.
pub fn read_record(path: &Path) -> Result<Option<ExecutorRecord>> {
  match std::fs::read_to_string(path) {
    Ok(text) => serde_json::from_str(&text)
      .map(Some)
      .map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(e) => Err(e.into()),
  }
}

/// Writes `record` to `path` as pretty JSON.
fn write_record(path: &Path, record: &ExecutorRecord) -> Result<()> {
  std::fs::write(path, serde_json::to_string_pretty(record)?)?;
  Ok(())
}

/// Deploys a new executor from the owner under the fee cap, logging its worst case cost
/// first, and returns its address.
async fn deploy<P: Provider>(
  provider: &P,
  evm: &EvmConfig,
  weth: Address,
  treasury: Address,
  operators: &[Address],
) -> Result<Address> {
  let (cap, tip) = current_fees(provider, evm).await?;
  let builder = FlashArbitrage::deploy_builder(
    provider,
    evm.pool_manager,
    weth,
    treasury,
    operators.to_vec(),
  );
  let gas = setup_gas(builder.estimate_gas().await?);
  info!(gas, fee_cap = cap, worst_case = %gas_cost(gas, cap), "deploying executor");
  let address = builder
    .gas(gas)
    .max_fee_per_gas(cap)
    .max_priority_fee_per_gas(tip)
    .deploy()
    .await?;
  Ok(address)
}

/// Sends each `setOperator` in `changes` from the owner under the fee cap, logging each one's
/// worst case cost first, and waits for every receipt.
async fn apply_operators<P: Provider>(
  provider: &P,
  evm: &EvmConfig,
  executor: Address,
  changes: &[(Address, bool)],
) -> Result<()> {
  if changes.is_empty() {
    return Ok(());
  }
  let (cap, tip) = current_fees(provider, evm).await?;
  let contract = FlashArbitrage::new(executor, provider);
  for &(operator, allowed) in changes {
    let call = contract.setOperator(operator, allowed);
    let gas = setup_gas(call.estimate_gas().await?);
    info!(%operator, allowed, gas, fee_cap = cap, worst_case = %gas_cost(gas, cap), "sending setOperator");
    let receipt = call
      .gas(gas)
      .max_fee_per_gas(cap)
      .max_priority_fee_per_gas(tip)
      .send()
      .await?
      .get_receipt()
      .await
      .map_err(|e| Error::Contract(format!("setOperator {operator}: {e}")))?;
    if !receipt.status() {
      return Err(Error::Contract(format!("setOperator {operator} reverted")));
    }
    info!(%operator, allowed, "operator updated");
  }
  Ok(())
}

/// Returns the executor to trade through, deploying one when the record at `record_path` is
/// missing or the contract it names no longer fits. A reused executor gets `setOperator`
/// for every configured operator it lacks and a revoke for every recorded operator no
/// longer configured; the record then lists the configured operators.
///
/// `provider` must send from `owner`, which becomes the owner of a new deployment. A corrupt
/// record is an error rather than a redeploy, so an executor still holding profit is not
/// silently forgotten.
pub async fn ensure_executor<P: Provider>(
  provider: &P,
  record_path: &Path,
  evm: &EvmConfig,
  weth: Address,
  treasury: Address,
  owner: Address,
  operators: &[Address],
) -> Result<Address> {
  let record = read_record(record_path)?;
  let action = plan_executor(provider, record.as_ref(), evm, weth, treasury, owner).await?;
  let fresh = |address| ExecutorRecord {
    address,
    owner,
    treasury,
    chain_id: evm.chain_id,
    operators: operators.to_vec(),
  };
  match action {
    ExecutorAction::Reuse(address) => {
      info!(%address, "reusing executor");
      let recorded = record.map(|r| r.operators).unwrap_or_default();
      let contract = FlashArbitrage::new(address, provider);
      let mut granted = HashSet::new();
      let known: BTreeSet<Address> = operators.iter().chain(&recorded).copied().collect();
      for operator in known {
        if contract.operators(operator).call().await? {
          granted.insert(operator);
        }
      }
      let changes = operator_changes(&recorded, operators, &granted);
      apply_operators(provider, evm, address, &changes).await?;
      if recorded != operators {
        write_record(record_path, &fresh(address))?;
      }
      Ok(address)
    }
    ExecutorAction::Deploy => {
      if let Some(record) = &record {
        warn!(old = %record.address, "recorded executor does not fit the config, deploying a new one");
      }
      let address = deploy(provider, evm, weth, treasury, operators).await?;
      write_record(record_path, &fresh(address))?;
      info!(%address, "deployed executor");
      Ok(address)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use alloy::primitives::Bytes;
  use alloy::providers::mock::Asserter;
  use alloy::sol_types::SolCall;

  use crate::testkit::{mock_evm_config, mock_provider};

  /// One gwei in wei.
  const GWEI: u128 = 1_000_000_000;

  /// The configured owner.
  fn owner() -> Address {
    Address::repeat_byte(0x01)
  }

  /// The configured treasury.
  fn treasury() -> Address {
    Address::repeat_byte(0x09)
  }

  /// A record matching the configured owner and treasury on chain 1.
  fn record() -> ExecutorRecord {
    ExecutorRecord {
      address: Address::repeat_byte(0xbb),
      owner: owner(),
      treasury: treasury(),
      chain_id: 1,
      operators: vec![operator(2)],
    }
  }

  /// The configured wrapped native token.
  fn weth() -> Address {
    Address::repeat_byte(0xee)
  }

  /// Operator `n`.
  fn operator(n: u8) -> Address {
    Address::repeat_byte(n)
  }

  /// Over the fee cap nothing is sent; under it the engine's fee cap and the tip are used.
  #[test]
  fn setup_fees_obey_cap() {
    let evm = mock_evm_config();
    assert!(matches!(
      setup_fees(45 * GWEI, &evm),
      Err(Error::Contract(_))
    ));
    assert_eq!(
      setup_fees(10 * GWEI, &evm).unwrap(),
      (fee_cap(10 * GWEI, &evm), evm.priority_fee)
    );
  }

  /// The setup gas limit is the estimate plus 20%, and the executor deploy's 1.37M estimate
  /// gets its limit even though it is above the 1M per trade `max_gas_limit`.
  #[test]
  fn deploy_gas_not_bound_by_max_gas_limit() {
    let evm = mock_evm_config();
    assert_eq!(setup_gas(100_000), 120_000);
    let deploy = setup_gas(1_370_687);
    assert_eq!(deploy, 1_644_824);
    assert!(deploy > evm.max_gas_limit);
  }

  /// New operators are granted, dropped ones still holding the role are revoked, and
  /// operators already in place are left alone.
  #[test]
  fn operator_changes_grant_and_revoke() {
    let granted = HashSet::from([operator(2), operator(3), operator(4)]);
    let recorded = [operator(2), operator(3), operator(5)];
    let configured = [operator(2), operator(6)];
    assert_eq!(
      operator_changes(&recorded, &configured, &granted),
      vec![(operator(6), true), (operator(3), false)]
    );
    let in_place = HashSet::from(configured);
    assert!(operator_changes(&configured, &configured, &in_place).is_empty());
  }

  /// Answers the code read and the four immutable reads of an executor whose owner is
  /// `owner` and whose other immutables match the config.
  fn mock_immutables(asserter: &Asserter, owner: Address) {
    asserter.push_success(&Bytes::from(vec![0x60, 0x80]));
    asserter.push_success(&Bytes::from(FlashArbitrage::ownerCall::abi_encode_returns(
      &owner,
    )));
    asserter.push_success(&Bytes::from(
      FlashArbitrage::treasuryCall::abi_encode_returns(&treasury()),
    ));
    asserter.push_success(&Bytes::from(FlashArbitrage::pmCall::abi_encode_returns(
      &mock_evm_config().pool_manager,
    )));
    asserter.push_success(&Bytes::from(FlashArbitrage::wethCall::abi_encode_returns(
      &weth(),
    )));
  }

  /// A reused executor whose operators all match sends no transaction and keeps its record.
  #[tokio::test]
  async fn reuse_with_operators_in_place_sends_nothing() {
    let dir = std::env::temp_dir().join(format!("executor-reuse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".arbitrage-test.json");
    write_record(&path, &record()).unwrap();
    let asserter = Asserter::new();
    mock_immutables(&asserter, owner());
    asserter.push_success(&Bytes::from(
      FlashArbitrage::operatorsCall::abi_encode_returns(&true),
    ));
    let address = ensure_executor(
      &mock_provider(&asserter),
      &path,
      &mock_evm_config(),
      weth(),
      treasury(),
      owner(),
      &[operator(2)],
    )
    .await
    .unwrap();
    assert_eq!(address, record().address);
    assert!(asserter.read_q().is_empty(), "unexpected calls");
    assert_eq!(read_record(&path).unwrap(), Some(record()));
  }

  /// A record survives a trip through its JSON file.
  #[test]
  fn executor_record_roundtrip() {
    let dir = std::env::temp_dir().join(format!("executor-record-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".arbitrage-test.json");
    std::fs::write(&path, serde_json::to_string_pretty(&record()).unwrap()).unwrap();
    assert_eq!(read_record(&path).unwrap(), Some(record()));
    assert_eq!(read_record(&dir.join("missing.json")).unwrap(), None);
    std::fs::write(&path, "{not json").unwrap();
    assert!(matches!(read_record(&path), Err(Error::Config(_))));
  }

  /// A record with code behind it that fits the config, on this chain, is reused.
  #[test]
  fn reuse_when_code_and_contract_fit() {
    let action = executor_action(Some(&record()), true, true, 1);
    assert_eq!(action, ExecutorAction::Reuse(record().address));
  }

  /// A contract that does not fit, or a record for another chain, means a new deployment.
  #[test]
  fn deploy_when_contract_differs() {
    let r = record();
    assert_eq!(
      executor_action(Some(&r), true, false, 1),
      ExecutorAction::Deploy
    );
    assert_eq!(
      executor_action(Some(&r), true, true, 31337),
      ExecutorAction::Deploy
    );
  }

  /// No code at the recorded address, as after a refork, or no record at all, means a new
  /// deployment.
  #[test]
  fn deploy_when_code_missing() {
    assert_eq!(
      executor_action(Some(&record()), false, false, 1),
      ExecutorAction::Deploy
    );
    assert_eq!(
      executor_action(None, false, false, 1),
      ExecutorAction::Deploy
    );
  }

  /// The record names our owner and treasury, but the contract at its address, read from
  /// the chain, has another owner, as when a refork put a stranger's contract there: a new
  /// executor is deployed and nothing more is asked of that contract.
  #[tokio::test]
  async fn deploy_when_contract_owner_differs() {
    let asserter = Asserter::new();
    mock_immutables(&asserter, Address::repeat_byte(0x02));
    let action = plan_executor(
      &mock_provider(&asserter),
      Some(&record()),
      &mock_evm_config(),
      weth(),
      treasury(),
      owner(),
    )
    .await
    .unwrap();
    assert_eq!(action, ExecutorAction::Deploy);
    assert_eq!(asserter.read_q().len(), 3, "read past the owner");
  }

  /// A contract whose immutables all match the config is reused.
  #[tokio::test]
  async fn reuse_when_contract_immutables_match() {
    let asserter = Asserter::new();
    mock_immutables(&asserter, owner());
    let action = plan_executor(
      &mock_provider(&asserter),
      Some(&record()),
      &mock_evm_config(),
      weth(),
      treasury(),
      owner(),
    )
    .await
    .unwrap();
    assert_eq!(action, ExecutorAction::Reuse(record().address));
    assert!(asserter.read_q().is_empty(), "immutables left unread");
  }
}
