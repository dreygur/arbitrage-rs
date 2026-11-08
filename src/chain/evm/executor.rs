//! Executor lifecycle: reuse the deployed `FlashArbitrage` named in `.arbitrage-<network>.json`
//! when it still fits the config, otherwise deploy a new one from the owner and record it.

use std::path::Path;

use alloy::primitives::Address;
use alloy::providers::Provider;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::chain::evm::bindings::FlashArbitrage;
use crate::chain::evm::config::EvmConfig;
use crate::error::{Error, Result};

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
}

/// What `ensure_executor` does about the executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutorAction {
  /// Keep using the recorded contract at this address.
  Reuse(Address),
  /// Deploy a new contract.
  Deploy,
}

/// Reuses the recorded executor only when it has code and was deployed on this chain by this
/// owner for this treasury, since the treasury is immutable and only the owner can sweep.
pub fn executor_action(
  record: Option<&ExecutorRecord>,
  code_present: bool,
  owner: Address,
  treasury: Address,
  chain_id: u64,
) -> ExecutorAction {
  match record {
    Some(record)
      if code_present
        && record.owner == owner
        && record.treasury == treasury
        && record.chain_id == chain_id =>
    {
      ExecutorAction::Reuse(record.address)
    }
    _ => ExecutorAction::Deploy,
  }
}

/// Reads the record at `path`, `None` when the file does not exist.
fn read_record(path: &Path) -> Result<Option<ExecutorRecord>> {
  match std::fs::read_to_string(path) {
    Ok(text) => serde_json::from_str(&text)
      .map(Some)
      .map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(e) => Err(e.into()),
  }
}

/// Returns the executor to trade through, deploying one when the record at `record_path` is
/// missing or no longer fits, and grants `setOperator` to every operator it lacks.
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
  let code_present = match &record {
    Some(record) => !provider.get_code_at(record.address).await?.is_empty(),
    None => false,
  };
  let address = match executor_action(record.as_ref(), code_present, owner, treasury, evm.chain_id)
  {
    ExecutorAction::Reuse(address) => {
      info!(%address, "reusing executor");
      address
    }
    ExecutorAction::Deploy => {
      if let Some(record) = &record {
        warn!(old = %record.address, "recorded executor does not fit the config, deploying a new one");
      }
      let address = FlashArbitrage::deploy_builder(
        provider,
        evm.pool_manager,
        weth,
        treasury,
        operators.to_vec(),
      )
      .deploy()
      .await?;
      let record = ExecutorRecord {
        address,
        owner,
        treasury,
        chain_id: evm.chain_id,
      };
      std::fs::write(record_path, serde_json::to_string_pretty(&record)?)?;
      info!(%address, "deployed executor");
      address
    }
  };
  let contract = FlashArbitrage::new(address, provider);
  for &operator in operators {
    if contract.operators(operator).call().await? {
      continue;
    }
    let receipt = contract
      .setOperator(operator, true)
      .send()
      .await?
      .get_receipt()
      .await
      .map_err(|e| Error::Contract(format!("setOperator {operator}: {e}")))?;
    if !receipt.status() {
      return Err(Error::Contract(format!("setOperator {operator} reverted")));
    }
    info!(%operator, "granted operator");
  }
  Ok(address)
}

#[cfg(test)]
mod tests {
  use super::*;

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
    }
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

  /// A record with code behind it and the configured owner, treasury and chain is reused.
  #[test]
  fn reuse_when_code_and_owner_match() {
    let action = executor_action(Some(&record()), true, owner(), treasury(), 1);
    assert_eq!(action, ExecutorAction::Reuse(record().address));
  }

  /// Another owner, treasury or chain means a new deployment.
  #[test]
  fn deploy_when_owner_differs() {
    let r = record();
    let other = Address::repeat_byte(0x02);
    assert_eq!(
      executor_action(Some(&r), true, other, treasury(), 1),
      ExecutorAction::Deploy
    );
    assert_eq!(
      executor_action(Some(&r), true, owner(), other, 1),
      ExecutorAction::Deploy
    );
    assert_eq!(
      executor_action(Some(&r), true, owner(), treasury(), 31337),
      ExecutorAction::Deploy
    );
  }

  /// No code at the recorded address, as after a refork, or no record at all, means a new
  /// deployment.
  #[test]
  fn deploy_when_code_missing() {
    assert_eq!(
      executor_action(Some(&record()), false, owner(), treasury(), 1),
      ExecutorAction::Deploy
    );
    assert_eq!(
      executor_action(None, false, owner(), treasury(), 1),
      ExecutorAction::Deploy
    );
  }
}
